use std::collections::{BTreeMap, BTreeSet};

use icloud_location_core::Coordinates;
use serde::{Deserialize, Serialize};

use crate::TrackingError;

#[derive(Clone, Debug)]
pub struct NearbyDevice<'a> {
    pub device_id: &'a str,
    pub location: Coordinates,
    pub horizontal_accuracy_meters: Option<f64>,
    pub current_zone: Option<&'a str>,
    pub tracked: bool,
    pub online: bool,
    pub is_watch: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct NearbyDevicePolicy {
    pub maximum_distance_meters: f64,
    pub maximum_accuracy_meters: f64,
}

impl Default for NearbyDevicePolicy {
    fn default() -> Self {
        Self {
            maximum_distance_meters: 25.0,
            maximum_accuracy_meters: 25.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NearbyDeviceGroup {
    pub id: u64,
    pub device_ids: Vec<String>,
}

/// Groups devices connected by iCloud3's 25-meter nearby threshold.
///
/// # Errors
///
/// Returns an error for invalid policy distances or GPS accuracy values.
pub fn group_nearby_devices(
    devices: &[NearbyDevice<'_>],
    policy: NearbyDevicePolicy,
) -> Result<Vec<NearbyDeviceGroup>, TrackingError> {
    if !policy.maximum_distance_meters.is_finite()
        || !policy.maximum_accuracy_meters.is_finite()
        || policy.maximum_distance_meters < 0.0
        || policy.maximum_accuracy_meters < 0.0
        || devices.iter().any(|device| {
            device
                .horizontal_accuracy_meters
                .is_some_and(|accuracy| !accuracy.is_finite() || accuracy < 0.0)
        })
    {
        return Err(TrackingError::InvalidInput(
            "nearby-device thresholds and accuracy must be finite and non-negative".into(),
        ));
    }

    let mut adjacency: BTreeMap<&str, BTreeSet<&str>> = devices
        .iter()
        .map(|device| (device.device_id, BTreeSet::new()))
        .collect();
    for (index, left) in devices.iter().enumerate() {
        for right in &devices[index + 1..] {
            let eligible = left.tracked
                && right.tracked
                && left.online
                && right.online
                && !left.is_watch
                && !right.is_watch
                && left.current_zone == right.current_zone
                && left
                    .horizontal_accuracy_meters
                    .unwrap_or(f64::INFINITY)
                    .min(right.horizontal_accuracy_meters.unwrap_or(f64::INFINITY))
                    <= policy.maximum_accuracy_meters
                && left.location.distance_meters(right.location) <= policy.maximum_distance_meters;
            if eligible {
                if let Some(neighbors) = adjacency.get_mut(left.device_id) {
                    neighbors.insert(right.device_id);
                }
                if let Some(neighbors) = adjacency.get_mut(right.device_id) {
                    neighbors.insert(left.device_id);
                }
            }
        }
    }

    let mut visited = BTreeSet::new();
    let mut groups = Vec::new();
    for device in devices {
        if visited.contains(device.device_id) {
            continue;
        }
        let mut pending = vec![device.device_id];
        let mut members = Vec::new();
        while let Some(device_id) = pending.pop() {
            if !visited.insert(device_id) {
                continue;
            }
            members.push(device_id.to_owned());
            pending.extend(adjacency[device_id].iter().copied());
        }
        if members.len() > 1 {
            members.sort();
            groups.push(NearbyDeviceGroup {
                id: u64::try_from(groups.len() + 1).unwrap_or(u64::MAX),
                device_ids: members,
            });
        }
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_eligible_devices_and_rejects_watch_or_poor_gps() {
        let origin = Coordinates::new(0.0, 0.0).unwrap();
        let near = Coordinates::new(0.0, 0.000_1).unwrap();
        let devices = [
            NearbyDevice {
                device_id: "phone",
                location: origin,
                horizontal_accuracy_meters: Some(5.0),
                current_zone: Some("home"),
                tracked: true,
                online: true,
                is_watch: false,
            },
            NearbyDevice {
                device_id: "tablet",
                location: near,
                horizontal_accuracy_meters: Some(5.0),
                current_zone: Some("home"),
                tracked: true,
                online: true,
                is_watch: false,
            },
            NearbyDevice {
                device_id: "watch",
                location: near,
                horizontal_accuracy_meters: Some(5.0),
                current_zone: Some("home"),
                tracked: true,
                online: true,
                is_watch: true,
            },
        ];

        let groups = group_nearby_devices(&devices, NearbyDevicePolicy::default()).unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].device_ids, ["phone", "tablet"]);
    }
}
