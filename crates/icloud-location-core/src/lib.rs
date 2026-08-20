//! Platform-neutral models and interfaces shared by tracking components.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Coordinates {
    pub latitude: f64,
    pub longitude: f64,
}

impl<'de> Deserialize<'de> for Coordinates {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireCoordinates {
            latitude: f64,
            longitude: f64,
        }

        let coordinates = WireCoordinates::deserialize(deserializer)?;
        Self::new(coordinates.latitude, coordinates.longitude).map_err(serde::de::Error::custom)
    }
}

impl Coordinates {
    /// Creates validated latitude and longitude coordinates.
    ///
    /// # Errors
    ///
    /// Returns an error for non-finite values or values outside WGS-84 ranges.
    pub fn new(latitude: f64, longitude: f64) -> Result<Self, CoreError> {
        if !latitude.is_finite() || !longitude.is_finite() {
            return Err(CoreError::InvalidCoordinates(
                "latitude and longitude must be finite".into(),
            ));
        }
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(CoreError::InvalidCoordinates(format!(
                "latitude {latitude} is outside -90..=90"
            )));
        }
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(CoreError::InvalidCoordinates(format!(
                "longitude {longitude} is outside -180..=180"
            )));
        }
        Ok(Self {
            latitude,
            longitude,
        })
    }

    #[must_use]
    pub fn distance_meters(self, other: Self) -> f64 {
        const EARTH_RADIUS_METERS: f64 = 6_371_008.8;

        let latitude_delta = (other.latitude - self.latitude).to_radians();
        let longitude_delta = (other.longitude - self.longitude).to_radians();
        let latitude_a = self.latitude.to_radians();
        let latitude_b = other.latitude.to_radians();
        let haversine = (latitude_delta / 2.0).sin().powi(2)
            + latitude_a.cos() * latitude_b.cos() * (longitude_delta / 2.0).sin().powi(2);
        EARTH_RADIUS_METERS * 2.0 * haversine.sqrt().atan2((1.0 - haversine).sqrt())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationSourceKind {
    Apple,
    External(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocationSample {
    pub coordinates: Coordinates,
    pub horizontal_accuracy_meters: Option<f64>,
    pub vertical_accuracy_meters: Option<f64>,
    pub timestamp: DateTime<Utc>,
    pub source: LocationSourceKind,
    pub is_old: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatterySnapshot {
    pub level_percent: Option<u8>,
    pub status: Option<String>,
    pub low_power_mode: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAvailability {
    Unknown,
    Online,
    Offline,
    Pending,
    Unregistered,
    Other(i32),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeviceSnapshot {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub availability: DeviceAvailability,
    pub battery: Option<BatterySnapshot>,
    pub location: Option<LocationSample>,
    pub family_shared: Option<bool>,
    pub raw: serde_json::Value,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocationRequest {
    pub family: bool,
    pub selected_device: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExternalLocationUpdate {
    pub device_id: String,
    pub sample: LocationSample,
    pub battery: Option<BatterySnapshot>,
    pub trigger: Option<ExternalTrigger>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalTrigger {
    ZoneEntered(String),
    ZoneExited(String),
    Manual,
    Background,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TrackingEvent {
    AuthenticationRequired {
        account: String,
    },
    DeviceUpdated {
        device_id: String,
    },
    DeviceOffline {
        device_id: String,
    },
    ZoneEntered {
        device_id: String,
        zone_id: String,
    },
    ZoneExited {
        device_id: String,
        zone_id: String,
    },
    StationaryZoneCreated {
        zone_id: String,
        device_id: String,
    },
    StationaryZoneMoved {
        zone_id: String,
        device_id: String,
    },
    StationaryZoneRemoved {
        zone_id: String,
    },
    TrackingPaused {
        device_id: Option<String>,
    },
    TrackingResumed {
        device_id: Option<String>,
    },
    TrackingScheduled {
        device_id: String,
        at: DateTime<Utc>,
    },
    TrackingLocateRequested {
        account: String,
    },
    Warning {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimestampedTrackingEvent {
    pub occurred_at: DateTime<Utc>,
    pub event: TrackingEvent,
}

pub trait LocationProvider: Send + Sync {
    fn locate<'a>(
        &'a self,
        request: &'a LocationRequest,
    ) -> BoxFuture<'a, Result<Vec<DeviceSnapshot>, ProviderError>>;
}

pub trait ExternalLocationSource: Send + Sync {
    /// Returns the next available external update, or `None` when the source is
    /// currently drained.
    fn next_update(&self) -> BoxFuture<'_, Result<Option<ExternalLocationUpdate>, ProviderError>>;
}

pub trait ExternalLocationRequester: Send + Sync {
    fn request_location<'a>(
        &'a self,
        device_id: &'a str,
    ) -> BoxFuture<'a, Result<(), ProviderError>>;
}

pub trait EventSink: Send + Sync {
    /// Publishes a tracking event.
    ///
    /// # Errors
    ///
    /// Returns an error when the destination cannot accept the event.
    fn emit(&self, event: &TrackingEvent) -> Result<(), EventSinkError>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub trait CredentialProvider: Send + Sync {
    /// Retrieves a password for an account without requiring persistence in the client.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured credential source cannot be read.
    fn password(&self, account: &str) -> Result<Option<SecretString>, CredentialError>;
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid coordinates: {0}")]
    InvalidCoordinates(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderErrorKind {
    Authentication,
    Unavailable,
    #[default]
    Other,
}

#[derive(Debug, Error)]
#[error("location provider failed: {message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

#[derive(Debug, Error)]
#[error("event sink failed: {message}")]
pub struct EventSinkError {
    pub message: String,
}

#[derive(Debug, Error)]
#[error("credential provider failed: {message}")]
pub struct CredentialError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_coordinate_ranges() {
        assert!(Coordinates::new(90.0, -180.0).is_ok());
        assert!(Coordinates::new(90.1, 0.0).is_err());
        assert!(Coordinates::new(0.0, f64::NAN).is_err());
        assert!(
            serde_json::from_str::<Coordinates>(r#"{"latitude":90.1,"longitude":0.0}"#).is_err()
        );
    }

    #[test]
    fn calculates_known_equatorial_distance() {
        let start = Coordinates::new(0.0, 0.0).unwrap();
        let end = Coordinates::new(0.0, 1.0).unwrap();

        assert!((start.distance_meters(end) - 111_195.08).abs() < 0.1);
    }
}
