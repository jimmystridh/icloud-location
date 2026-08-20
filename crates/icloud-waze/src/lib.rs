//! Optional Waze route provider kept behind the root crate's `waze` feature.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, fs::OpenOptions};

use icloud_location_core::BoxFuture;
use icloud_routing::{
    RouteEstimate, RouteHistoryEntry, RouteHistoryMaintenance, RouteHistoryQuery,
    RouteHistoryRecalculation, RouteHistoryStore, RouteProvider, RouteRequest, RouteStatus,
    RoutingError,
};
use reqwest::{Client, Url, header};
use rusqlite::{Connection, params};
use serde::Deserialize;
use thiserror::Error;

const USER_AGENT: &str = "Mozilla/5.0";
const WAZE_REFERER: &str = "https://routing-livemap-";
const REQUEST_ATTEMPTS: usize = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WazeRegion {
    America,
    Israel,
    #[default]
    RestOfWorld,
}

impl WazeRegion {
    #[must_use]
    pub fn from_icloud3_name(region: &str) -> Self {
        match region.to_ascii_lowercase().as_str() {
            "us" | "ca" | "na" | "am" => Self::America,
            "il" => Self::Israel,
            _ => Self::RestOfWorld,
        }
    }

    #[must_use]
    pub const fn server_code(self) -> &'static str {
        match self {
            Self::America => "am",
            Self::Israel => "il",
            Self::RestOfWorld => "row",
        }
    }

    /// Returns the undocumented endpoint used by the original library.
    ///
    /// # Errors
    ///
    /// Returns an error only if the built-in endpoint is no longer a valid URL.
    pub fn endpoint(self) -> Result<Url, WazeError> {
        Url::parse(&format!(
            "https://routing-livemap-{}.waze.com/RoutingManager/routingRequest",
            self.server_code()
        ))
        .map_err(WazeError::InvalidEndpoint)
    }
}

#[derive(Clone, Debug)]
pub struct WazeConfig {
    pub region: WazeRegion,
    pub real_time: bool,
    pub minimum_distance_km: f64,
    pub maximum_distance_km: f64,
    pub request_timeout: Duration,
}

impl Default for WazeConfig {
    fn default() -> Self {
        Self {
            region: WazeRegion::RestOfWorld,
            real_time: true,
            minimum_distance_km: 1.0,
            maximum_distance_km: 100.0,
            request_timeout: Duration::from_secs(60),
        }
    }
}

