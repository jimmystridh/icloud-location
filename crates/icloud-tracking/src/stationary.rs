use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use icloud_location_core::{Coordinates, TrackingEvent};
use serde::{Deserialize, Serialize};

use crate::{TrackingError, ZoneSet};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct StationaryPolicy {
    pub enabled: bool,
    pub still_seconds: u64,
    pub movement_limit_meters: f64,
    pub radius_meters: f64,
    pub reuse_cooldown_seconds: u64,
}

impl Default for StationaryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            still_seconds: 1_800,
            movement_limit_meters: 60.0,
            radius_meters: 100.0,
            reuse_cooldown_seconds: 300,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StationaryZone {
    pub id: String,
    pub center: Coordinates,
    pub radius_meters: f64,
    pub active: bool,
    pub occupants: BTreeSet<String>,
    pub last_removed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug)]
pub struct StationaryObservation<'a> {
    pub device_id: &'a str,
    pub location: Coordinates,
    pub observed_at: DateTime<Utc>,
    pub distance_moved_meters: f64,
    pub location_is_good: bool,
    pub monitored_only: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct StationaryZoneManager {
    pub zones: Vec<StationaryZone>,
    device_zones: BTreeMap<String, String>,
    still_since: BTreeMap<String, DateTime<Utc>>,
    next_id: u64,
}

impl StationaryZoneManager {
    #[must_use]
    pub fn device_zone(&self, device_id: &str) -> Option<&str> {
        self.device_zones.get(device_id).map(String::as_str)
    }

    /// Applies one movement observation and creates, joins, exits, or reuses a
    /// platform-neutral stationary zone when required.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid movement, policy, or regular-zone geometry.
    pub fn observe(
        &mut self,
        observation: StationaryObservation<'_>,
        policy: StationaryPolicy,
        regular_zones: &ZoneSet,
    ) -> Result<Vec<TrackingEvent>, TrackingError> {
        validate(policy, observation.distance_moved_meters)?;
        if let Some(zone_id) = self.device_zones.get(observation.device_id).cloned() {
            return self.observe_existing(observation, &zone_id);
        }
        if !policy.enabled || !observation.location_is_good || observation.monitored_only {
            self.still_since.remove(observation.device_id);
            return Ok(Vec::new());
        }
        if regular_zones
            .select(observation.location, 0.0)?
            .selected_zone
            .is_some()
        {
            self.still_since.remove(observation.device_id);
            return Ok(Vec::new());
        }
        if observation.distance_moved_meters > policy.movement_limit_meters {
            self.still_since
                .insert(observation.device_id.into(), observation.observed_at);
            return Ok(Vec::new());
        }
        let still_since = *self
            .still_since
            .entry(observation.device_id.into())
            .or_insert(observation.observed_at);
        let still_seconds = i64::try_from(policy.still_seconds).unwrap_or(i64::MAX);
        if observation.observed_at - still_since < Duration::seconds(still_seconds) {
            return Ok(Vec::new());
        }

        let joinable = self.zones.iter().position(|zone| {
            zone.active
                && zone.center.distance_meters(observation.location) <= zone.radius_meters * 1.5
        });
        let zone_index = if let Some(index) = joinable {
            index
        } else if let Some(index) = self.reusable_zone(observation.observed_at, policy) {
            let zone = &mut self.zones[index];
            zone.center = observation.location;
            zone.radius_meters = policy.radius_meters;
            zone.active = true;
            zone.last_removed_at = None;
            index
        } else {
            self.next_id = self.next_id.saturating_add(1);
            self.zones.push(StationaryZone {
                id: format!("stationary_{}", self.next_id),
                center: observation.location,
                radius_meters: policy.radius_meters,
                active: true,
                occupants: BTreeSet::new(),
                last_removed_at: None,
            });
            self.zones.len() - 1
        };
        let zone = &mut self.zones[zone_index];
        let created_or_reused = zone.occupants.is_empty();
        zone.occupants.insert(observation.device_id.into());
        let zone_id = zone.id.clone();
        self.device_zones
            .insert(observation.device_id.into(), zone_id.clone());
        self.still_since.remove(observation.device_id);
        let mut events = Vec::new();
        if created_or_reused {
            events.push(TrackingEvent::StationaryZoneCreated {
                zone_id: zone_id.clone(),
                device_id: observation.device_id.into(),
            });
        }
        events.push(TrackingEvent::ZoneEntered {
            device_id: observation.device_id.into(),
            zone_id,
        });
        Ok(events)
    }

