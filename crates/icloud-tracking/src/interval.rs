use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::TrackingError;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Towards,
    AwayFrom,
    Stationary,
    InZone,
    FarAway,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalReason {
    NeedInformation,
    NearAfterDriving,
    UnderTwoKilometers,
    UnderThreeAndHalfKilometers,
    UnderFiveKilometers,
    UnderEightKilometers,
    UnderTwelveKilometers,
    UnderTwentyKilometers,
    UnderFortyKilometers,
    OverOneHundredFiftyKilometers,
    CalculatedDistance,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IntervalDecision {
    pub seconds: u64,
    pub reason: IntervalReason,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IntervalPolicy;

impl IntervalPolicy {
    /// Reproduces iCloud3's distance-only interval branch.
    ///
    /// Higher-priority state such as zone changes, GPS errors, battery state,
    /// configured fixed intervals, and route time is applied by later policy layers.
    ///
    /// # Errors
    ///
    /// Returns an error when the distance is negative or non-finite.
    pub fn distance_interval(
        distance_km: f64,
        direction: Direction,
        went_beyond_three_km: bool,
    ) -> Result<IntervalDecision, TrackingError> {
        if !distance_km.is_finite() || distance_km < 0.0 {
            return Err(TrackingError::InvalidInput(format!(
                "distance must be finite and non-negative, got {distance_km}"
            )));
        }
        if direction == Direction::Unknown {
            return Ok(IntervalDecision {
                seconds: 150,
                reason: IntervalReason::NeedInformation,
            });
        }

        let (seconds, reason) = if distance_km < 2.0 && went_beyond_three_km {
            (15, IntervalReason::NearAfterDriving)
        } else if distance_km < 2.0 {
            (60, IntervalReason::UnderTwoKilometers)
        } else if distance_km < 3.5 {
            (90, IntervalReason::UnderThreeAndHalfKilometers)
        } else if distance_km < 5.0 {
            (120, IntervalReason::UnderFiveKilometers)
        } else if distance_km < 8.0 {
            (180, IntervalReason::UnderEightKilometers)
        } else if distance_km < 12.0 {
            (300, IntervalReason::UnderTwelveKilometers)
        } else if distance_km < 20.0 {
            (600, IntervalReason::UnderTwentyKilometers)
        } else if distance_km < 40.0 {
            (900, IntervalReason::UnderFortyKilometers)
        } else if distance_km > 150.0 {
            (3600, IntervalReason::OverOneHundredFiftyKilometers)
        } else {
            let miles = distance_km * 0.621_371;
            (
                Duration::try_from_secs_f64((miles * 0.5).round_ties_even() * 60.0)
                    .map_err(|error| TrackingError::InvalidInput(error.to_string()))?
                    .as_secs(),
                IntervalReason::CalculatedDistance,
            )
        };

        Ok(IntervalDecision { seconds, reason })
    }
}

#[must_use]
pub fn retry_interval_seconds(error_count: u32) -> u64 {
    match error_count {
        0..=1 => 5,
        2..=3 => 15,
        4..=7 => 30,
        8..=11 => 60,
        12..=15 => 300,
        16..=19 => 900,
        20..=23 => 1800,
        _ => 3600,
    }
}

#[must_use]
pub fn offline_interval_seconds(location_age_seconds: u64) -> u64 {
    if location_age_seconds > 3_600 {
        3_600
    } else if location_age_seconds > 1_800 {
        1_800
    } else if location_age_seconds > 180 {
        300
    } else {
        180
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackingIntervalReason {
    StateChange,
    EnterZone,
    ExitZone,
    ZoneChange,
    ExternalExitTrigger,
    OldLocation,
    PoorGps,
    LowBattery,
    AtHome,
    InZone,
    NeedInformation,
    AwayCloseToZone,
    Distance,
    WazeTravelTime,
    StationaryZone,
    Fixed,
    Maximum,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct TrackingIntervalContext {
    pub state_changed: bool,
    pub in_zone: bool,
    pub was_in_zone: bool,
    pub in_stationary_zone: bool,
    pub stationary_zone_is_small: bool,
    pub external_exit_trigger_recent: bool,
    pub external_update: bool,
    pub location_old: bool,
    pub gps_poor: bool,
    pub location_good: bool,
    pub offline: bool,
    pub pass_through_timer_active: bool,
    pub battery_percent: Option<u8>,
    pub distance_from_zone_km: f64,
    pub direction: Direction,
    pub went_beyond_three_km: bool,
    pub waze_enabled: bool,
    pub waze_travel_seconds: Option<u64>,
    pub travel_time_factor: f64,
    pub in_zone_interval_seconds: u64,
    pub stationary_interval_seconds: u64,
    pub exit_zone_interval_seconds: u64,
    pub old_location_threshold_seconds: u64,
    pub fixed_interval_seconds: u64,
    pub maximum_interval_seconds: u64,
    pub error_count: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrackingIntervalDecision {
    pub seconds: u64,
    pub reason: TrackingIntervalReason,
}

impl IntervalPolicy {
    /// Applies iCloud3's higher-priority state, quality, battery, zone, Waze,
    /// fixed-interval, and maximum-interval layers around distance policy.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid distance, travel factor, or zero required
    /// intervals.
    #[allow(clippy::too_many_lines)]
    pub fn determine(
        context: TrackingIntervalContext,
    ) -> Result<TrackingIntervalDecision, TrackingError> {
        validate_tracking_context(context)?;
        let retry = retry_interval_seconds(context.error_count);
        let waze_seconds = context.waze_travel_seconds.unwrap_or_default();
        let (mut seconds, mut reason) = if context.state_changed {
            if context.in_zone && (context.location_old || context.gps_poor) {
                (retry, TrackingIntervalReason::OldLocation)
            } else if context.in_zone && !context.in_stationary_zone {
                (
                    context.in_zone_interval_seconds,
                    TrackingIntervalReason::EnterZone,
                )
            } else if context.battery_percent.is_some_and(|battery| battery <= 5)
                && context.distance_from_zone_km <= 1.0
            {
                (15, TrackingIntervalReason::LowBattery)
            } else if context.battery_percent.is_some_and(|battery| battery <= 10) {
                (
                    context.stationary_interval_seconds,
                    TrackingIntervalReason::LowBattery,
                )
            } else if !context.in_zone && context.was_in_zone {
                (
                    context.exit_zone_interval_seconds,
                    TrackingIntervalReason::ExitZone,
                )
            } else {
                (240, TrackingIntervalReason::ZoneChange)
            }
        } else if context.external_exit_trigger_recent {
            (
                context.exit_zone_interval_seconds,
                TrackingIntervalReason::ExternalExitTrigger,
            )
        } else if context.gps_poor {
            (retry, TrackingIntervalReason::PoorGps)
        } else if context.location_old {
            (retry, TrackingIntervalReason::OldLocation)
        } else if context.battery_percent.is_some_and(|battery| battery <= 10)
            && context.distance_from_zone_km > 1.0
        {
            (
                context.stationary_interval_seconds,
                TrackingIntervalReason::LowBattery,
            )
        } else if context.distance_from_zone_km < 0.05
            && (context.in_zone || context.direction == Direction::Towards)
        {
            (
                context.in_zone_interval_seconds,
                TrackingIntervalReason::AtHome,
            )
        } else if context.in_zone && context.in_zone_interval_seconds > waze_seconds {
            (
                context.in_zone_interval_seconds,
                TrackingIntervalReason::InZone,
            )
        } else if context.direction == Direction::Unknown {
            (150, TrackingIntervalReason::NeedInformation)
        } else if context.distance_from_zone_km < 2.0
            && context.direction == Direction::AwayFrom
            && waze_seconds > 0
        {
            (
                context.old_location_threshold_seconds,
                TrackingIntervalReason::AwayCloseToZone,
            )
        } else if context.distance_from_zone_km < 3.5 {
            let decision = Self::distance_interval(
                context.distance_from_zone_km,
                context.direction,
                context.went_beyond_three_km,
            )?;
            (decision.seconds, TrackingIntervalReason::Distance)
        } else if waze_seconds > 300 {
            (
                scaled_seconds(waze_seconds, context.travel_time_factor),
                TrackingIntervalReason::WazeTravelTime,
            )
        } else {
            let decision = Self::distance_interval(
                context.distance_from_zone_km,
                context.direction,
                context.went_beyond_three_km,
            )?;
            (decision.seconds, TrackingIntervalReason::Distance)
        };

        if context.direction == Direction::AwayFrom
            && context.distance_from_zone_km >= 3.0
            && !context.waze_enabled
            && context.fixed_interval_seconds == 0
            && seconds > 60
        {
            seconds = seconds.saturating_mul(2);
        } else if context.direction == Direction::Unknown && seconds > 180 {
            seconds = 180;
        }
        if context.external_update && (31..180).contains(&seconds) {
            seconds = 180;
        }

        if context.in_stationary_zone {
            seconds = if context.stationary_zone_is_small {
                300
            } else {
                context.stationary_interval_seconds
            };
            reason = TrackingIntervalReason::StationaryZone;
        } else if context.fixed_interval_seconds >= 300
            && seconds > 300
            && !context.in_zone
            && context.location_good
            && !context.offline
            && !context.pass_through_timer_active
        {
            seconds = context.fixed_interval_seconds;
            reason = TrackingIntervalReason::Fixed;
        } else if seconds > context.maximum_interval_seconds
            && !context.in_zone
            && context.location_good
            && !context.offline
            && !context.pass_through_timer_active
        {
            seconds = context.maximum_interval_seconds;
            reason = TrackingIntervalReason::Maximum;
        }

        if matches!(context.direction, Direction::AwayFrom | Direction::Towards) && waze_seconds > 0
        {
            let upper = scaled_seconds(waze_seconds, context.travel_time_factor * 1.5);
            if seconds > upper {
                seconds = scaled_seconds(waze_seconds, context.travel_time_factor);
                reason = TrackingIntervalReason::WazeTravelTime;
            }
        }
        seconds = (seconds / 5 * 5).max(5);
        Ok(TrackingIntervalDecision { seconds, reason })
    }
}

fn scaled_seconds(seconds: u64, factor: f64) -> u64 {
    Duration::from_secs(seconds).mul_f64(factor).as_secs()
}

fn validate_tracking_context(context: TrackingIntervalContext) -> Result<(), TrackingError> {
    if !context.distance_from_zone_km.is_finite()
        || context.distance_from_zone_km < 0.0
        || !context.travel_time_factor.is_finite()
        || context.travel_time_factor <= 0.0
        || context.in_zone_interval_seconds == 0
        || context.stationary_interval_seconds == 0
        || context.exit_zone_interval_seconds == 0
        || context.maximum_interval_seconds == 0
    {
        return Err(TrackingError::InvalidInput(
            "tracking interval context contains invalid distances or intervals".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Fixture {
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        distance_km: f64,
        went_beyond_three_km: bool,
        direction_known: bool,
        expected_seconds: u64,
    }

    #[test]
    fn matches_distance_interval_fixture() {
        let fixture: Fixture = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tracking/interval_cases.json"
        ))
        .unwrap();

        for case in fixture.cases {
            let direction = if case.direction_known {
                Direction::Towards
            } else {
                Direction::Unknown
            };
            let result = IntervalPolicy::distance_interval(
                case.distance_km,
                direction,
                case.went_beyond_three_km,
            )
            .unwrap();
            assert_eq!(result.seconds, case.expected_seconds, "{}", case.name);
        }
    }

    #[test]
    fn matches_error_retry_boundaries() {
        assert_eq!(retry_interval_seconds(0), 5);
        assert_eq!(retry_interval_seconds(1), 5);
        assert_eq!(retry_interval_seconds(2), 15);
        assert_eq!(retry_interval_seconds(4), 30);
        assert_eq!(retry_interval_seconds(8), 60);
        assert_eq!(retry_interval_seconds(12), 300);
        assert_eq!(retry_interval_seconds(16), 900);
        assert_eq!(retry_interval_seconds(20), 1800);
        assert_eq!(retry_interval_seconds(24), 3600);
        assert_eq!(offline_interval_seconds(180), 180);
        assert_eq!(offline_interval_seconds(181), 300);
        assert_eq!(offline_interval_seconds(1_801), 1_800);
        assert_eq!(offline_interval_seconds(3_601), 3_600);
    }

    fn tracking_context() -> TrackingIntervalContext {
        TrackingIntervalContext {
            state_changed: false,
            in_zone: false,
            was_in_zone: false,
            in_stationary_zone: false,
            stationary_zone_is_small: false,
            external_exit_trigger_recent: false,
            external_update: false,
            location_old: false,
            gps_poor: false,
            location_good: true,
            offline: false,
            pass_through_timer_active: false,
            battery_percent: Some(80),
            distance_from_zone_km: 10.0,
            direction: Direction::Towards,
            went_beyond_three_km: true,
            waze_enabled: false,
            waze_travel_seconds: None,
            travel_time_factor: 0.5,
            in_zone_interval_seconds: 120,
            stationary_interval_seconds: 300,
            exit_zone_interval_seconds: 30,
            old_location_threshold_seconds: 60,
            fixed_interval_seconds: 0,
            maximum_interval_seconds: 7_200,
            error_count: 0,
        }
    }

    #[test]
    fn applies_quality_zone_fixed_maximum_and_waze_priorities() {
        let mut context = tracking_context();
        context.gps_poor = true;
        context.error_count = 12;
        assert_eq!(
            IntervalPolicy::determine(context).unwrap(),
            TrackingIntervalDecision {
                seconds: 300,
                reason: TrackingIntervalReason::PoorGps,
            }
        );

        context = tracking_context();
        context.in_stationary_zone = true;
        context.stationary_zone_is_small = true;
        assert_eq!(IntervalPolicy::determine(context).unwrap().seconds, 300);

        context = tracking_context();
        context.distance_from_zone_km = 100.0;
        context.fixed_interval_seconds = 600;
        assert_eq!(
            IntervalPolicy::determine(context).unwrap().reason,
            TrackingIntervalReason::Fixed
        );

        context = tracking_context();
        context.distance_from_zone_km = 100.0;
        context.maximum_interval_seconds = 600;
        assert_eq!(
            IntervalPolicy::determine(context).unwrap().reason,
            TrackingIntervalReason::Maximum
        );

        context = tracking_context();
        context.distance_from_zone_km = 10.0;
        context.waze_enabled = true;
        context.waze_travel_seconds = Some(600);
        assert_eq!(
            IntervalPolicy::determine(context).unwrap(),
            TrackingIntervalDecision {
                seconds: 300,
                reason: TrackingIntervalReason::WazeTravelTime,
            }
        );

        context = tracking_context();
        context.state_changed = true;
        context.was_in_zone = true;
        context.exit_zone_interval_seconds = 1;
        assert_eq!(IntervalPolicy::determine(context).unwrap().seconds, 5);
    }
}