impl WazeConfig {
    /// Validates distance and timeout bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when distance bounds are negative or reversed, or the
    /// timeout is zero.
    pub fn validate(&self) -> Result<(), WazeError> {
        if !self.minimum_distance_km.is_finite()
            || !self.maximum_distance_km.is_finite()
            || self.minimum_distance_km < 0.0
            || self.maximum_distance_km < self.minimum_distance_km
        {
            return Err(WazeError::InvalidConfiguration(
                "Waze distance bounds must be finite, non-negative, and ordered".into(),
            ));
        }
        if self.request_timeout.is_zero() {
            return Err(WazeError::InvalidConfiguration(
                "Waze request timeout must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct WazeClient {
    client: Client,
    config: WazeConfig,
    endpoint: Url,
    availability: Arc<Mutex<WazeAvailability>>,
}

impl WazeClient {
    /// Builds a Waze route client with an explicit timeout and the request
    /// headers used by iCloud3.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, endpoint, or HTTP client
    /// settings.
    pub fn new(config: WazeConfig) -> Result<Self, WazeError> {
        let endpoint = config.region.endpoint()?;
        Self::with_endpoint(config, endpoint)
    }

    /// Builds a client targeting a supplied endpoint. This is public so the
    /// wire protocol can be tested without contacting Waze.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration or HTTP client settings.
    pub fn with_endpoint(config: WazeConfig, endpoint: Url) -> Result<Self, WazeError> {
        config.validate()?;
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::REFERER,
            header::HeaderValue::from_static(WAZE_REFERER),
        );
        let client = Client::builder()
            .default_headers(headers)
            .user_agent(USER_AGENT)
            .timeout(config.request_timeout)
            .build()
            .map_err(WazeError::HttpClient)?;
        Ok(Self {
            client,
            config,
            endpoint,
            availability: Arc::new(Mutex::new(WazeAvailability::default())),
        })
    }

    /// Pauses or resumes Waze requests without affecting straight-line routing.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal availability lock is poisoned.
    pub fn set_manual_pause(&self, paused: bool) -> Result<(), WazeError> {
        self.availability
            .lock()
            .map_err(|_| WazeError::AvailabilityLock)?
            .set_manual_pause(paused);
        Ok(())
    }

    /// Returns the current retry and pause state.
    ///
    /// # Errors
    ///
    /// Returns an error if the internal availability lock is poisoned.
    pub fn availability(&self) -> Result<WazeAvailability, WazeError> {
        self.availability
            .lock()
            .map(|availability| availability.clone())
            .map_err(|_| WazeError::AvailabilityLock)
    }

    async fn fetch(&self, request: &RouteRequest) -> Result<RouteEstimate, WazeError> {
        if self
            .availability
            .lock()
            .map_err(|_| WazeError::AvailabilityLock)?
            .is_paused(chrono::Utc::now())
        {
            return Ok(RouteEstimate {
                status: RouteStatus::Paused,
                distance_km: request.origin.distance_meters(request.destination) / 1000.0,
                duration_seconds: None,
                provider: "waze".into(),
            });
        }
        let direct_distance_km = request.origin.distance_meters(request.destination) / 1000.0;
        if direct_distance_km < self.config.minimum_distance_km
            || direct_distance_km > self.config.maximum_distance_km
        {
            return Ok(RouteEstimate {
                status: RouteStatus::OutOfRange,
                distance_km: direct_distance_km,
                duration_seconds: None,
                provider: "waze".into(),
            });
        }

        let query = [
            (
                "from",
                format!(
                    "x:{} y:{}",
                    request.origin.longitude, request.origin.latitude
                ),
            ),
            (
                "to",
                format!(
                    "x:{} y:{}",
                    request.destination.longitude, request.destination.latitude
                ),
            ),
            ("at", "0".into()),
            ("returnJSON", "true".into()),
            ("returnGeometries", "true".into()),
            ("returnInstructions", "true".into()),
            ("timeout", "60000".into()),
            ("nPaths", "1".into()),
            ("options", "AVOID_TRAILS:t".into()),
        ];

        let mut last_error = None;
        for _ in 0..REQUEST_ATTEMPTS {
            match self
                .client
                .get(self.endpoint.clone())
                .query(&query)
                .send()
                .await
            {
                Ok(response) => match response.error_for_status() {
                    Ok(response) => match response.json::<WazeEnvelope>().await {
                        Ok(envelope) => {
                            self.availability
                                .lock()
                                .map_err(|_| WazeError::AvailabilityLock)?
                                .record_success();
                            return envelope.estimate(self.config.real_time);
                        }
                        Err(error) => last_error = Some(WazeError::Response(error)),
                    },
                    Err(error) => last_error = Some(WazeError::Request(error)),
                },
                Err(error) => last_error = Some(WazeError::Request(error)),
            }
        }

        self.availability
            .lock()
            .map_err(|_| WazeError::AvailabilityLock)?
            .record_failure(chrono::Utc::now());
        Err(last_error.unwrap_or(WazeError::EmptyResponse))
    }
}

impl RouteProvider for WazeClient {
    fn route<'a>(
        &'a self,
        request: &'a RouteRequest,
    ) -> BoxFuture<'a, Result<RouteEstimate, RoutingError>> {
        Box::pin(async move {
            self.fetch(request)
                .await
                .map_err(|error| RoutingError::Provider(error.to_string()))
        })
    }
}

#[derive(Debug, Deserialize)]
struct WazeEnvelope {
    response: WazeResponse,
}

impl WazeEnvelope {
    fn estimate(self, real_time: bool) -> Result<RouteEstimate, WazeError> {
        let route = match self.response {
            WazeResponse::Route(route) => route,
            WazeResponse::Routes(mut routes) => {
                routes.drain(..).next().ok_or(WazeError::EmptyResponse)?
            }
        };
        if route.results.is_empty() {
            return Err(WazeError::EmptyResponse);
        }

        let mut distance_meters = 0.0;
        let mut duration_seconds = 0.0;
        for segment in route.results {
            distance_meters += segment.length;
            duration_seconds += segment.duration(real_time)?;
        }

        let duration = Duration::try_from_secs_f64(duration_seconds.round())
            .map_err(|error| WazeError::InvalidRoute(error.to_string()))?;
        Ok(RouteEstimate {
            status: RouteStatus::Used,
            distance_km: distance_meters / 1000.0,
            duration_seconds: Some(duration.as_secs()),
            provider: "waze".into(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WazeResponse {
    Route(WazeRoute),
    Routes(Vec<WazeRoute>),
}

#[derive(Debug, Deserialize)]
struct WazeRoute {
    results: Vec<WazeSegment>,
}

#[derive(Debug, Deserialize)]
struct WazeSegment {
    length: f64,
    #[serde(rename = "crossTime")]
    cross_time_camel: Option<f64>,
    cross_time: Option<f64>,
    #[serde(rename = "crossTimeWithoutRealTime")]
    historical_time_camel: Option<f64>,
    cross_time_without_real_time: Option<f64>,
}

impl WazeSegment {
    fn duration(&self, real_time: bool) -> Result<f64, WazeError> {
        let duration = if real_time {
            self.cross_time_camel.or(self.cross_time)
        } else {
            self.historical_time_camel
                .or(self.cross_time_without_real_time)
        };
        duration.ok_or(WazeError::MissingSegmentTime)
    }
}

#[derive(Debug, Error)]
pub enum WazeError {
    #[error("invalid Waze configuration: {0}")]
    InvalidConfiguration(String),
    #[error("invalid Waze endpoint: {0}")]
    InvalidEndpoint(url::ParseError),
    #[error("could not construct Waze HTTP client: {0}")]
    HttpClient(reqwest::Error),
    #[error("Waze request failed: {0}")]
    Request(reqwest::Error),
    #[error("could not decode Waze response: {0}")]
    Response(reqwest::Error),
    #[error("Waze returned no route")]
    EmptyResponse,
    #[error("Waze route segment did not contain the selected travel time")]
    MissingSegmentTime,
    #[error("Waze returned an invalid route: {0}")]
    InvalidRoute(String),
    #[error("Waze availability lock is poisoned")]
    AvailabilityLock,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WazeAvailability {
    pub consecutive_errors: u32,
    pub paused_until: Option<chrono::DateTime<chrono::Utc>>,
    pub manually_paused: bool,
}

impl WazeAvailability {
    pub fn record_failure(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        let seconds = match self.consecutive_errors {
            10 => Some(600),
            20 => Some(1_800),
            30 => Some(3_600),
            _ => None,
        };
        if let Some(seconds) = seconds {
            self.paused_until = Some(now + chrono::TimeDelta::seconds(seconds));
        }
        if self.consecutive_errors > 40 {
            self.consecutive_errors = 0;
            self.paused_until = None;
            self.manually_paused = true;
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive_errors = 0;
        self.paused_until = None;
    }

    pub fn set_manual_pause(&mut self, paused: bool) {
        self.manually_paused = paused;
    }

    #[must_use]
    pub fn is_paused(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.manually_paused || self.paused_until.is_some_and(|until| now < until)
    }
}

pub struct SqliteRouteHistoryStore {
    connection: Mutex<Connection>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RouteHistoryOrder {
    NorthSouth,
    #[default]
    EastWest,
}

impl SqliteRouteHistoryStore {
    /// Opens or creates a versioned `SQLite` route-history database.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot open or migrate the database.
    pub fn open(path: &Path) -> Result<Self, WazeHistoryError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        create_private_database_file(path)?;
        let connection = Connection::open(path)?;
        migrate_history(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Returns the number of stored route observations.
    ///
    /// # Errors
    ///
    /// Returns an error when the database lock or query fails.
    pub fn record_count(&self) -> Result<u64, WazeHistoryError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| WazeHistoryError::LockPoisoned)?;
        let count: i64 =
            connection.query_row("SELECT count(*) FROM routes", [], |row| row.get(0))?;
        u64::try_from(count).map_err(|error| WazeHistoryError::InvalidRecord(error.to_string()))
    }

    /// Lists stored routes in the north-south or east-west map order used by
    /// iCloud3's route-history inspection.
    ///
    /// # Errors
    ///
    /// Returns an error when the database lock or query fails.
    pub fn entries(
        &self,
        order: RouteHistoryOrder,
    ) -> Result<Vec<RouteHistoryEntry>, WazeHistoryError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| WazeHistoryError::LockPoisoned)?;
        let order_by = match order {
            RouteHistoryOrder::NorthSouth => "destination_latitude, destination_longitude, id",
            RouteHistoryOrder::EastWest => "destination_longitude, destination_latitude, id",
        };
        let mut statement = connection.prepare(&format!(
            "SELECT id, zone_id, destination_latitude, destination_longitude,
                    estimate_json, recorded_at, use_count
             FROM routes ORDER BY {order_by}"
        ))?;
        statement
            .query_map([], history_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn lookup_sync(
        &self,
        query: &RouteHistoryQuery,
    ) -> Result<Option<RouteHistoryEntry>, WazeHistoryError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| WazeHistoryError::LockPoisoned)?;
        let transaction = connection.transaction()?;
        let selected = {
            let mut statement = transaction.prepare(
                "SELECT id, zone_id, destination_latitude, destination_longitude,
                        estimate_json, recorded_at, use_count
                 FROM routes WHERE zone_id = ?1",
            )?;
            let rows = statement.query_map([&query.zone_id], history_row)?;
            let mut selected: Option<(RouteHistoryEntry, f64)> = None;
            for row in rows {
                let entry = row?;
                let distance = entry.destination.distance_meters(query.destination);
                if distance <= query.maximum_distance_meters
                    && selected
                        .as_ref()
                        .is_none_or(|(_, current_distance)| distance < *current_distance)
                {
                    selected = Some((entry, distance));
                }
            }
            selected.map(|(entry, _)| entry)
        };
        if let Some(entry) = &selected {
            transaction.execute(
                "UPDATE routes SET use_count = use_count + 1, last_used_at = ?1 WHERE id = ?2",
                params![chrono::Utc::now().to_rfc3339(), entry.id],
            )?;
        }
        transaction.commit()?;
        Ok(selected.map(|mut entry| {
            entry.use_count = entry.use_count.saturating_add(1);
            entry
        }))
    }

    fn store_sync(&self, entry: &RouteHistoryEntry) -> Result<RouteHistoryEntry, WazeHistoryError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| WazeHistoryError::LockPoisoned)?;
        let estimate_json = serde_json::to_string(&entry.estimate)?;
        let use_count = i64::try_from(entry.use_count.max(1))
            .map_err(|error| WazeHistoryError::InvalidRecord(error.to_string()))?;
        connection.execute(
            "INSERT INTO routes (
                zone_id, destination_latitude, destination_longitude,
                latitude_key, longitude_key, estimate_json, recorded_at,
                last_used_at, use_count
             ) VALUES (?1, ?2, ?3, round(?2, 4), round(?3, 4), ?4, ?5, ?5, ?6)",
            params![
                entry.zone_id,
                entry.destination.latitude,
                entry.destination.longitude,
                estimate_json,
                entry.recorded_at.to_rfc3339(),
                use_count,
            ],
        )?;
        let mut stored = entry.clone();
        stored.id = Some(connection.last_insert_rowid());
        stored.use_count = stored.use_count.max(1);
        Ok(stored)
    }

    fn maintain_sync(&self) -> Result<RouteHistoryMaintenance, WazeHistoryError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| WazeHistoryError::LockPoisoned)?;
        let before: i64 =
            connection.query_row("SELECT count(*) FROM routes", [], |row| row.get(0))?;
        connection.execute(
            "DELETE FROM routes
             WHERE id NOT IN (
                SELECT min(id) FROM routes GROUP BY zone_id, latitude_key, longitude_key
             )",
            [],
        )?;
        let after: i64 =
            connection.query_row("SELECT count(*) FROM routes", [], |row| row.get(0))?;
        connection.execute_batch("VACUUM")?;
        Ok(RouteHistoryMaintenance {
            removed_records: u64::try_from(before.saturating_sub(after)).unwrap_or_default(),
            updated_records: 0,
        })
    }

