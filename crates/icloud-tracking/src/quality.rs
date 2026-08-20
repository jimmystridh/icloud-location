use std::time::Duration;

use chrono::{DateTime, Utc};
use icloud_location_core::LocationSample;
use serde::{Deserialize, Serialize};

use crate::TrackingError;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    OldLocation,
    PoorGps,
    OutOfOrder,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "quality", content = "reason", rename_all = "snake_case")]
pub enum LocationQuality {
    Good,
    Grace(RejectionReason),
    Rejected(RejectionReason),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocationQualityPolicy {
    pub gps_accuracy_threshold_meters: f64,
    pub old_location_threshold_seconds: u64,
    pub grace_updates: u32,
}

impl Default for LocationQualityPolicy {
    fn default() -> Self {
        Self {
            gps_accuracy_threshold_meters: 100.0,
            old_location_threshold_seconds: 120,
            grace_updates: 2,
        }
    }
}

impl LocationQualityPolicy {
    /// Classifies an incoming sample using iCloud3's age, GPS-accuracy, and
    /// initial-retry grace concepts.
    ///
    /// # Errors
    ///
    /// Returns an error when policy thresholds are invalid.
    pub fn evaluate(
        self,
        previous: Option<&LocationSample>,
        candidate: &LocationSample,
        now: DateTime<Utc>,
        consecutive_bad_updates: u32,
    ) -> Result<LocationQuality, TrackingError> {
        if !self.gps_accuracy_threshold_meters.is_finite()
            || self.gps_accuracy_threshold_meters < 0.0
            || candidate
                .horizontal_accuracy_meters
                .is_some_and(|accuracy| !accuracy.is_finite() || accuracy < 0.0)
        {
            return Err(TrackingError::InvalidInput(
                "GPS accuracy threshold must be finite and non-negative".into(),
            ));
        }
        let reason = if previous.is_some_and(|sample| candidate.timestamp < sample.timestamp) {
            Some(RejectionReason::OutOfOrder)
        } else if candidate.is_old
            || now.signed_duration_since(candidate.timestamp).num_seconds()
                > i64::try_from(self.old_location_threshold_seconds).unwrap_or(i64::MAX)
        {
            Some(RejectionReason::OldLocation)
        } else if candidate
            .horizontal_accuracy_meters
            .is_none_or(|accuracy| accuracy > self.gps_accuracy_threshold_meters)
        {
            Some(RejectionReason::PoorGps)
        } else {
            None
        };

        Ok(match reason {
            None => LocationQuality::Good,
            Some(RejectionReason::OutOfOrder) => {
                LocationQuality::Rejected(RejectionReason::OutOfOrder)
            }
            Some(reason) if consecutive_bad_updates < self.grace_updates => {
                LocationQuality::Grace(reason)
            }
            Some(reason) => LocationQuality::Rejected(reason),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct OldLocationContext {
    pub approaching_tracked_zone: bool,
    pub old_location_count: u32,
    pub tracked_interval_seconds: Vec<u64>,
    pub in_zone: bool,
    pub distance_from_zone_km: f64,
    pub pass_through_timer_active: bool,
    pub configured_maximum_seconds: Option<u64>,
    pub adjustment_seconds: i64,
}

/// Reproduces iCloud3's dynamic old-location threshold calculation.
///
/// # Errors
///
/// Returns an error for a non-finite or negative zone distance.
pub fn calculate_old_location_threshold(
    context: &OldLocationContext,
) -> Result<u64, TrackingError> {
    if !context.distance_from_zone_km.is_finite() || context.distance_from_zone_km < 0.0 {
        return Err(TrackingError::InvalidInput(
            "distance from zone must be finite and non-negative".into(),
        ));
    }
    let threshold = if context.approaching_tracked_zone && context.old_location_count <= 4 {
        30
    } else {
        let interval = context
            .tracked_interval_seconds
            .iter()
            .copied()
            .min()
            .unwrap_or(120);
        let mut threshold = if context.in_zone {
            Duration::from_secs(interval)
                .mul_f64(0.025)
                .as_secs()
                .max(120)
        } else if context.distance_from_zone_km > 5.0 {
            180
        } else if interval < 90 {
            60
        } else {
            Duration::from_secs(interval).mul_f64(0.125).as_secs()
        };
        threshold = if context.pass_through_timer_active {
            15
        } else {
            threshold.clamp(60, 600)
        };
        if let Some(maximum) = context.configured_maximum_seconds {
            threshold = threshold.min(maximum);
        }
        threshold
    };
    Ok(threshold.saturating_add_signed(context.adjustment_seconds))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use icloud_location_core::{Coordinates, LocationSourceKind};

    use super::*;

    fn sample(seconds: i64, accuracy: Option<f64>, is_old: bool) -> LocationSample {
        LocationSample {
            coordinates: Coordinates::new(10.0, 20.0).unwrap(),
            horizontal_accuracy_meters: accuracy,
            vertical_accuracy_meters: None,
            timestamp: Utc.timestamp_opt(seconds, 0).unwrap(),
            source: LocationSourceKind::Apple,
            is_old,
        }
    }

    #[test]
    fn gives_two_bad_updates_grace_before_rejection() {
        let policy = LocationQualityPolicy::default();
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let candidate = sample(800, Some(5.0), false);

        assert!(matches!(
            policy.evaluate(None, &candidate, now, 0).unwrap(),
            LocationQuality::Grace(RejectionReason::OldLocation)
        ));
        assert!(matches!(
            policy.evaluate(None, &candidate, now, 2).unwrap(),
            LocationQuality::Rejected(RejectionReason::OldLocation)
        ));
    }

    #[test]
    fn never_regresses_to_an_out_of_order_location() {
        let policy = LocationQualityPolicy::default();
        let current = sample(1_000, Some(5.0), false);
        let older = sample(999, Some(5.0), false);

        assert_eq!(
            policy
                .evaluate(Some(&current), &older, current.timestamp, 0)
                .unwrap(),
            LocationQuality::Rejected(RejectionReason::OutOfOrder)
        );
    }

    #[test]
    fn matches_old_location_threshold_branches() {
        let approaching = OldLocationContext {
            approaching_tracked_zone: true,
            ..OldLocationContext::default()
        };
        assert_eq!(calculate_old_location_threshold(&approaching).unwrap(), 30);

        let in_zone = OldLocationContext {
            tracked_interval_seconds: vec![7_200],
            in_zone: true,
            adjustment_seconds: 5,
            ..OldLocationContext::default()
        };
        assert_eq!(calculate_old_location_threshold(&in_zone).unwrap(), 185);

        let pass_through = OldLocationContext {
            pass_through_timer_active: true,
            ..OldLocationContext::default()
        };
        assert_eq!(calculate_old_location_threshold(&pass_through).unwrap(), 15);

        let configured_maximum = OldLocationContext {
            distance_from_zone_km: 10.0,
            configured_maximum_seconds: Some(90),
            ..OldLocationContext::default()
        };
        assert_eq!(
            calculate_old_location_threshold(&configured_maximum).unwrap(),
            90
        );
    }
}
