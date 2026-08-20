use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use icloud_tracking::{TrackingError, Zone, ZoneSet};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const CURRENT_CONFIG_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub version: u32,
    pub accounts: Vec<AccountConfig>,
    pub tracking: TrackingConfig,
    pub zones: Vec<Zone>,
    pub base_zone_id: Option<String>,
    pub tracked_from_zones: Vec<String>,
    pub waze: Option<WazeConfig>,
    pub external_sources: Vec<ExternalSourceConfig>,
    pub away_time_zones: Vec<AwayTimeZoneConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            accounts: Vec::new(),
            tracking: TrackingConfig::default(),
            zones: Vec::new(),
            base_zone_id: None,
            tracked_from_zones: Vec::new(),
            waze: None,
            external_sources: Vec::new(),
            away_time_zones: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Loads, migrates, and validates TOML configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, malformed TOML, future versions, or invalid
    /// accounts, zones, tracking bounds, and adapter settings.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&source)?;
        config.migrate()?;
        config.validate()?;
        Ok(config)
    }

    /// Atomically saves validated configuration with owner-only file access on
    /// Unix systems.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, serialization, or I/O.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let parent = path
            .parent()
            .ok_or_else(|| ConfigError::Invalid("configuration path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        set_directory_permissions(parent)?;
        let temporary = path.with_extension("toml.tmp");
        {
            let mut writer = BufWriter::new(create_private_file(&temporary)?);
            writer.write_all(toml::to_string_pretty(self)?.as_bytes())?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        fs::rename(temporary, path)?;
        Ok(())
    }

    /// Validates configuration without contacting any external service.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, duplicate or empty accounts,
    /// invalid intervals, zones, Waze bounds, or external-source settings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CURRENT_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }
        validate_accounts(&self.accounts)?;
        validate_tracking(&self.tracking)?;
        ZoneSet::new(self.zones.clone()).map_err(ConfigError::Zone)?;
        let active_zone_ids = self
            .zones
            .iter()
            .filter(|zone| !zone.passive)
            .map(|zone| zone.id.as_str())
            .collect::<HashSet<_>>();
        if self
            .base_zone_id
            .as_deref()
            .is_some_and(|zone_id| !active_zone_ids.contains(zone_id))
        {
            return Err(ConfigError::Invalid(
                "base_zone_id must reference an active configured zone".into(),
            ));
        }
        let mut tracked_zones = HashSet::new();
        if self.tracked_from_zones.iter().any(|zone_id| {
            zone_id.trim().is_empty()
                || !active_zone_ids.contains(zone_id.as_str())
                || !tracked_zones.insert(zone_id)
        }) {
            return Err(ConfigError::Invalid(
                "tracked_from_zones must contain unique active configured zone IDs".into(),
            ));
        }
        if let Some(waze) = &self.waze {
            if !waze.minimum_distance_km.is_finite()
                || !waze.maximum_distance_km.is_finite()
                || waze.minimum_distance_km < 0.0
                || waze.maximum_distance_km < waze.minimum_distance_km
                || !matches!(
                    waze.region.trim().to_ascii_lowercase().as_str(),
                    "us" | "ca" | "na" | "am" | "il" | "eu" | "au" | "row" | "rest_of_world"
                )
            {
                return Err(ConfigError::Invalid(
                    "Waze region and distance bounds are invalid".into(),
                ));
            }
        }
        let mut sources = HashSet::new();
        for source in &self.external_sources {
            if source.id.trim().is_empty()
                || source.id.trim() != source.id
                || !sources.insert(source.id.clone())
                || source.request_throttle_seconds == 0
                || source.alive_interval_seconds == 0
            {
                return Err(ConfigError::Invalid(
                    "external source IDs and timing settings are invalid".into(),
                ));
            }
        }
        let mut away_devices = HashSet::new();
        for away_zone in &self.away_time_zones {
            if !(-23..=23).contains(&away_zone.offset_hours)
                || away_zone.device_ids.is_empty()
                || away_zone.device_ids.iter().any(|device_id| {
                    device_id.trim().is_empty() || !away_devices.insert(device_id.clone())
                })
            {
                return Err(ConfigError::Invalid(
                    "away-time-zone offsets must be within -23..=23 and device IDs must be non-empty and unique".into(),
                ));
            }
        }
        Ok(())
    }

    fn migrate(&mut self) -> Result<(), ConfigError> {
        match self.version {
            0 => {
                self.version = CURRENT_CONFIG_VERSION;
                Ok(())
            }
            CURRENT_CONFIG_VERSION => Ok(()),
            version => Err(ConfigError::UnsupportedVersion(version)),
        }
    }
}