    fn update_estimate_sync(
        &self,
        id: i64,
        estimate: &RouteEstimate,
        recorded_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), WazeHistoryError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| WazeHistoryError::LockPoisoned)?;
        connection.execute(
            "UPDATE routes SET estimate_json = ?1, recorded_at = ?2 WHERE id = ?3",
            params![
                serde_json::to_string(estimate)?,
                recorded_at.to_rfc3339(),
                id
            ],
        )?;
        Ok(())
    }
}

fn create_private_database_file(path: &Path) -> Result<(), WazeHistoryError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl RouteHistoryStore for SqliteRouteHistoryStore {
    fn lookup<'a>(
        &'a self,
        query: &'a RouteHistoryQuery,
    ) -> BoxFuture<'a, Result<Option<RouteHistoryEntry>, RoutingError>> {
        Box::pin(async move {
            self.lookup_sync(query)
                .map_err(|error| RoutingError::History(error.to_string()))
        })
    }

    fn store<'a>(
        &'a self,
        entry: &'a RouteHistoryEntry,
    ) -> BoxFuture<'a, Result<RouteHistoryEntry, RoutingError>> {
        Box::pin(async move {
            self.store_sync(entry)
                .map_err(|error| RoutingError::History(error.to_string()))
        })
    }

    fn maintain(&self) -> BoxFuture<'_, Result<RouteHistoryMaintenance, RoutingError>> {
        Box::pin(async move {
            self.maintain_sync()
                .map_err(|error| RoutingError::History(error.to_string()))
        })
    }

    fn recalculate<'a>(
        &'a self,
        provider: &'a dyn RouteProvider,
        zone_origins: &'a BTreeMap<String, icloud_location_core::Coordinates>,
        departure: chrono::DateTime<chrono::Utc>,
    ) -> BoxFuture<'a, Result<RouteHistoryRecalculation, RoutingError>> {
        Box::pin(async move {
            let entries = self
                .entries(RouteHistoryOrder::EastWest)
                .map_err(|error| RoutingError::History(error.to_string()))?;
            let mut result = RouteHistoryRecalculation {
                examined_records: u64::try_from(entries.len()).unwrap_or(u64::MAX),
                ..RouteHistoryRecalculation::default()
            };
            for entry in entries {
                let (Some(id), Some(origin)) =
                    (entry.id, zone_origins.get(&entry.zone_id).copied())
                else {
                    result.failed_records = result.failed_records.saturating_add(1);
                    continue;
                };
                match provider
                    .route(&RouteRequest {
                        origin,
                        destination: entry.destination,
                        departure,
                    })
                    .await
                {
                    Ok(estimate) => {
                        self.update_estimate_sync(id, &estimate, departure)
                            .map_err(|error| RoutingError::History(error.to_string()))?;
                        result.updated_records = result.updated_records.saturating_add(1);
                    }
                    Err(_) => {
                        result.failed_records = result.failed_records.saturating_add(1);
                    }
                }
            }
            Ok(result)
        })
    }
}

