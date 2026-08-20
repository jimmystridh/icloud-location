use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use icloud_location_core::TrackingEvent;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PassThroughPolicy {
    pub delay_seconds: u64,
    pub tracked_from_zones: BTreeSet<String>,
    pub stationary_zone_prefix: String,
}

impl PassThroughPolicy {
    fn should_delay(&self, zone_id: &str) -> bool {
        self.delay_seconds > 0
            && !self.tracked_from_zones.contains(zone_id)
            && (self.stationary_zone_prefix.is_empty()
                || !zone_id.starts_with(&self.stationary_zone_prefix))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingZone {
    pub zone_id: String,
    pub confirm_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ZoneTransitionState {
    pub current_zone: Option<String>,
    pub entered_at: Option<DateTime<Utc>>,
    pub last_exited_zone: Option<String>,
    pub exited_at: Option<DateTime<Utc>>,
    pub pending_zone: Option<PendingZone>,
}

impl ZoneTransitionState {
    #[must_use]
    pub fn update(
        &mut self,
        device_id: &str,
        selected_zone: Option<&str>,
        now: DateTime<Utc>,
        pass_through: &PassThroughPolicy,
    ) -> Vec<TrackingEvent> {
        if self.current_zone.as_deref() == selected_zone {
            self.pending_zone = None;
            return Vec::new();
        }

        if self.current_zone.is_none() {
            if let Some(zone_id) = selected_zone {
                if pass_through.should_delay(zone_id) {
                    match &self.pending_zone {
                        Some(pending)
                            if pending.zone_id == zone_id && now >= pending.confirm_at => {}
                        Some(pending) if pending.zone_id == zone_id => return Vec::new(),
                        _ => {
                            let delay =
                                i64::try_from(pass_through.delay_seconds).unwrap_or(i64::MAX);
                            self.pending_zone = Some(PendingZone {
                                zone_id: zone_id.into(),
                                confirm_at: now + Duration::seconds(delay),
                            });
                            return Vec::new();
                        }
                    }
                }
            } else {
                self.pending_zone = None;
                return Vec::new();
            }
        }

        self.pending_zone = None;
        let mut events = Vec::new();
        if let Some(previous) = self.current_zone.take() {
            self.last_exited_zone = Some(previous.clone());
            self.exited_at = Some(now);
            events.push(TrackingEvent::ZoneExited {
                device_id: device_id.into(),
                zone_id: previous,
            });
        }
        if let Some(zone_id) = selected_zone {
            self.current_zone = Some(zone_id.into());
            self.entered_at = Some(now);
            events.push(TrackingEvent::ZoneEntered {
                device_id: device_id.into(),
                zone_id: zone_id.into(),
            });
        }
        events
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ZoneOccupancy {
    device_zones: BTreeMap<String, String>,
}

impl ZoneOccupancy {
    pub fn update(&mut self, device_id: &str, zone_id: Option<&str>) {
        if let Some(zone_id) = zone_id {
            self.device_zones.insert(device_id.into(), zone_id.into());
        } else {
            self.device_zones.remove(device_id);
        }
    }

    #[must_use]
    pub fn counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for zone_id in self.device_zones.values() {
            *counts.entry(zone_id.clone()).or_default() += 1;
        }
        counts
    }

    #[must_use]
    pub fn devices_in(&self, zone_id: &str) -> Vec<&str> {
        self.device_zones
            .iter()
            .filter_map(|(device_id, current_zone)| {
                (current_zone == zone_id).then_some(device_id.as_str())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn delays_pass_through_zone_but_enters_tracked_zone_immediately() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let policy = PassThroughPolicy {
            delay_seconds: 60,
            tracked_from_zones: BTreeSet::from(["home".into()]),
            stationary_zone_prefix: "stationary_".into(),
        };
        let mut state = ZoneTransitionState::default();

        assert!(
            state
                .update("device", Some("shops"), now, &policy)
                .is_empty()
        );
        assert!(
            state
                .update(
                    "device",
                    Some("shops"),
                    now + Duration::seconds(59),
                    &policy
                )
                .is_empty()
        );
        let events = state.update(
            "device",
            Some("shops"),
            now + Duration::seconds(60),
            &policy,
        );
        assert!(matches!(events[0], TrackingEvent::ZoneEntered { .. }));

        let exit = state.update("device", None, now + Duration::seconds(61), &policy);
        assert!(matches!(exit[0], TrackingEvent::ZoneExited { .. }));
        let enter = state.update("device", Some("home"), now + Duration::seconds(62), &policy);
        assert!(matches!(enter[0], TrackingEvent::ZoneEntered { .. }));
    }

    #[test]
    fn reports_zone_device_counts() {
        let mut occupancy = ZoneOccupancy::default();
        occupancy.update("one", Some("home"));
        occupancy.update("two", Some("home"));
        occupancy.update("three", Some("work"));

        assert_eq!(occupancy.counts()["home"], 2);
        assert_eq!(occupancy.devices_in("work"), ["three"]);
    }
}
