//! Normalized Find My device data.

use std::collections::HashSet;
use std::fmt;

use chrono::{DateTime, Utc};
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coordinates::{ChinaCoordinates, to_wgs84};
use crate::error::{Error, Result};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub unique_name: String,
    pub device_class: Option<String>,
    pub device_display_name: Option<String>,
    pub model_display_name: Option<String>,
    pub raw_device_model: Option<String>,
    pub status: DeviceStatus,
    pub battery: Option<Battery>,
    pub location: Option<Location>,
    pub family_shared: Option<bool>,
    pub raw: Value,
}

impl Device {
    pub(crate) fn from_apple(value: &Value, coordinates: ChinaCoordinates) -> Result<Self> {
        let id = required_string(value, "id")?;
        let name = required_string(value, "name")?
            .replace('’', "'")
            .replace('\u{a0}', " ");
        let status_code = value
            .get("deviceStatus")
            .and_then(value_as_i32)
            .unwrap_or_default();
        let battery_level = value
            .get("batteryLevel")
            .and_then(Value::as_f64)
            .and_then(|level| (level * 100.0).round().clamp(0.0, 100.0).to_u8());
        let mut battery_status = value
            .get("batteryStatus")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(level) = battery_level {
            if level > 99 {
                battery_status = Some("Charged".into());
            } else if level > 0
                && level < 20
                && !matches!(battery_status.as_deref(), Some("Charging" | "Unknown"))
            {
                battery_status = Some("Low".into());
            }
        }
        let low_power_mode = value.get("lowPowerMode").and_then(Value::as_bool);
        let battery =
            (battery_level.is_some() || battery_status.is_some() || low_power_mode.is_some())
                .then_some(Battery {
                    level_percent: battery_level,
                    status: battery_status,
                    low_power_mode,
                });

        Ok(Self {
            id,
            unique_name: name.clone(),
            name,
            device_class: optional_string(value, "deviceClass"),
            device_display_name: optional_string(value, "deviceDisplayName"),
            model_display_name: optional_string(value, "modelDisplayName"),
            raw_device_model: optional_string(value, "rawDeviceModel"),
            status: DeviceStatus::from_code(status_code),
            battery,
            location: parse_location(value.get("location"), coordinates),
            family_shared: value.get("fmlyShare").and_then(Value::as_bool),
            raw: value.clone(),
        })
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.device_display_name
            .as_deref()
            .or(self.model_display_name.as_deref())
            .or(self.raw_device_model.as_deref())
    }

    #[must_use]
    pub fn kind(&self) -> DeviceKind {
        let raw_model = self.raw_device_model.as_deref().unwrap_or_default();
        let device_class = self.device_class.as_deref().unwrap_or_default();
        if raw_model.starts_with("AirPods") {
            DeviceKind::AirPods
        } else if raw_model.starts_with("AirTag") {
            DeviceKind::AirTag
        } else if raw_model.starts_with("Watch") || device_class.eq_ignore_ascii_case("watch") {
            DeviceKind::Watch
        } else if device_class.eq_ignore_ascii_case("iphone") {
            DeviceKind::IPhone
        } else if device_class.eq_ignore_ascii_case("ipad") {
            DeviceKind::IPad
        } else if device_class.eq_ignore_ascii_case("mac") {
            DeviceKind::Mac
        } else {
            DeviceKind::Other
        }
    }

    #[must_use]
    pub fn normalized_model_name(&self) -> Option<String> {
        if matches!(self.model_display_name.as_deref(), Some("Accessory")) {
            match self.kind() {
                DeviceKind::AirPods => return Some("AirPods".into()),
                DeviceKind::AirTag => return Some("AirTags".into()),
                _ => {}
            }
        }
        if self.kind() == DeviceKind::Watch {
            return Some("Watch".into());
        }
        self.model_display_name.clone()
    }

    #[must_use]
    pub fn normalized_display_name(&self) -> Option<String> {
        let mut name = self.device_display_name.clone()?;
        for (from, to) in [
            ("generation", "gen"),
            ("nd gen", ""),
            ("th gen", ""),
            ("Series ", ""),
            ("mini", "Mini"),
            ("(", ""),
            (")", ""),
        ] {
            name = name.replace(from, to);
        }
        if let Some(index) = name.find("-inch").filter(|index| *index >= 3) {
            name.replace_range(index - 3..index + 5, "");
        }
        Some(name.replace(' ', ""))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    IPhone,
    IPad,
    Watch,
    Mac,
    AirPods,
    AirTag,
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    pub horizontal_accuracy_meters: Option<f64>,
    pub vertical_accuracy_meters: Option<f64>,
    pub position_type: Option<String>,
    pub is_old: Option<bool>,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Battery {
    pub level_percent: Option<u8>,
    pub status: Option<String>,
    pub low_power_mode: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Unknown,
    Online,
    Offline,
    Pending,
    Unregistered,
    Other(i32),
}

impl DeviceStatus {
    #[must_use]
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => Self::Unknown,
            200 => Self::Online,
            201 => Self::Offline,
            203 => Self::Pending,
            204 => Self::Unregistered,
            other => Self::Other(other),
        }
    }

    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Online => 200,
            Self::Offline => 201,
            Self::Pending => 203,
            Self::Unregistered => 204,
            Self::Other(code) => code,
        }
    }
}

