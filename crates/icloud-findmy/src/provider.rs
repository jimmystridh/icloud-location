use icloud_location_core::{
    BatterySnapshot, BoxFuture, Coordinates, DeviceAvailability, DeviceSnapshot, LocationProvider,
    LocationRequest, LocationSample, LocationSourceKind, ProviderError, ProviderErrorKind,
};
use tokio::sync::{Mutex, MutexGuard};

use crate::{Device, DeviceStatus, Error, ICloudClient, LocateOptions};

pub struct FindMyProvider {
    client: Mutex<ICloudClient>,
}

impl FindMyProvider {
    #[must_use]
    pub fn new(client: ICloudClient) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }

    pub async fn client(&self) -> MutexGuard<'_, ICloudClient> {
        self.client.lock().await
    }

    pub fn into_inner(self) -> ICloudClient {
        self.client.into_inner()
    }
}

impl LocationProvider for FindMyProvider {
    fn locate<'a>(
        &'a self,
        request: &'a LocationRequest,
    ) -> BoxFuture<'a, Result<Vec<DeviceSnapshot>, ProviderError>> {
        Box::pin(async move {
            let mut client = self.client.lock().await;
            let mut options = if request.family {
                LocateOptions::family()
            } else {
                LocateOptions::owner()
            };
            if let Some(device_id) = &request.selected_device {
                options = options.selected(device_id);
            }
            client
                .locate_devices(options)
                .await
                .map_err(|error| find_my_provider_error(&error))?
                .into_iter()
                .map(snapshot_from_device)
                .collect()
        })
    }
}

fn snapshot_from_device(device: Device) -> Result<DeviceSnapshot, ProviderError> {
    let model = device
        .normalized_display_name()
        .or_else(|| device.model().map(str::to_owned));
    let location = device
        .location
        .map(|location| {
            let timestamp = location.timestamp.ok_or_else(|| ProviderError {
                kind: ProviderErrorKind::Other,
                message: "device location has no timestamp".into(),
            })?;
            let coordinates =
                Coordinates::new(location.latitude, location.longitude).map_err(provider_error)?;
            Ok(LocationSample {
                coordinates,
                horizontal_accuracy_meters: location.horizontal_accuracy_meters,
                vertical_accuracy_meters: location.vertical_accuracy_meters,
                timestamp,
                source: LocationSourceKind::Apple,
                is_old: location.is_old.unwrap_or_default(),
            })
        })
        .transpose()?;
    let battery = device.battery.map(|battery| BatterySnapshot {
        level_percent: battery.level_percent,
        status: battery.status,
        low_power_mode: battery.low_power_mode,
    });
    Ok(DeviceSnapshot {
        id: device.id,
        name: device.unique_name,
        model,
        availability: availability(device.status),
        battery,
        location,
        family_shared: device.family_shared,
        raw: device.raw,
    })
}

const fn availability(status: DeviceStatus) -> DeviceAvailability {
    match status {
        DeviceStatus::Unknown => DeviceAvailability::Unknown,
        DeviceStatus::Online => DeviceAvailability::Online,
        DeviceStatus::Offline => DeviceAvailability::Offline,
        DeviceStatus::Pending => DeviceAvailability::Pending,
        DeviceStatus::Unregistered => DeviceAvailability::Unregistered,
        DeviceStatus::Other(code) => DeviceAvailability::Other(code),
    }
}

fn provider_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Other,
        message: error.to_string(),
    }
}

fn find_my_provider_error(error: &Error) -> ProviderError {
    let kind = match error {
        Error::CredentialsRequired
        | Error::NotAuthenticated
        | Error::TwoFactorRequired
        | Error::TermsOfUseRequired
        | Error::AccountLocked => ProviderErrorKind::Authentication,
        Error::FindMyUnavailable => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Other,
    };
    ProviderError {
        kind,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::{ChinaCoordinates, Device};

    #[test]
    fn preserves_raw_response_in_provider_snapshot() {
        let raw = json!({
            "id": "device-id",
            "name": "Test iPhone",
            "deviceClass": "iPhone",
            "deviceDisplayName": "iPhone Example Pro",
            "deviceStatus": 200,
            "batteryLevel": 0.5,
            "location": {
                "latitude": 10.0,
                "longitude": 20.0,
                "horizontalAccuracy": 5.0,
                "timeStamp": 1_750_000_000_000_i64
            }
        });
        let device = Device::from_apple(&raw, ChinaCoordinates::Unchanged).unwrap();

        let snapshot = snapshot_from_device(device).unwrap();

        assert_eq!(snapshot.raw, raw);
        assert_eq!(
            snapshot.location.unwrap().timestamp,
            chrono::Utc.timestamp_millis_opt(1_750_000_000_000).unwrap()
        );
    }
}