fn migrate_history(connection: &Connection) -> Result<(), WazeHistoryError> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => {
            connection.execute_batch(
                "BEGIN;
                 CREATE TABLE routes (
                    id INTEGER PRIMARY KEY,
                    zone_id TEXT NOT NULL,
                    destination_latitude REAL NOT NULL,
                    destination_longitude REAL NOT NULL,
                    latitude_key REAL NOT NULL,
                    longitude_key REAL NOT NULL,
                    estimate_json TEXT NOT NULL,
                    recorded_at TEXT NOT NULL,
                    last_used_at TEXT NOT NULL,
                    use_count INTEGER NOT NULL DEFAULT 1
                 );
                 CREATE INDEX routes_zone_coordinates
                    ON routes(zone_id, latitude_key, longitude_key);
                 PRAGMA user_version = 1;
                 COMMIT;",
            )?;
            Ok(())
        }
        1 => Ok(()),
        version => Err(WazeHistoryError::UnsupportedVersion(version)),
    }
}

fn history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RouteHistoryEntry> {
    let latitude = row.get(2)?;
    let longitude = row.get(3)?;
    let coordinates =
        icloud_location_core::Coordinates::new(latitude, longitude).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Real,
                Box::new(error),
            )
        })?;
    let estimate_json: String = row.get(4)?;
    let estimate = serde_json::from_str(&estimate_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let recorded_at: String = row.get(5)?;
    let recorded_at = chrono::DateTime::parse_from_rfc3339(&recorded_at)
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?
        .with_timezone(&chrono::Utc);
    let use_count: i64 = row.get(6)?;
    let use_count = u64::try_from(use_count).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    Ok(RouteHistoryEntry {
        id: row.get(0)?,
        zone_id: row.get(1)?,
        destination: coordinates,
        estimate,
        recorded_at,
        use_count,
    })
}

