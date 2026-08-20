//! Provider-neutral route calculation and history interfaces.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use icloud_location_core::{BoxFuture, Coordinates};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RouteRequest {
    pub origin: Coordinates,
    pub destination: Coordinates,
    pub departure: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStatus {
    Used,
    StraightLine,
    OutOfRange,
    Paused,
    NoData,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RouteEstimate {
    pub status: RouteStatus,
    pub distance_km: f64,
    pub duration_seconds: Option<u64>,
    pub provider: String,
}

pub trait RouteProvider: Send + Sync {
    fn route<'a>(
        &'a self,
        request: &'a RouteRequest,
    ) -> BoxFuture<'a, Result<RouteEstimate, RoutingError>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StraightLineRouteProvider;

impl RouteProvider for StraightLineRouteProvider {
    fn route<'a>(
        &'a self,
        request: &'a RouteRequest,
    ) -> BoxFuture<'a, Result<RouteEstimate, RoutingError>> {
        Box::pin(async move {
            Ok(RouteEstimate {
                status: RouteStatus::StraightLine,
                distance_km: request.origin.distance_meters(request.destination) / 1000.0,
                duration_seconds: None,
                provider: "straight_line".into(),
            })
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RouteHistoryEntry {
    pub id: Option<i64>,
    pub zone_id: String,
    pub destination: Coordinates,
    pub estimate: RouteEstimate,
    pub recorded_at: DateTime<Utc>,
    pub use_count: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RouteHistoryQuery {
    pub zone_id: String,
    pub destination: Coordinates,
    pub maximum_distance_meters: f64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteHistoryMaintenance {
    pub removed_records: u64,
    pub updated_records: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteHistoryRecalculation {
    pub examined_records: u64,
    pub updated_records: u64,
    pub failed_records: u64,
}

pub trait RouteHistoryStore: Send + Sync {
    fn lookup<'a>(
        &'a self,
        query: &'a RouteHistoryQuery,
    ) -> BoxFuture<'a, Result<Option<RouteHistoryEntry>, RoutingError>>;

    fn store<'a>(
        &'a self,
        entry: &'a RouteHistoryEntry,
    ) -> BoxFuture<'a, Result<RouteHistoryEntry, RoutingError>>;

    fn maintain(&self) -> BoxFuture<'_, Result<RouteHistoryMaintenance, RoutingError>>;

    fn recalculate<'a>(
        &'a self,
        provider: &'a dyn RouteProvider,
        zone_origins: &'a BTreeMap<String, Coordinates>,
        departure: DateTime<Utc>,
    ) -> BoxFuture<'a, Result<RouteHistoryRecalculation, RoutingError>> {
        let _ = (provider, zone_origins, departure);
        Box::pin(async {
            Err(RoutingError::History(
                "route-history recalculation is unsupported by this store".into(),
            ))
        })
    }
}

#[derive(Debug, Error)]
pub enum RoutingError {
    #[error("route provider failed: {0}")]
    Provider(String),

    #[error("route history failed: {0}")]
    History(String),

    #[error("invalid route response: {0}")]
    InvalidResponse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn straight_line_provider_returns_distance_without_duration() {
        let request = RouteRequest {
            origin: Coordinates::new(0.0, 0.0).unwrap(),
            destination: Coordinates::new(0.0, 1.0).unwrap(),
            departure: Utc::now(),
        };

        let estimate = StraightLineRouteProvider.route(&request).await.unwrap();

        assert_eq!(estimate.status, RouteStatus::StraightLine);
        assert!(estimate.duration_seconds.is_none());
        assert!((estimate.distance_km - 111.195_08).abs() < 0.001);
    }
}