fn validate_accounts(accounts: &[AccountConfig]) -> Result<(), ConfigError> {
    let mut usernames = HashSet::new();
    let mut configured_devices = HashSet::new();
    for account in accounts {
        let username = account.username.trim().to_ascii_lowercase();
        if username.is_empty() || !usernames.insert(username) {
            return Err(ConfigError::Invalid(
                "Apple account usernames must be non-empty and unique".into(),
            ));
        }
        let mut account_devices = HashSet::new();
        if account.device_ids.iter().any(|device_id| {
            device_id.trim().is_empty()
                || !account_devices.insert(device_id)
                || !configured_devices.insert(device_id)
        }) {
            return Err(ConfigError::Invalid(
                "configured device IDs must be non-empty and assigned to one account".into(),
            ));
        }
    }
    Ok(())
}

fn validate_tracking(tracking: &TrackingConfig) -> Result<(), ConfigError> {
    if tracking.tick_seconds == 0
        || tracking.prefetch_seconds == 0
        || tracking.default_interval_seconds < 5
        || tracking.in_zone_interval_seconds < 5
        || tracking.stationary_interval_seconds < 5
        || tracking.exit_zone_interval_seconds < 5
        || tracking.maximum_interval_seconds < tracking.default_interval_seconds
        || !tracking.gps_accuracy_threshold_meters.is_finite()
        || tracking.gps_accuracy_threshold_meters < 0.0
        || !tracking.travel_time_factor.is_finite()
        || tracking.travel_time_factor <= 0.0
        || tracking.stationary_still_seconds == 0
        || !tracking.stationary_radius_meters.is_finite()
        || tracking.stationary_radius_meters <= 0.0
        || (tracking.fixed_interval_seconds > 0 && tracking.fixed_interval_seconds < 300)
    {
        return Err(ConfigError::Invalid(
            "tracking intervals and GPS thresholds are invalid".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    pub username: String,
    pub region: AppleRegion,
    pub session_root: Option<PathBuf>,
    pub device_ids: Vec<String>,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            username: String::new(),
            region: AppleRegion::Global,
            session_root: None,
            device_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppleRegion {
    #[default]
    Global,
    ChinaGcj02,
    ChinaBd09,
    ChinaWgs84,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrackingConfig {
    pub tick_seconds: u64,
    pub prefetch_seconds: u64,
    pub default_interval_seconds: u64,
    pub maximum_interval_seconds: u64,
    pub gps_accuracy_threshold_meters: f64,
    pub old_location_adjustment_seconds: i64,
    pub old_location_maximum_seconds: u64,
    pub pass_through_delay_seconds: u64,
    pub stationary_enabled: bool,
    pub stationary_still_seconds: u64,
    pub stationary_radius_meters: f64,
    pub in_zone_interval_seconds: u64,
    pub stationary_interval_seconds: u64,
    pub exit_zone_interval_seconds: u64,
    pub fixed_interval_seconds: u64,
    pub travel_time_factor: f64,
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            tick_seconds: 5,
            prefetch_seconds: 15,
            default_interval_seconds: 60,
            maximum_interval_seconds: 7_200,
            gps_accuracy_threshold_meters: 100.0,
            old_location_adjustment_seconds: 0,
            old_location_maximum_seconds: 0,
            pass_through_delay_seconds: 60,
            stationary_enabled: true,
            stationary_still_seconds: 1_800,
            stationary_radius_meters: 100.0,
            in_zone_interval_seconds: 120,
            stationary_interval_seconds: 300,
            exit_zone_interval_seconds: 30,
            fixed_interval_seconds: 0,
            travel_time_factor: 0.5,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WazeConfig {
    pub region: String,
    pub real_time: bool,
    pub minimum_distance_km: f64,
    pub maximum_distance_km: f64,
    pub history_database: Option<PathBuf>,
}

impl Default for WazeConfig {
    fn default() -> Self {
        Self {
            region: "eu".into(),
            real_time: true,
            minimum_distance_km: 1.0,
            maximum_distance_km: 100.0,
            history_database: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalSourceConfig {
    pub id: String,
    pub alive_interval_seconds: u64,
    pub request_throttle_seconds: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AwayTimeZoneConfig {
    pub offset_hours: i32,
    pub device_ids: Vec<String>,
}

impl Default for ExternalSourceConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            alive_interval_seconds: 1_200,
            request_throttle_seconds: 60,
        }
    }
}

/// Returns the platform-appropriate default configuration path.
///
/// # Errors
///
/// Returns an error on platforms without an application configuration directory.
pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    ProjectDirs::from("io", "icloud-location", "icloud-location")
        .map(|directories| directories.config_dir().join("config.toml"))
        .ok_or(ConfigError::NoConfigurationDirectory)
}

fn create_private_file(path: &Path) -> Result<File, ConfigError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_file_permissions(path)?;
    Ok(file)
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("this platform has no application configuration directory")]
    NoConfigurationDirectory,
    #[error("unsupported configuration version: {0}")]
    UnsupportedVersion(u32),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("invalid zone configuration: {0}")]
    Zone(TrackingError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),
    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn loads_defaults_and_migrates_version_zero() {
        let config: AppConfig = toml::from_str(
            r#"
                version = 0
                [[accounts]]
                username = "example@example.invalid"
            "#,
        )
        .unwrap();
        let mut config = config;

        config.migrate().unwrap();
        config.validate().unwrap();

        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.tracking.tick_seconds, 5);
        assert_eq!(config.accounts[0].region, AppleRegion::Global);
    }

    #[test]
    fn rejects_duplicate_accounts_and_invalid_zones() {
        let mut config = AppConfig {
            accounts: vec![
                AccountConfig {
                    username: "Same@Example.invalid".into(),
                    ..AccountConfig::default()
                },
                AccountConfig {
                    username: "same@example.invalid".into(),
                    ..AccountConfig::default()
                },
            ],
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());

        config.accounts.truncate(1);
        config.zones.push(Zone {
            id: "bad".into(),
            latitude: 10.0,
            longitude: 20.0,
            radius_meters: 0.0,
            passive: false,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_base_tracked_and_away_zone_references() {
        let zone = Zone {
            id: "home".into(),
            latitude: 10.0,
            longitude: 20.0,
            radius_meters: 100.0,
            passive: false,
        };
        let mut config = AppConfig {
            zones: vec![zone],
            base_zone_id: Some("home".into()),
            tracked_from_zones: vec!["home".into()],
            away_time_zones: vec![AwayTimeZoneConfig {
                offset_hours: 23,
                device_ids: vec!["device".into()],
            }],
            ..AppConfig::default()
        };
        config.validate().unwrap();

        config.base_zone_id = Some("missing".into());
        assert!(config.validate().is_err());
        config.base_zone_id = Some("home".into());
        config.tracked_from_zones.push("home".into());
        assert!(config.validate().is_err());
        config.tracked_from_zones.truncate(1);
        config.away_time_zones[0].offset_hours = 24;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_conflicting_devices_passive_tracking_and_invalid_policy_values() {
        let mut config = AppConfig {
            accounts: vec![
                AccountConfig {
                    username: "one@example.invalid".into(),
                    device_ids: vec!["same-device".into()],
                    ..AccountConfig::default()
                },
                AccountConfig {
                    username: "two@example.invalid".into(),
                    device_ids: vec!["same-device".into()],
                    ..AccountConfig::default()
                },
            ],
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());

        config.accounts.truncate(1);
        config.zones = vec![Zone {
            id: "passive".into(),
            latitude: 10.0,
            longitude: 20.0,
            radius_meters: 100.0,
            passive: true,
        }];
        config.base_zone_id = Some("passive".into());
        assert!(config.validate().is_err());

        config.base_zone_id = None;
        config.tracking.stationary_radius_meters = f64::NAN;
        assert!(config.validate().is_err());
        config.tracking.stationary_radius_meters = 100.0;
        config.tracking.fixed_interval_seconds = 299;
        assert!(config.validate().is_err());
    }

    #[test]
    fn atomically_round_trips_configuration() {
        let directory = std::env::temp_dir().join(format!(
            "icloud-location-config-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("config.toml");
        let config = AppConfig {
            accounts: vec![AccountConfig {
                username: "example@example.invalid".into(),
                ..AccountConfig::default()
            }],
            ..AppConfig::default()
        };

        config.save(&path).unwrap();
        let restored = AppConfig::load(&path).unwrap();

        assert_eq!(restored, config);
        assert!(!directory.join("config.toml.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