#[derive(Debug, Error)]
pub enum WazeHistoryError {
    #[error("unsupported Waze history database version: {0}")]
    UnsupportedVersion(u32),
    #[error("Waze history database lock is poisoned")]
    LockPoisoned,
    #[error("invalid Waze history record: {0}")]
    InvalidRecord(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::Utc;
    use icloud_location_core::Coordinates;

    use super::*;

    fn fixture_estimate(real_time: bool) -> RouteEstimate {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/waze/route_response.json"
        ))
        .unwrap();
        serde_json::from_value::<WazeEnvelope>(fixture)
            .unwrap()
            .estimate(real_time)
            .unwrap()
    }

    #[test]
    fn maps_icloud3_regions() {
        assert_eq!(WazeRegion::from_icloud3_name("US"), WazeRegion::America);
        assert_eq!(WazeRegion::from_icloud3_name("na"), WazeRegion::America);
        assert_eq!(WazeRegion::from_icloud3_name("ca"), WazeRegion::America);
        assert_eq!(WazeRegion::from_icloud3_name("IL"), WazeRegion::Israel);
        assert_eq!(WazeRegion::from_icloud3_name("EU"), WazeRegion::RestOfWorld);
        assert!(
            WazeRegion::America
                .endpoint()
                .unwrap()
                .as_str()
                .contains("-am.")
        );
    }

    #[test]
    fn aggregates_camel_and_snake_case_segments() {
        let realtime = fixture_estimate(true);
        let historical = fixture_estimate(false);

        assert!((realtime.distance_km - 4.0).abs() < f64::EPSILON);
        assert_eq!(realtime.duration_seconds, Some(210));
        assert_eq!(historical.duration_seconds, Some(260));
    }

    #[tokio::test]
    async fn skips_server_request_outside_configured_range() {
        let client = WazeClient::new(WazeConfig {
            maximum_distance_km: 1.0,
            ..WazeConfig::default()
        })
        .unwrap();
        let request = RouteRequest {
            origin: Coordinates::new(0.0, 0.0).unwrap(),
            destination: Coordinates::new(0.0, 1.0).unwrap(),
            departure: Utc::now(),
        };

        let estimate = client.route(&request).await.unwrap();

        assert_eq!(estimate.status, RouteStatus::OutOfRange);
        assert!(estimate.duration_seconds.is_none());
    }

    #[tokio::test]
    async fn retries_waze_requests_three_times_and_preserves_query_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for attempt in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let (status, reason, body) = if attempt < 2 {
                    (500, "Server Error", "{}".to_owned())
                } else {
                    (
                        200,
                        "OK",
                        include_str!("../../../tests/fixtures/waze/route_response.json").to_owned(),
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        let client = WazeClient::with_endpoint(
            WazeConfig {
                minimum_distance_km: 0.0,
                maximum_distance_km: 1_000.0,
                ..WazeConfig::default()
            },
            Url::parse(&format!("http://{address}/routingRequest")).unwrap(),
        )
        .unwrap();
        let request = RouteRequest {
            origin: Coordinates::new(10.0, 20.0).unwrap(),
            destination: Coordinates::new(10.01, 20.01).unwrap(),
            departure: Utc::now(),
        };

        let estimate = client.route(&request).await.unwrap();
        let requests = server.join().unwrap();

        assert_eq!(estimate.duration_seconds, Some(210));
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /routingRequest?"));
        assert!(requests[0].contains("returnJSON=true"));
        assert!(requests[0].contains("AVOID_TRAILS"));
        assert_eq!(client.availability().unwrap().consecutive_errors, 0);
    }

    #[test]
    fn escalates_waze_pause_intervals_and_eventually_requires_manual_resume() {
        let now = Utc::now();
        let mut availability = WazeAvailability::default();
        for _ in 0..10 {
            availability.record_failure(now);
        }
        assert_eq!(
            availability.paused_until,
            Some(now + chrono::TimeDelta::minutes(10))
        );
        assert!(availability.is_paused(now + chrono::TimeDelta::minutes(9)));
        assert!(!availability.is_paused(now + chrono::TimeDelta::minutes(10)));

        availability.record_success();
        for _ in 0..41 {
            availability.record_failure(now);
        }
        assert!(availability.manually_paused);
        assert!(availability.is_paused(now + chrono::TimeDelta::days(1)));
    }

    #[tokio::test]
    async fn sqlite_history_round_trips_looks_up_and_compresses_duplicates() {
        let directory = std::env::temp_dir().join(format!(
            "icloud-waze-history-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("routes.sqlite3");
        let store = SqliteRouteHistoryStore::open(&path).unwrap();
        let destination = Coordinates::new(59.329_300_1, 18.068_600_1).unwrap();
        let entry = RouteHistoryEntry {
            id: None,
            zone_id: "home".into(),
            destination,
            estimate: RouteEstimate {
                status: RouteStatus::Used,
                distance_km: 4.0,
                duration_seconds: Some(210),
                provider: "waze".into(),
            },
            recorded_at: Utc::now(),
            use_count: 1,
        };
        let first = store.store(&entry).await.unwrap();
        let mut duplicate = entry.clone();
        duplicate.destination = Coordinates::new(59.329_300_2, 18.068_600_2).unwrap();
        store.store(&duplicate).await.unwrap();
        assert_eq!(store.record_count().unwrap(), 2);

        let found = store
            .lookup(&RouteHistoryQuery {
                zone_id: "home".into(),
                destination,
                maximum_distance_meters: 20.0,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, first.id);
        assert_eq!(found.use_count, 2);

        let maintenance = store.maintain().await.unwrap();
        assert_eq!(maintenance.removed_records, 1);
        assert_eq!(store.record_count().unwrap(), 1);
        drop(store);
        assert_eq!(
            SqliteRouteHistoryStore::open(&path)
                .unwrap()
                .record_count()
                .unwrap(),
            1
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_an_unsupported_history_database_version() {
        let directory = std::env::temp_dir().join(format!(
            "icloud-waze-future-history-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("routes.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);

        let Err(error) = SqliteRouteHistoryStore::open(&path) else {
            panic!("future database version should be rejected");
        };

        assert!(matches!(error, WazeHistoryError::UnsupportedVersion(99)));
        fs::remove_dir_all(directory).unwrap();
    }

    struct RecalculationProvider;

    impl RouteProvider for RecalculationProvider {
        fn route<'a>(
            &'a self,
            request: &'a RouteRequest,
        ) -> BoxFuture<'a, Result<RouteEstimate, RoutingError>> {
            Box::pin(async move {
                Ok(RouteEstimate {
                    status: RouteStatus::Used,
                    distance_km: request.origin.distance_meters(request.destination) / 1_000.0,
                    duration_seconds: Some(999),
                    provider: "recalculated".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn orders_and_recalculates_private_history_records() {
        let directory = std::env::temp_dir().join(format!(
            "icloud-waze-recalculate-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("routes.sqlite3");
        let store = SqliteRouteHistoryStore::open(&path).unwrap();
        for (zone_id, destination) in [
            ("north", Coordinates::new(60.0, 10.0).unwrap()),
            ("east", Coordinates::new(50.0, 20.0).unwrap()),
        ] {
            store
                .store(&RouteHistoryEntry {
                    id: None,
                    zone_id: zone_id.into(),
                    destination,
                    estimate: RouteEstimate {
                        status: RouteStatus::Used,
                        distance_km: 1.0,
                        duration_seconds: Some(60),
                        provider: "old".into(),
                    },
                    recorded_at: Utc::now(),
                    use_count: 1,
                })
                .await
                .unwrap();
        }
        assert_eq!(
            store.entries(RouteHistoryOrder::NorthSouth).unwrap()[0].zone_id,
            "east"
        );
        assert_eq!(
            store.entries(RouteHistoryOrder::EastWest).unwrap()[0].zone_id,
            "north"
        );
        let origins = BTreeMap::from([
            ("north".into(), Coordinates::new(59.0, 10.0).unwrap()),
            ("east".into(), Coordinates::new(50.0, 19.0).unwrap()),
        ]);

        let result = store
            .recalculate(&RecalculationProvider, &origins, Utc::now())
            .await
            .unwrap();

        assert_eq!(result.examined_records, 2);
        assert_eq!(result.updated_records, 2);
        assert_eq!(result.failed_records, 0);
        assert!(
            store
                .entries(RouteHistoryOrder::EastWest)
                .unwrap()
                .iter()
                .all(|entry| entry.estimate.duration_seconds == Some(999))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }
}
