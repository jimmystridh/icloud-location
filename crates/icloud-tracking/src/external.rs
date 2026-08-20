use chrono::{DateTime, Duration, Utc};
use icloud_location_core::{ExternalLocationUpdate, LocationSample};
use serde::{Deserialize, Serialize};

use crate::TrackingError;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalRejectionReason {
    Stale,
    PoorGps,
    Duplicate,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "decision", content = "details", rename_all = "snake_case")]
pub enum ArbitrationDecision {
    Accept(LocationSample),
    Reject(ExternalRejectionReason),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExternalArbitrationPolicy {
    pub gps_accuracy_threshold_meters: f64,
    pub minimum_newer_by_seconds: i64,
}

impl Default for ExternalArbitrationPolicy {
    fn default() -> Self {
        Self {
            gps_accuracy_threshold_meters: 100.0,
            minimum_newer_by_seconds: 5,
        }
    }
}

impl ExternalArbitrationPolicy {
    /// Selects an external sample only when it has acceptable accuracy and is
    /// sufficiently newer than the currently selected location.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid accuracy or freshness configuration.
    pub fn arbitrate(
        self,
        current: Option<&LocationSample>,
        update: &ExternalLocationUpdate,
    ) -> Result<ArbitrationDecision, TrackingError> {
        if !self.gps_accuracy_threshold_meters.is_finite()
            || self.gps_accuracy_threshold_meters < 0.0
            || self.minimum_newer_by_seconds < 0
            || update
                .sample
                .horizontal_accuracy_meters
                .is_some_and(|accuracy| !accuracy.is_finite() || accuracy < 0.0)
        {
            return Err(TrackingError::InvalidInput(
                "external arbitration thresholds must be non-negative and finite".into(),
            ));
        }
        if update
            .sample
            .horizontal_accuracy_meters
            .is_none_or(|accuracy| accuracy > self.gps_accuracy_threshold_meters)
        {
            return Ok(ArbitrationDecision::Reject(
                ExternalRejectionReason::PoorGps,
            ));
        }
        let Some(current) = current else {
            return Ok(ArbitrationDecision::Accept(update.sample.clone()));
        };
        let delta = update.sample.timestamp - current.timestamp;
        if delta.is_zero() && update.sample.coordinates == current.coordinates {
            return Ok(ArbitrationDecision::Reject(
                ExternalRejectionReason::Duplicate,
            ));
        }
        if delta.num_seconds() <= self.minimum_newer_by_seconds {
            return Ok(ArbitrationDecision::Reject(ExternalRejectionReason::Stale));
        }
        Ok(ArbitrationDecision::Accept(update.sample.clone()))
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExternalSourceHealth {
    pub last_update_at: Option<DateTime<Utc>>,
    pub last_request_at: Option<DateTime<Utc>>,
    pub consecutive_errors: u32,
}

impl ExternalSourceHealth {
    pub fn record_update(&mut self, timestamp: DateTime<Utc>) {
        if self
            .last_update_at
            .is_none_or(|current| timestamp > current)
        {
            self.last_update_at = Some(timestamp);
        }
        self.consecutive_errors = 0;
    }

    pub fn record_error(&mut self) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
    }

    #[must_use]
    pub fn is_healthy(&self, now: DateTime<Utc>, alive_interval: Duration) -> bool {
        self.last_update_at
            .is_some_and(|last| now.signed_duration_since(last) <= alive_interval)
    }

    #[must_use]
    pub fn can_request(&self, now: DateTime<Utc>, throttle: Duration) -> bool {
        self.last_request_at
            .is_none_or(|last| now.signed_duration_since(last) >= throttle)
    }

    pub fn record_request(&mut self, now: DateTime<Utc>) {
        self.last_request_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use icloud_location_core::{Coordinates, LocationSourceKind};

    #[test]
    fn accepts_fresh_fixture_update_and_rejects_old_poor_update() {
        let updates: Vec<ExternalLocationUpdate> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/external/location_updates.json"
        ))
        .unwrap();
        let mut updates = updates.into_iter();
        let fresh = updates.next().unwrap();
        let poor = updates.next().unwrap();
        let current = LocationSample {
            coordinates: Coordinates::new(10.123_456, 20.654_321).unwrap(),
            horizontal_accuracy_meters: Some(7.5),
            vertical_accuracy_meters: None,
            timestamp: Utc.timestamp_millis_opt(1_749_999_999_000).unwrap(),
            source: LocationSourceKind::Apple,
            is_old: false,
        };
        let policy = ExternalArbitrationPolicy::default();

        assert!(matches!(
            policy.arbitrate(Some(&current), &fresh).unwrap(),
            ArbitrationDecision::Accept(_)
        ));
        assert_eq!(
            policy.arbitrate(Some(&current), &poor).unwrap(),
            ArbitrationDecision::Reject(ExternalRejectionReason::PoorGps)
        );
        let mut invalid = fresh;
        invalid.sample.horizontal_accuracy_meters = Some(-1.0);
        assert!(policy.arbitrate(Some(&current), &invalid).is_err());
    }

    #[test]
    fn throttles_requests_and_tracks_source_health() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let mut health = ExternalSourceHealth::default();
        assert!(!health.is_healthy(now, Duration::minutes(5)));
        assert!(health.can_request(now, Duration::minutes(1)));

        health.record_update(now);
        health.record_request(now);
        assert!(health.is_healthy(now + Duration::minutes(4), Duration::minutes(5)));
        assert!(!health.can_request(now + Duration::seconds(30), Duration::minutes(1)));
        assert!(health.can_request(now + Duration::minutes(1), Duration::minutes(1)));
    }
}