impl fmt::Display for DeviceStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => formatter.write_str("unknown"),
            Self::Online => formatter.write_str("online"),
            Self::Offline => formatter.write_str("offline"),
            Self::Pending => formatter.write_str("pending"),
            Self::Unregistered => formatter.write_str("unregistered"),
            Self::Other(code) => write!(formatter, "other ({code})"),
        }
    }
}

pub(crate) fn devices_from_response(
    response: &Value,
    coordinates: ChinaCoordinates,
) -> Result<Vec<Device>> {
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::UnexpectedResponse("Find My response has no content array".into()))?;

    let mut devices: Vec<_> = content
        .iter()
        .map(|device| Device::from_apple(device, coordinates))
        .collect::<Result<_>>()?;
    assign_unique_names(&mut devices);
    Ok(devices)
}

fn assign_unique_names(devices: &mut [Device]) {
    let mut names = HashSet::new();
    for device in devices {
        let mut unique_name = device.name.clone();
        while !names.insert(unique_name.clone()) {
            unique_name.push('.');
        }
        device.unique_name = unique_name;
    }
}

fn parse_location(value: Option<&Value>, coordinates: ChinaCoordinates) -> Option<Location> {
    let value = value?.as_object()?;
    let latitude = value.get("latitude")?.as_f64()?;
    let longitude = value.get("longitude")?.as_f64()?;
    let (latitude, longitude) = to_wgs84(latitude, longitude, coordinates);
    let timestamp = value
        .get("timeStamp")
        .and_then(Value::as_i64)
        .and_then(DateTime::from_timestamp_millis);

    Some(Location {
        latitude,
        longitude,
        horizontal_accuracy_meters: value.get("horizontalAccuracy").and_then(Value::as_f64),
        vertical_accuracy_meters: value.get("verticalAccuracy").and_then(Value::as_f64),
        position_type: value
            .get("positionType")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        is_old: value.get("isOld").and_then(Value::as_bool),
        timestamp,
    })
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    optional_string(value, key)
        .ok_or_else(|| Error::UnexpectedResponse(format!("device has no {key} field")))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn value_as_i32(value: &Value) -> Option<i32> {
    value
        .as_i64()
        .and_then(|number| i32::try_from(number).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    #[test]
    fn normalizes_find_my_device_data() {
        let raw = json!({
            "id": "device-id",
            "name": "Jimmy’s\u{a0}iPhone",
            "deviceClass": "iPhone",
            "deviceDisplayName": "iPhone 16 Pro",
            "modelDisplayName": "iPhone",
            "rawDeviceModel": "iPhone17,1",
            "deviceStatus": "200",
            "batteryLevel": 0.786,
            "batteryStatus": "NotCharging",
            "lowPowerMode": false,
            "fmlyShare": true,
            "location": {
                "latitude": 59.3293,
                "longitude": 18.0686,
                "horizontalAccuracy": 7.5,
                "verticalAccuracy": 4.0,
                "positionType": "GPS",
                "isOld": false,
                "timeStamp": 1_750_000_000_123_i64
            }
        });

        let device = Device::from_apple(&raw, ChinaCoordinates::Unchanged).unwrap();

        assert_eq!(device.name, "Jimmy's iPhone");
        assert_eq!(device.status, DeviceStatus::Online);
        assert_eq!(device.battery.unwrap().level_percent, Some(79));
        let location = device.location.unwrap();
        assert!((location.latitude - 59.3293).abs() < f64::EPSILON);
        assert_eq!(
            location.timestamp,
            Utc.timestamp_millis_opt(1_750_000_000_123).single()
        );
    }

    #[test]
    fn accepts_devices_without_location_or_battery() {
        let raw = json!({
            "id": "offline-id",
            "name": "AirPods",
            "deviceStatus": 201,
            "location": null
        });

        let device = Device::from_apple(&raw, ChinaCoordinates::Unchanged).unwrap();

        assert_eq!(device.status, DeviceStatus::Offline);
        assert!(device.location.is_none());
        assert!(device.battery.is_none());
    }

    #[test]
    fn classifies_accessories_and_normalizes_battery_labels() {
        let airpods = Device::from_apple(
            &json!({
                "id": "airpods-id",
                "name": "AirPods",
                "deviceClass": "Accessory",
                "deviceDisplayName": "AirPods Pro (2nd generation)",
                "modelDisplayName": "Accessory",
                "rawDeviceModel": "AirPodsPro1,1",
                "deviceStatus": 200,
                "batteryLevel": 0.12,
                "batteryStatus": "NotCharging"
            }),
            ChinaCoordinates::Unchanged,
        )
        .unwrap();

        assert_eq!(airpods.kind(), DeviceKind::AirPods);
        assert_eq!(airpods.normalized_model_name().as_deref(), Some("AirPods"));
        assert_eq!(
            airpods.normalized_display_name().as_deref(),
            Some("AirPodsPro2")
        );
        assert_eq!(airpods.battery.unwrap().status.as_deref(), Some("Low"));
    }

    #[test]
    fn assigns_period_suffixes_to_duplicate_names() {
        let response = json!({
            "content": [
                { "id": "one", "name": "Shared iPhone", "deviceStatus": 200 },
                { "id": "two", "name": "Shared iPhone", "deviceStatus": 200 },
                { "id": "three", "name": "Shared iPhone", "deviceStatus": 200 }
            ]
        });

        let devices = devices_from_response(&response, ChinaCoordinates::Unchanged).unwrap();

        assert_eq!(devices[0].unique_name, "Shared iPhone");
        assert_eq!(devices[1].unique_name, "Shared iPhone.");
        assert_eq!(devices[2].unique_name, "Shared iPhone..");
    }
}
