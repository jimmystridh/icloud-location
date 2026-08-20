use serde::{Deserialize, Serialize};

use crate::{Direction, TrackingError};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct DirectionInput {
    pub in_zone: bool,
    pub current_distance_km: f64,
    pub previous_distance_km: Option<f64>,
    pub current_travel_time_seconds: Option<f64>,
    pub previous_travel_time_seconds: Option<f64>,
    pub previous_direction: Direction,
    pub went_beyond_three_km: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DirectionDecision {
    pub direction: Direction,
    pub away_from_overridden: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DirectionObservation {
    pub direction: Direction,
    pub overridden: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DirectionHistory {
    observations: Vec<DirectionObservation>,
}

impl DirectionHistory {
    pub fn record(&mut self, decision: DirectionDecision) {
        self.observations.push(DirectionObservation {
            direction: decision.direction,
            overridden: decision.away_from_overridden,
        });
        if self.observations.len() > 30 {
            let remove = self.observations.len() - 30;
            self.observations.drain(..remove);
        }
    }

    #[must_use]
    pub fn observations(&self) -> &[DirectionObservation] {
        &self.observations
    }

    fn supports_close_range_override(&self) -> bool {
        let recent: Vec<_> = self.observations.iter().rev().take(3).collect();
        recent.len() == 3
            && recent
                .iter()
                .all(|observation| observation.direction == Direction::Towards)
            && recent.iter().any(|observation| !observation.overridden)
    }
}

/// Determines direction from route-time or straight-line distance deltas and
/// applies iCloud3's close-range away-from override.
///
/// # Errors
///
/// Returns an error for negative or non-finite distances and travel times.
pub fn determine_direction(
    input: DirectionInput,
    history: &DirectionHistory,
) -> Result<DirectionDecision, TrackingError> {
    validate_input(input)?;
    let mut direction = if input.in_zone {
        Direction::InZone
    } else if input.previous_distance_km.is_none() {
        Direction::Unknown
    } else if input.current_distance_km > 150.0 {
        Direction::FarAway
    } else {
        let distance_delta_meters = input
            .previous_distance_km
            .map(|previous| (input.current_distance_km - previous) * 1_000.0)
            .unwrap_or_default();
        let travel_delta_seconds = input
            .current_travel_time_seconds
            .zip(input.previous_travel_time_seconds)
            .map(|(current, previous)| current - previous)
            .unwrap_or_default();
        if distance_delta_meters <= -1.0 || travel_delta_seconds <= -1.0 {
            Direction::Towards
        } else if distance_delta_meters >= 1.0 || travel_delta_seconds >= 1.0 {
            Direction::AwayFrom
        } else {
            input.previous_direction
        }
    };

    let away_from_overridden = direction == Direction::AwayFrom
        && input.went_beyond_three_km
        && input.current_distance_km < 2.0
        && history.supports_close_range_override();
    if away_from_overridden {
        direction = Direction::Towards;
    }
    Ok(DirectionDecision {
        direction,
        away_from_overridden,
    })
}

fn validate_input(input: DirectionInput) -> Result<(), TrackingError> {
    let distances = [Some(input.current_distance_km), input.previous_distance_km];
    let travel_times = [
        input.current_travel_time_seconds,
        input.previous_travel_time_seconds,
    ];
    if distances
        .into_iter()
        .flatten()
        .chain(travel_times.into_iter().flatten())
        .any(|value| !value.is_finite() || value < 0.0)
    {
        return Err(TrackingError::InvalidInput(
            "direction distances and travel times must be finite and non-negative".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(current: f64, previous: f64) -> DirectionInput {
        DirectionInput {
            in_zone: false,
            current_distance_km: current,
            previous_distance_km: Some(previous),
            current_travel_time_seconds: None,
            previous_travel_time_seconds: None,
            previous_direction: Direction::Unknown,
            went_beyond_three_km: false,
        }
    }

    #[test]
    fn classifies_towards_away_stationary_and_far_away() {
        let history = DirectionHistory::default();
        assert_eq!(
            determine_direction(input(1.0, 2.0), &history)
                .unwrap()
                .direction,
            Direction::Towards
        );
        assert_eq!(
            determine_direction(input(2.0, 1.0), &history)
                .unwrap()
                .direction,
            Direction::AwayFrom
        );
        let mut stationary = input(1.000_4, 1.0);
        stationary.previous_direction = Direction::Stationary;
        assert_eq!(
            determine_direction(stationary, &history).unwrap().direction,
            Direction::Stationary
        );
        assert_eq!(
            determine_direction(input(151.0, 149.0), &history)
                .unwrap()
                .direction,
            Direction::FarAway
        );
    }

    #[test]
    fn overrides_one_close_range_away_sample_after_towards_history() {
        let mut history = DirectionHistory::default();
        for _ in 0..3 {
            history.record(DirectionDecision {
                direction: Direction::Towards,
                away_from_overridden: false,
            });
        }
        let mut close_away = input(1.6, 1.5);
        close_away.went_beyond_three_km = true;

        let decision = determine_direction(close_away, &history).unwrap();

        assert_eq!(decision.direction, Direction::Towards);
        assert!(decision.away_from_overridden);
    }
}
