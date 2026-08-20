use std::cmp::Ordering;
use std::collections::HashSet;

use icloud_location_core::Coordinates;
use serde::{Deserialize, Serialize};

use crate::TrackingError;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Zone {
    pub id: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius_meters: f64,
    #[serde(default)]
    pub passive: bool,
}

impl Zone {
    /// Returns the validated center coordinates.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid latitude or longitude values.
    pub fn center(&self) -> Result<Coordinates, TrackingError> {
        Coordinates::new(self.latitude, self.longitude)
            .map_err(|error| TrackingError::InvalidInput(error.to_string()))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ZoneDistance {
    pub zone_id: String,
    pub distance_meters: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ZoneSelection {
    pub selected_zone: Option<String>,
    pub selected_distance_meters: Option<f64>,
    pub distances: Vec<ZoneDistance>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ZoneSet {
    zones: Vec<Zone>,
}

impl ZoneSet {
    /// Creates and validates a set of zones.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate IDs, invalid coordinates, empty IDs, or
    /// non-positive radii.
    pub fn new(zones: Vec<Zone>) -> Result<Self, TrackingError> {
        let mut ids = HashSet::new();
        for zone in &zones {
            if zone.id.trim().is_empty() {
                return Err(TrackingError::InvalidInput(
                    "zone ID cannot be empty".into(),
                ));
            }
            if !ids.insert(zone.id.clone()) {
                return Err(TrackingError::DuplicateZone(zone.id.clone()));
            }
            zone.center()?;
            if !zone.radius_meters.is_finite() || zone.radius_meters <= 0.0 {
                return Err(TrackingError::InvalidInput(format!(
                    "zone {} has an invalid radius",
                    zone.id
                )));
            }
        }
        Ok(Self { zones })
    }

    #[must_use]
    pub fn zones(&self) -> &[Zone] {
        &self.zones
    }

    /// Selects the smallest active zone containing a location after applying
    /// half of the horizontal accuracy as iCloud3's boundary allowance.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid location or accuracy values.
    pub fn select(
        &self,
        location: Coordinates,
        horizontal_accuracy_meters: f64,
    ) -> Result<ZoneSelection, TrackingError> {
        if !horizontal_accuracy_meters.is_finite() || horizontal_accuracy_meters < 0.0 {
            return Err(TrackingError::InvalidInput(format!(
                "horizontal accuracy must be finite and non-negative, got {horizontal_accuracy_meters}"
            )));
        }
        let allowance = (horizontal_accuracy_meters / 2.0).trunc();
        let mut distances = Vec::new();
        let mut candidates = Vec::new();
        for zone in self.zones.iter().filter(|zone| !zone.passive) {
            let distance = location.distance_meters(zone.center()?);
            distances.push(ZoneDistance {
                zone_id: zone.id.clone(),
                distance_meters: distance,
            });
            if distance <= zone.radius_meters + allowance {
                candidates.push((zone, distance));
            }
        }
        distances.sort_by(compare_zone_distances);
        candidates.sort_by(|(left, _), (right, _)| {
            left.radius_meters
                .total_cmp(&right.radius_meters)
                .then_with(|| left.id.cmp(&right.id))
        });
        let selected = candidates.first();

        Ok(ZoneSelection {
            selected_zone: selected.map(|(zone, _)| zone.id.clone()),
            selected_distance_meters: selected.map(|(_, distance)| *distance),
            distances,
        })
    }

    /// Returns the closest zone with a usable radius.
    ///
    /// # Errors
    ///
    /// Returns an error if a stored zone has invalid coordinates.
    pub fn closest(&self, location: Coordinates) -> Result<Option<ZoneDistance>, TrackingError> {
        self.zones
            .iter()
            .filter(|zone| zone.radius_meters > 1.0)
            .map(|zone| {
                Ok(ZoneDistance {
                    zone_id: zone.id.clone(),
                    distance_meters: location.distance_meters(zone.center()?),
                })
            })
            .collect::<Result<Vec<_>, TrackingError>>()
            .map(|mut distances| {
                distances.sort_by(compare_zone_distances);
                distances.into_iter().next()
            })
    }
}

/// iCloud3 treats two zones as the same or overlapping when their centers are
/// within two meters, independently from their configured radii.
///
/// # Errors
///
/// Returns an error when either zone has invalid coordinates.
pub fn zones_have_same_center(left: &Zone, right: &Zone) -> Result<bool, TrackingError> {
    if left.id == right.id {
        return Ok(true);
    }
    Ok(left.center()?.distance_meters(right.center()?) <= 2.0)
}

fn compare_zone_distances(left: &ZoneDistance, right: &ZoneDistance) -> Ordering {
    left.distance_meters
        .total_cmp(&right.distance_meters)
        .then_with(|| left.zone_id.cmp(&right.zone_id))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Fixture {
        zones: Vec<Zone>,
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        latitude: f64,
        longitude: f64,
        horizontal_accuracy_meters: f64,
        expected_zone: Option<String>,
    }

    #[test]
    fn matches_zone_selection_fixture() {
        let fixture: Fixture = serde_json::from_str(include_str!(
            "../../../tests/fixtures/zones/zone_cases.json"
        ))
        .unwrap();
        let zones = ZoneSet::new(fixture.zones).unwrap();

        for case in fixture.cases {
            let location = Coordinates::new(case.latitude, case.longitude).unwrap();
            let selection = zones
                .select(location, case.horizontal_accuracy_meters)
                .unwrap();
            assert_eq!(selection.selected_zone, case.expected_zone, "{}", case.name);
        }
    }

    #[test]
    fn detects_two_meter_center_boundary() {
        let left = Zone {
            id: "left".into(),
            latitude: 0.0,
            longitude: 0.0,
            radius_meters: 10.0,
            passive: false,
        };
        let mut right = left.clone();
        right.id = "right".into();
        right.longitude = 0.000_01;
        assert!(zones_have_same_center(&left, &right).unwrap());
        right.longitude = 0.000_1;
        assert!(!zones_have_same_center(&left, &right).unwrap());
    }

    #[test]
    fn returns_the_closest_zone_independently_from_containment() {
        let zones = ZoneSet::new(vec![
            Zone {
                id: "far".into(),
                latitude: 10.1,
                longitude: 20.1,
                radius_meters: 100.0,
                passive: false,
            },
            Zone {
                id: "near".into(),
                latitude: 10.01,
                longitude: 20.01,
                radius_meters: 100.0,
                passive: false,
            },
        ])
        .unwrap();

        let closest = zones
            .closest(Coordinates::new(10.0, 20.0).unwrap())
            .unwrap()
            .unwrap();

        assert_eq!(closest.zone_id, "near");
        assert!(closest.distance_meters > 1_000.0);
    }
}