    /// Moves an active stationary zone to its owning device's current location.
    ///
    /// # Errors
    ///
    /// Returns an error when the device is not assigned to a stationary zone.
    pub fn move_to_device(
        &mut self,
        device_id: &str,
        location: Coordinates,
    ) -> Result<TrackingEvent, TrackingError> {
        let zone_id = self
            .device_zones
            .get(device_id)
            .ok_or_else(|| TrackingError::InvalidInput("device has no stationary zone".into()))?;
        let zone = self
            .zones
            .iter_mut()
            .find(|zone| zone.id == *zone_id)
            .ok_or_else(|| TrackingError::InvalidInput("stationary zone is missing".into()))?;
        zone.center = location;
        Ok(TrackingEvent::StationaryZoneMoved {
            zone_id: zone.id.clone(),
            device_id: device_id.into(),
        })
    }

    fn observe_existing(
        &mut self,
        observation: StationaryObservation<'_>,
        zone_id: &str,
    ) -> Result<Vec<TrackingEvent>, TrackingError> {
        let zone = self
            .zones
            .iter_mut()
            .find(|zone| zone.id == zone_id)
            .ok_or_else(|| TrackingError::InvalidInput("stationary zone is missing".into()))?;
        if zone.center.distance_meters(observation.location) <= zone.radius_meters * 1.5 {
            return Ok(Vec::new());
        }

        zone.occupants.remove(observation.device_id);
        self.device_zones.remove(observation.device_id);
        self.still_since
            .insert(observation.device_id.into(), observation.observed_at);
        let mut events = vec![TrackingEvent::ZoneExited {
            device_id: observation.device_id.into(),
            zone_id: zone.id.clone(),
        }];
        if zone.occupants.is_empty() {
            zone.active = false;
            zone.last_removed_at = Some(observation.observed_at);
            events.push(TrackingEvent::StationaryZoneRemoved {
                zone_id: zone.id.clone(),
            });
        }
        Ok(events)
    }

    fn reusable_zone(&self, now: DateTime<Utc>, policy: StationaryPolicy) -> Option<usize> {
        let cooldown = i64::try_from(policy.reuse_cooldown_seconds).unwrap_or(i64::MAX);
        self.zones.iter().position(|zone| {
            !zone.active
                && zone
                    .last_removed_at
                    .is_some_and(|removed| now - removed >= Duration::seconds(cooldown))
        })
    }
}

fn validate(policy: StationaryPolicy, distance_moved_meters: f64) -> Result<(), TrackingError> {
    if !policy.movement_limit_meters.is_finite()
        || !policy.radius_meters.is_finite()
        || policy.movement_limit_meters < 0.0
        || policy.radius_meters <= 0.0
        || !distance_moved_meters.is_finite()
        || distance_moved_meters < 0.0
    {
        return Err(TrackingError::InvalidInput(
            "stationary-zone distances must be finite and valid".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn observation(
        device_id: &str,
        location: Coordinates,
        observed_at: DateTime<Utc>,
    ) -> StationaryObservation<'_> {
        StationaryObservation {
            device_id,
            location,
            observed_at,
            distance_moved_meters: 0.0,
            location_is_good: true,
            monitored_only: false,
        }
    }

    #[test]
    fn creates_exits_removes_and_reuses_stationary_zone() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let location = Coordinates::new(10.0, 20.0).unwrap();
        let far = Coordinates::new(10.01, 20.01).unwrap();
        let policy = StationaryPolicy {
            still_seconds: 60,
            reuse_cooldown_seconds: 30,
            ..StationaryPolicy::default()
        };
        let regular_zones = ZoneSet::default();
        let mut manager = StationaryZoneManager::default();

        assert!(
            manager
                .observe(observation("one", location, now), policy, &regular_zones)
                .unwrap()
                .is_empty()
        );
        let entered = manager
            .observe(
                observation("one", location, now + Duration::seconds(60)),
                policy,
                &regular_zones,
            )
            .unwrap();
        assert!(matches!(
            entered[0],
            TrackingEvent::StationaryZoneCreated { .. }
        ));
        assert!(manager.zones[0].active);
        let moved = manager
            .move_to_device("one", Coordinates::new(10.000_1, 20.000_1).unwrap())
            .unwrap();
        assert!(matches!(moved, TrackingEvent::StationaryZoneMoved { .. }));

        let exited = manager
            .observe(
                observation("one", far, now + Duration::seconds(61)),
                policy,
                &regular_zones,
            )
            .unwrap();
        assert!(matches!(exited[0], TrackingEvent::ZoneExited { .. }));
        assert!(matches!(
            exited[1],
            TrackingEvent::StationaryZoneRemoved { .. }
        ));

        manager
            .observe(
                observation("two", far, now + Duration::seconds(100)),
                policy,
                &regular_zones,
            )
            .unwrap();
        manager
            .observe(
                observation("two", far, now + Duration::seconds(160)),
                policy,
                &regular_zones,
            )
            .unwrap();
        assert_eq!(manager.zones.len(), 1);
        assert_eq!(manager.zones[0].occupants, BTreeSet::from(["two".into()]));
    }
}
