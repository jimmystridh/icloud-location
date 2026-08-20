use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Utc};
use icloud_location_core::{
    BatterySnapshot, DeviceAvailability, LocationSample, LocationSourceKind,
    TimestampedTrackingEvent, TrackingEvent,
};
use serde::{Deserialize, Serialize};

use crate::{
    Direction, DirectionHistory, ExternalSourceHealth, LocationQuality, StationaryZoneManager,
    TrackingError, ZoneOccupancy, ZoneTransitionState,
};

const CURRENT_STATE_VERSION: u32 = 4;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct AccountTrackingState {
    pub device_ids: BTreeSet<String>,
    pub authentication_error_count: u32,
    pub next_discovery_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TrackFromZoneState {
    pub zone_id: String,
    pub last_distance_km: Option<f64>,
    pub direction: Direction,
    pub direction_history: DirectionHistory,
    pub went_beyond_three_km: bool,
    pub next_update_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DeviceTrackingState {
    pub device_id: String,
    pub current_location: Option<LocationSample>,
    pub previous_location: Option<LocationSample>,
    pub last_update_at: Option<DateTime<Utc>>,
    pub next_update_at: Option<DateTime<Utc>>,
    pub current_zone: Option<String>,
    pub zone_transition: ZoneTransitionState,
    pub direction: Direction,
    pub direction_history: DirectionHistory,
    pub last_zone_distance_km: Option<f64>,
    pub zone_distances_km: BTreeMap<String, f64>,
    pub track_from_zones: BTreeMap<String, TrackFromZoneState>,
    pub went_beyond_three_km: bool,
    pub distance_moved_meters: Option<f64>,
    pub location_quality: Option<LocationQuality>,
    pub consecutive_bad_updates: u32,
    pub authentication_error_count: u32,
    pub paused: bool,
    pub battery: Option<BatterySnapshot>,
    pub battery_updated_at: Option<DateTime<Utc>>,
    pub battery_source: Option<LocationSourceKind>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub family_shared: Option<bool>,
    pub availability: Option<DeviceAvailability>,
    pub offline_since: Option<DateTime<Utc>>,
    pub raw_device: serde_json::Value,
    pub route_zone_id: Option<String>,
    pub route_distance_km: Option<f64>,
    pub route_duration_seconds: Option<u64>,
    pub route_updated_at: Option<DateTime<Utc>>,
    pub nearby_group: Option<u64>,
    pub nearby_device_id: Option<String>,
    pub nearby_device_distance_meters: Option<f64>,
    pub away_time_zone_offset_hours: i32,
}

impl Default for DeviceTrackingState {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            current_location: None,
            previous_location: None,
            last_update_at: None,
            next_update_at: None,
            current_zone: None,
            zone_transition: ZoneTransitionState::default(),
            direction: Direction::Unknown,
            direction_history: DirectionHistory::default(),
            last_zone_distance_km: None,
            zone_distances_km: BTreeMap::new(),
            track_from_zones: BTreeMap::new(),
            went_beyond_three_km: false,
            distance_moved_meters: None,
            location_quality: None,
            consecutive_bad_updates: 0,
            authentication_error_count: 0,
            paused: false,
            battery: None,
            battery_updated_at: None,
            battery_source: None,
            name: None,
            model: None,
            family_shared: None,
            availability: None,
            offline_since: None,
            raw_device: serde_json::Value::Null,
            route_zone_id: None,
            route_distance_km: None,
            route_duration_seconds: None,
            route_updated_at: None,
            nearby_group: None,
            nearby_device_id: None,
            nearby_device_distance_meters: None,
            away_time_zone_offset_hours: 0,
        }
    }
}

impl DeviceTrackingState {
    #[must_use]
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            ..Self::default()
        }
    }

    pub fn apply_location(&mut self, sample: LocationSample, quality: &LocationQuality) {
        match quality {
            LocationQuality::Good => {
                self.accept_location(sample);
                self.consecutive_bad_updates = 0;
            }
            LocationQuality::Grace(_) => {
                self.accept_location(sample);
                self.consecutive_bad_updates = self.consecutive_bad_updates.saturating_add(1);
            }
            LocationQuality::Rejected(_) => {
                self.consecutive_bad_updates = self.consecutive_bad_updates.saturating_add(1);
            }
        }
    }

    fn accept_location(&mut self, sample: LocationSample) {
        let timestamp = sample.timestamp;
        self.distance_moved_meters = self
            .current_location
            .as_ref()
            .map(|current| current.coordinates.distance_meters(sample.coordinates));
        self.previous_location = self.current_location.replace(sample);
        self.last_update_at = Some(timestamp);
    }

    pub fn update_battery(&mut self, battery: BatterySnapshot, timestamp: DateTime<Utc>) {
        if self
            .battery_updated_at
            .is_none_or(|current| timestamp >= current)
        {
            self.battery = Some(battery);
            self.battery_updated_at = Some(timestamp);
        }
    }

    pub fn update_availability(&mut self, availability: DeviceAvailability, now: DateTime<Utc>) {
        if matches!(availability, DeviceAvailability::Online) {
            self.offline_since = None;
        } else if self.offline_since.is_none() {
            self.offline_since = Some(now);
        }
        self.availability = Some(availability);
    }

    #[must_use]
    pub fn offline_duration_seconds(&self, now: DateTime<Utc>) -> u64 {
        self.offline_since.map_or(0, |started| {
            u64::try_from(now.signed_duration_since(started).num_seconds().max(0))
                .unwrap_or_default()
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TrackingState {
    pub version: u32,
    pub saved_at: Option<DateTime<Utc>>,
    pub accounts: BTreeMap<String, AccountTrackingState>,
    pub devices: BTreeMap<String, DeviceTrackingState>,
    pub stationary_zones: StationaryZoneManager,
    pub zone_occupancy: ZoneOccupancy,
    pub event_history: Vec<TimestampedTrackingEvent>,
    pub external_source_health: BTreeMap<String, ExternalSourceHealth>,
}

impl Default for TrackingState {
    fn default() -> Self {
        Self {
            version: CURRENT_STATE_VERSION,
            saved_at: None,
            accounts: BTreeMap::new(),
            devices: BTreeMap::new(),
            stationary_zones: StationaryZoneManager::default(),
            zone_occupancy: ZoneOccupancy::default(),
            event_history: Vec::new(),
            external_source_health: BTreeMap::new(),
        }
    }
}

impl TrackingState {
    pub fn record_event(&mut self, occurred_at: DateTime<Utc>, event: TrackingEvent) {
        self.event_history
            .push(TimestampedTrackingEvent { occurred_at, event });
        if self.event_history.len() > 1_000 {
            self.event_history.drain(..self.event_history.len() - 1_000);
        }
    }

    #[must_use]
    pub fn snapshot(&self, generated_at: DateTime<Utc>) -> TrackingSnapshot {
        TrackingSnapshot {
            version: self.version,
            generated_at,
            devices: self
                .devices
                .values()
                .map(|device| DeviceTrackingSnapshot::from_state(device, generated_at))
                .collect(),
            zone_occupancy: self.zone_occupancy.counts(),
            external_source_health: self.external_source_health.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TrackingSnapshot {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub devices: Vec<DeviceTrackingSnapshot>,
    pub zone_occupancy: BTreeMap<String, usize>,
    pub external_source_health: BTreeMap<String, ExternalSourceHealth>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeviceTrackingSnapshot {
    pub device_id: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub family_shared: Option<bool>,
    pub availability: Option<DeviceAvailability>,
    pub offline_duration_seconds: u64,
    pub location_age_seconds: Option<u64>,
    pub current_location: Option<LocationSample>,
    pub previous_location: Option<LocationSample>,
    pub current_zone: Option<String>,
    pub zone_distances_km: BTreeMap<String, f64>,
    pub track_from_zones: BTreeMap<String, TrackFromZoneSnapshot>,
    pub direction: Direction,
    pub distance_moved_meters: Option<f64>,
    pub location_quality: Option<LocationQuality>,
    pub next_update_at: Option<DateTime<Utc>>,
    pub battery: Option<BatterySnapshot>,
    pub battery_updated_at: Option<DateTime<Utc>>,
    pub battery_source: Option<LocationSourceKind>,
    pub route_zone_id: Option<String>,
    pub route_distance_km: Option<f64>,
    pub route_duration_seconds: Option<u64>,
    pub arrival_at: Option<DateTime<Utc>>,
    pub nearby_group: Option<u64>,
    pub nearby_device_id: Option<String>,
    pub nearby_device_distance_meters: Option<f64>,
    pub away_time_zone_offset_hours: i32,
    pub away_time_zone_time: Option<DateTime<FixedOffset>>,
    pub raw_device: serde_json::Value,
}

impl DeviceTrackingSnapshot {
    fn from_state(state: &DeviceTrackingState, generated_at: DateTime<Utc>) -> Self {
        let arrival_at = state.route_duration_seconds.and_then(|seconds| {
            i64::try_from(seconds).ok().map(|seconds| {
                state.route_updated_at.unwrap_or(generated_at) + chrono::Duration::seconds(seconds)
            })
        });
        Self {
            device_id: state.device_id.clone(),
            name: state.name.clone(),
            model: state.model.clone(),
            family_shared: state.family_shared,
            availability: state.availability.clone(),
            offline_duration_seconds: state.offline_duration_seconds(generated_at),
            location_age_seconds: state.current_location.as_ref().map(|location| {
                u64::try_from(
                    generated_at
                        .signed_duration_since(location.timestamp)
                        .num_seconds()
                        .max(0),
                )
                .unwrap_or_default()
            }),
            current_location: state.current_location.clone(),
            previous_location: state.previous_location.clone(),
            current_zone: state.current_zone.clone(),
            zone_distances_km: state.zone_distances_km.clone(),
            track_from_zones: state
                .track_from_zones
                .iter()
                .map(|(zone_id, track)| {
                    (
                        zone_id.clone(),
                        TrackFromZoneSnapshot {
                            distance_km: track.last_distance_km,
                            direction: track.direction,
                            next_update_at: track.next_update_at,
                        },
                    )
                })
                .collect(),
            direction: state.direction,
            distance_moved_meters: state.distance_moved_meters,
            location_quality: state.location_quality.clone(),
            next_update_at: state.next_update_at,
            battery: state.battery.clone(),
            battery_updated_at: state.battery_updated_at,
            battery_source: state.battery_source.clone(),
            route_zone_id: state.route_zone_id.clone(),
            route_distance_km: state.route_distance_km,
            route_duration_seconds: state.route_duration_seconds,
            arrival_at,
            nearby_group: state.nearby_group,
            nearby_device_id: state.nearby_device_id.clone(),
            nearby_device_distance_meters: state.nearby_device_distance_meters,
            away_time_zone_offset_hours: state.away_time_zone_offset_hours,
            away_time_zone_time: (state.away_time_zone_offset_hours != 0)
                .then(|| {
                    FixedOffset::east_opt(state.away_time_zone_offset_hours * 3_600)
                        .map(|offset| generated_at.with_timezone(&offset))
                })
                .flatten(),
            raw_device: state.raw_device.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TrackFromZoneSnapshot {
    pub distance_km: Option<f64>,
    pub direction: Direction,
    pub next_update_at: Option<DateTime<Utc>>,
}

pub trait TrackingStateStore: Send + Sync {
    /// Loads the latest durable tracking state.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, invalid JSON, or unsupported future versions.
    fn load(&self) -> Result<TrackingState, TrackingError>;

    /// Atomically replaces durable tracking state.
    ///
    /// # Errors
    ///
    /// Returns an error when private storage cannot be written or renamed.
    fn save(&self, state: &TrackingState) -> Result<(), TrackingError>;
}

#[derive(Clone, Debug)]
pub struct JsonTrackingStore {
    path: PathBuf,
}

impl JsonTrackingStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TrackingStateStore for JsonTrackingStore {
    fn load(&self) -> Result<TrackingState, TrackingError> {
        if !self.path.exists() {
            return Ok(TrackingState::default());
        }
        let reader = BufReader::new(File::open(&self.path).map_err(store_error)?);
        let mut value: serde_json::Value = serde_json::from_reader(reader).map_err(store_error)?;
        migrate_state(&mut value)?;
        serde_json::from_value(value).map_err(store_error)
    }

    fn save(&self, state: &TrackingState) -> Result<(), TrackingError> {
        if state.version != CURRENT_STATE_VERSION {
            return Err(TrackingError::Persistence(format!(
                "cannot save tracking state version {}",
                state.version
            )));
        }
        let parent = self.path.parent().ok_or_else(|| {
            TrackingError::Persistence("tracking state path has no parent".into())
        })?;
        fs::create_dir_all(parent).map_err(store_error)?;
        set_directory_permissions(parent)?;
        let temporary = self.path.with_extension("json.tmp");
        {
            let mut writer = BufWriter::new(create_private_file(&temporary)?);
            serde_json::to_writer_pretty(&mut writer, state).map_err(store_error)?;
            writer.write_all(b"\n").map_err(store_error)?;
            writer.flush().map_err(store_error)?;
            writer.get_ref().sync_all().map_err(store_error)?;
        }
        fs::rename(&temporary, &self.path).map_err(store_error)?;
        Ok(())
    }
}

fn migrate_state(value: &mut serde_json::Value) -> Result<(), TrackingError> {
    if !value.is_object() {
        return Err(TrackingError::Persistence(
            "tracking state root must be an object".into(),
        ));
    }
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    match version {
        None | Some(0) => {
            value["version"] = serde_json::Value::from(CURRENT_STATE_VERSION);
            if value.get("devices").is_none() {
                value["devices"] = serde_json::json!({});
            }
            Ok(())
        }
        Some(1) => {
            value["version"] = serde_json::Value::from(CURRENT_STATE_VERSION);
            if value.get("event_history").is_none() {
                value["event_history"] = serde_json::json!([]);
            }
            Ok(())
        }
        Some(2 | 3) => {
            value["version"] = serde_json::Value::from(CURRENT_STATE_VERSION);
            Ok(())
        }
        Some(version) if version == u64::from(CURRENT_STATE_VERSION) => Ok(()),
        Some(version) => Err(TrackingError::Persistence(format!(
            "tracking state version {version} is newer than supported version {CURRENT_STATE_VERSION}"
        ))),
    }
}

fn create_private_file(path: &Path) -> Result<File, TrackingError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(store_error)?;
    set_file_permissions(path)?;
    Ok(file)
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), TrackingError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(store_error)
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), TrackingError> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<(), TrackingError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(store_error)
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<(), TrackingError> {
    Ok(())
}

fn store_error(error: impl std::fmt::Display) -> TrackingError {
    TrackingError::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::TimeZone;
    use icloud_location_core::{Coordinates, LocationSourceKind};

    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "icloud-location-tracking-{name}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("state.json")
    }

    fn location(seconds: i64, longitude: f64) -> LocationSample {
        LocationSample {
            coordinates: Coordinates::new(10.0, longitude).unwrap(),
            horizontal_accuracy_meters: Some(5.0),
            vertical_accuracy_meters: None,
            timestamp: Utc.timestamp_opt(seconds, 0).unwrap(),
            source: LocationSourceKind::Apple,
            is_old: false,
        }
    }

    #[test]
    fn preserves_current_and_previous_location_across_restart() {
        let path = temporary_path("roundtrip");
        let store = JsonTrackingStore::new(&path);
        let mut device = DeviceTrackingState::new("device-id");
        device.apply_location(location(100, 20.0), &LocationQuality::Good);
        device.apply_location(location(200, 20.001), &LocationQuality::Good);
        let mut state = TrackingState::default();
        state.devices.insert(device.device_id.clone(), device);

        store.save(&state).unwrap();
        let restored = store.load().unwrap();

        let device = &restored.devices["device-id"];
        assert_eq!(
            device
                .current_location
                .as_ref()
                .unwrap()
                .timestamp
                .timestamp(),
            200
        );
        assert_eq!(
            device
                .previous_location
                .as_ref()
                .unwrap()
                .timestamp
                .timestamp(),
            100
        );
        assert!(device.distance_moved_meters.unwrap() > 100.0);
        assert_eq!(
            restored
                .snapshot(Utc.timestamp_opt(300, 0).unwrap())
                .devices[0]
                .location_age_seconds,
            Some(100)
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn migrates_unversioned_state_and_rejects_future_state() {
        let mut legacy = serde_json::json!({ "saved_at": null });
        migrate_state(&mut legacy).unwrap();
        assert_eq!(legacy["version"], CURRENT_STATE_VERSION);
        assert_eq!(legacy["devices"], serde_json::json!({}));

        let mut future = serde_json::json!({ "version": 99, "devices": {} });
        assert!(migrate_state(&mut future).is_err());

        let mut version_two = serde_json::json!({ "version": 2, "devices": {} });
        migrate_state(&mut version_two).unwrap();
        assert_eq!(version_two["version"], CURRENT_STATE_VERSION);
        let mut version_three = serde_json::json!({ "version": 3, "devices": {} });
        migrate_state(&mut version_three).unwrap();
        assert_eq!(version_three["version"], CURRENT_STATE_VERSION);
    }

    #[test]
    fn newer_battery_source_wins() {
        let mut device = DeviceTrackingState::new("device-id");
        device.update_battery(
            BatterySnapshot {
                level_percent: Some(80),
                ..BatterySnapshot::default()
            },
            Utc.timestamp_opt(200, 0).unwrap(),
        );
        device.update_battery(
            BatterySnapshot {
                level_percent: Some(20),
                ..BatterySnapshot::default()
            },
            Utc.timestamp_opt(100, 0).unwrap(),
        );

        assert_eq!(device.battery.as_ref().unwrap().level_percent, Some(80));
    }

    #[test]
    fn snapshot_exposes_zone_counts_away_offset_and_raw_device() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let mut state = TrackingState::default();
        let mut device = DeviceTrackingState::new("device-id");
        device.current_zone = Some("home".into());
        device.family_shared = Some(true);
        device.location_quality = Some(LocationQuality::Good);
        device.away_time_zone_offset_hours = 12;
        device.raw_device = serde_json::json!({ "custom": true });
        device.route_duration_seconds = Some(600);
        device.route_updated_at = Some(now);
        device.track_from_zones.insert(
            "home".into(),
            TrackFromZoneState {
                zone_id: "home".into(),
                last_distance_km: Some(1.25),
                direction: Direction::Towards,
                next_update_at: Some(now + chrono::Duration::seconds(60)),
                ..TrackFromZoneState::default()
            },
        );
        state.devices.insert("device-id".into(), device);
        state.zone_occupancy.update("device-id", Some("home"));
        let mut source_health = ExternalSourceHealth::default();
        source_health.record_update(now);
        state
            .external_source_health
            .insert("mobile".into(), source_health);

        let generated_at = now + chrono::Duration::seconds(200);
        let snapshot = state.snapshot(generated_at);

        assert_eq!(snapshot.zone_occupancy["home"], 1);
        assert_eq!(
            snapshot.external_source_health["mobile"].last_update_at,
            Some(now)
        );
        assert_eq!(
            snapshot.devices[0]
                .away_time_zone_time
                .unwrap()
                .offset()
                .local_minus_utc(),
            43_200
        );
        assert_eq!(snapshot.devices[0].raw_device["custom"], true);
        assert_eq!(snapshot.devices[0].family_shared, Some(true));
        assert_eq!(
            snapshot.devices[0].location_quality,
            Some(LocationQuality::Good)
        );
        assert_eq!(
            snapshot.devices[0].arrival_at,
            Some(now + chrono::Duration::seconds(600))
        );
        assert_eq!(
            snapshot.devices[0].track_from_zones["home"].direction,
            Direction::Towards
        );
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["version"], CURRENT_STATE_VERSION);
        assert!(json["generated_at"].is_string());
        assert!(json["devices"][0]["zone_distances_km"].is_object());
        assert_eq!(json["devices"][0]["raw_device"]["custom"], true);
    }

    #[test]
    fn atomic_store_ignores_partial_temporary_file_and_uses_private_permissions() {
        let path = temporary_path("atomic");
        let store = JsonTrackingStore::new(&path);
        let mut state = TrackingState::default();
        state
            .devices
            .insert("device-id".into(), DeviceTrackingState::new("device-id"));
        store.save(&state).unwrap();
        fs::write(path.with_extension("json.tmp"), b"{partial").unwrap();

        assert!(store.load().unwrap().devices.contains_key("device-id"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
