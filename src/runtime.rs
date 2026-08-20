use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use icloud_location_core::{
    Clock, DeviceAvailability, DeviceSnapshot, EventSink, ExternalLocationRequester,
    ExternalLocationSource, ExternalLocationUpdate, ExternalTrigger, LocationProvider,
    LocationRequest, LocationSourceKind, ProviderErrorKind, SystemClock, TrackingEvent,
};
use icloud_routing::{
    RouteHistoryEntry, RouteHistoryQuery, RouteHistoryStore, RouteProvider, RouteRequest,
};
use icloud_tracking::{
    AccountTrackingState, ArbitrationDecision, DeviceTrackingState, DirectionInput,
    ExternalArbitrationPolicy, IntervalPolicy, LocationQuality, LocationQualityPolicy,
    NearbyDevice, NearbyDevicePolicy, OldLocationContext, PassThroughPolicy, StationaryObservation,
    StationaryPolicy, TrackingIntervalContext, TrackingState, TrackingStateStore, ZoneSet,
    calculate_old_location_threshold, determine_direction, group_nearby_devices,
    offline_interval_seconds, retry_interval_seconds,
};
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub tick_interval: Duration,
    pub prefetch_window: Duration,
    pub default_update_interval: Duration,
    pub location_quality: LocationQualityPolicy,
    pub zones: ZoneSet,
    pub base_zone_id: Option<String>,
    pub pass_through: PassThroughPolicy,
    pub stationary: StationaryPolicy,
    pub maximum_update_interval: Duration,
    pub away_time_zone_offsets: BTreeMap<String, i32>,
    pub old_location_adjustment_seconds: i64,
    pub old_location_maximum_seconds: Option<u64>,
    pub in_zone_interval: Duration,
    pub stationary_interval: Duration,
    pub exit_zone_interval: Duration,
    pub fixed_interval: Option<Duration>,
    pub travel_time_factor: f64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            prefetch_window: Duration::from_secs(15),
            default_update_interval: Duration::from_secs(60),
            location_quality: LocationQualityPolicy::default(),
            zones: ZoneSet::default(),
            base_zone_id: None,
            pass_through: PassThroughPolicy::default(),
            stationary: StationaryPolicy::default(),
            maximum_update_interval: Duration::from_secs(7_200),
            away_time_zone_offsets: BTreeMap::new(),
            old_location_adjustment_seconds: 0,
            old_location_maximum_seconds: None,
            in_zone_interval: Duration::from_secs(120),
            stationary_interval: Duration::from_secs(300),
            exit_zone_interval: Duration::from_secs(30),
            fixed_interval: None,
            travel_time_factor: 0.5,
        }
    }
}

struct AccountRuntime {
    provider: Arc<dyn LocationProvider>,
    configured_device_ids: Option<BTreeSet<String>>,
    device_ids: BTreeSet<String>,
    next_discovery_at: DateTime<Utc>,
    authentication_error_count: u32,
}

pub struct TrackingRuntime {
    config: RuntimeConfig,
    accounts: BTreeMap<String, AccountRuntime>,
    state: TrackingState,
    store: Arc<dyn TrackingStateStore>,
    events: Arc<dyn EventSink>,
    clock: Arc<dyn Clock>,
    route_provider: Option<Arc<dyn RouteProvider>>,
    route_history: Option<Arc<dyn RouteHistoryStore>>,
}

impl TrackingRuntime {
    /// Loads durable state and creates a standalone scheduler.
    ///
    /// # Errors
    ///
    /// Returns an error when stored tracking state cannot be loaded.
    pub fn new(
        config: RuntimeConfig,
        store: Arc<dyn TrackingStateStore>,
        events: Arc<dyn EventSink>,
    ) -> Result<Self, RuntimeError> {
        Self::with_clock(config, store, events, Arc::new(SystemClock))
    }

    /// Loads durable state with an injected clock for deterministic scheduling
    /// and embedding environments.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration or stored tracking state is invalid.
    pub fn with_clock(
        config: RuntimeConfig,
        store: Arc<dyn TrackingStateStore>,
        events: Arc<dyn EventSink>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, RuntimeError> {
        validate_config(&config)?;
        let state = store.load()?;
        Ok(Self {
            config,
            accounts: BTreeMap::new(),
            state,
            store,
            events,
            clock,
            route_provider: None,
            route_history: None,
        })
    }

    pub fn register_account(
        &mut self,
        account_id: impl Into<String>,
        provider: Arc<dyn LocationProvider>,
        device_ids: impl IntoIterator<Item = String>,
    ) {
        let account_id = account_id.into();
        let configured_device_ids = device_ids.into_iter().collect::<BTreeSet<_>>();
        let persisted = self
            .state
            .accounts
            .get(&account_id)
            .cloned()
            .unwrap_or_default();
        let device_ids = if configured_device_ids.is_empty() {
            persisted.device_ids.clone()
        } else {
            configured_device_ids.clone()
        };
        for device_id in &device_ids {
            self.state
                .devices
                .entry(device_id.clone())
                .or_insert_with(|| DeviceTrackingState::new(device_id));
        }
        let authentication_error_count = device_ids
            .iter()
            .filter_map(|device_id| self.state.devices.get(device_id))
            .map(|device| device.authentication_error_count)
            .max()
            .unwrap_or_default()
            .max(persisted.authentication_error_count);
        let next_discovery_at = if authentication_error_count > 0 {
            persisted.next_discovery_at.unwrap_or_else(|| {
                device_ids
                    .iter()
                    .filter_map(|device_id| self.state.devices.get(device_id)?.next_update_at)
                    .min()
                    .unwrap_or(DateTime::<Utc>::MIN_UTC)
            })
        } else {
            persisted
                .next_discovery_at
                .unwrap_or(DateTime::<Utc>::MIN_UTC)
        };
        self.state.accounts.insert(
            account_id.clone(),
            AccountTrackingState {
                device_ids: device_ids.clone(),
                authentication_error_count,
                next_discovery_at: (next_discovery_at != DateTime::<Utc>::MIN_UTC)
                    .then_some(next_discovery_at),
            },
        );
        self.accounts.insert(
            account_id,
            AccountRuntime {
                provider,
                configured_device_ids: (!configured_device_ids.is_empty())
                    .then_some(configured_device_ids),
                device_ids,
                next_discovery_at,
                authentication_error_count,
            },
        );
    }

    pub fn set_routing(
        &mut self,
        provider: Arc<dyn RouteProvider>,
        history: Option<Arc<dyn RouteHistoryStore>>,
    ) {
        self.route_provider = Some(provider);
        self.route_history = history;
    }

    #[must_use]
    pub fn state(&self) -> &TrackingState {
        &self.state
    }

    /// Pauses one device, or all known devices when no ID is supplied.
    ///
    /// # Errors
    ///
    /// Returns an error if the pause event cannot be emitted.
    pub fn pause(&mut self, device_id: Option<&str>) -> Result<(), RuntimeError> {
        if let Some(device_id) = device_id {
            if !self.state.devices.contains_key(device_id) {
                return Err(RuntimeError::UnknownDevice(device_id.into()));
            }
        }
        for state in self.selected_devices_mut(device_id) {
            state.paused = true;
        }
        let now = self.clock.now();
        self.emit_at(
            now,
            &TrackingEvent::TrackingPaused {
                device_id: device_id.map(str::to_owned),
            },
        )?;
        self.persist_at(now)
    }

    /// Resumes one device or all devices and makes them immediately due.
    ///
    /// # Errors
    ///
    /// Returns an error if the resume event cannot be emitted.
    pub fn resume(
        &mut self,
        device_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), RuntimeError> {
        if let Some(device_id) = device_id {
            if !self.state.devices.contains_key(device_id) {
                return Err(RuntimeError::UnknownDevice(device_id.into()));
            }
        }
        for state in self.selected_devices_mut(device_id) {
            state.paused = false;
            state.next_update_at = Some(now);
        }
        self.emit_at(
            now,
            &TrackingEvent::TrackingResumed {
                device_id: device_id.map(str::to_owned),
            },
        )?;
        self.persist_at(now)
    }

    /// Schedules a device refresh at an absolute UTC time.
    ///
    /// # Errors
    ///
    /// Returns an error when the device ID is unknown.
    pub fn schedule(&mut self, device_id: &str, at: DateTime<Utc>) -> Result<(), RuntimeError> {
        let state = self
            .state
            .devices
            .get_mut(device_id)
            .ok_or_else(|| RuntimeError::UnknownDevice(device_id.into()))?;
        state.next_update_at = Some(at);
        let now = self.clock.now();
        self.emit_at(
            now,
            &TrackingEvent::TrackingScheduled {
                device_id: device_id.into(),
                at,
            },
        )?;
        self.persist_at(now)
    }

    /// Makes an account and all its known devices immediately due.
    ///
    /// # Errors
    ///
    /// Returns an error when the account ID is unknown.
    pub fn locate_now(&mut self, account_id: &str, now: DateTime<Utc>) -> Result<(), RuntimeError> {
        let account = self
            .accounts
            .get_mut(account_id)
            .ok_or_else(|| RuntimeError::UnknownAccount(account_id.into()))?;
        account.next_discovery_at = now;
        for device_id in &account.device_ids {
            if let Some(device) = self.state.devices.get_mut(device_id) {
                device.next_update_at = Some(now);
            }
        }
        self.state
            .accounts
            .entry(account_id.into())
            .or_default()
            .next_discovery_at = Some(now);
        self.emit_at(
            now,
            &TrackingEvent::TrackingLocateRequested {
                account: account_id.into(),
            },
        )?;
        self.persist_at(now)
    }

    /// Applies a mobile-app or other external location through the same durable
    /// state and event interfaces as Apple updates.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid source metadata, arbitration, event, or
    /// persistence failures.
    pub async fn ingest_external_update(
        &mut self,
        update: ExternalLocationUpdate,
        now: DateTime<Utc>,
    ) -> Result<ArbitrationDecision, RuntimeError> {
        if let Some(zone_id) = match update.trigger.as_ref() {
            Some(ExternalTrigger::ZoneEntered(zone_id) | ExternalTrigger::ZoneExited(zone_id)) => {
                Some(zone_id)
            }
            Some(ExternalTrigger::Manual | ExternalTrigger::Background) | None => None,
        } {
            let configured = self
                .config
                .zones
                .zones()
                .iter()
                .any(|zone| zone.id == *zone_id);
            let stationary = self
                .state
                .stationary_zones
                .zones
                .iter()
                .any(|zone| zone.active && zone.id == *zone_id);
            if !configured && !stationary {
                return Err(RuntimeError::InvalidExternalTrigger(zone_id.clone()));
            }
        }
        if update
            .battery
            .as_ref()
            .and_then(|battery| battery.level_percent)
            .is_some_and(|level| level > 100)
        {
            return Err(RuntimeError::InvalidExternalBattery);
        }
        let source_id = match &update.sample.source {
            LocationSourceKind::External(source_id) if !source_id.trim().is_empty() => {
                source_id.clone()
            }
            _ => {
                return Err(RuntimeError::InvalidConfiguration(
                    "external updates require a non-empty external source ID".into(),
                ));
            }
        };
        let policy = ExternalArbitrationPolicy {
            gps_accuracy_threshold_meters: self
                .config
                .location_quality
                .gps_accuracy_threshold_meters,
            ..ExternalArbitrationPolicy::default()
        };
        let decision = policy.arbitrate(
            self.state
                .devices
                .get(&update.device_id)
                .and_then(|state| state.current_location.as_ref()),
            &update,
        )?;
        let ArbitrationDecision::Accept(sample) = &decision else {
            return Ok(decision);
        };

        let existing = self.state.devices.get(&update.device_id);
        let snapshot = DeviceSnapshot {
            id: update.device_id.clone(),
            name: existing
                .and_then(|state| state.name.clone())
                .unwrap_or_else(|| update.device_id.clone()),
            model: existing.and_then(|state| state.model.clone()),
            availability: DeviceAvailability::Online,
            battery: update.battery,
            location: Some(sample.clone()),
            family_shared: existing.and_then(|state| state.family_shared),
            raw: existing.map_or(serde_json::Value::Null, |state| state.raw_device.clone()),
        };
        self.apply_refresh(None, vec![snapshot], now, true, update.trigger)
            .await?;
        self.state
            .external_source_health
            .entry(source_id)
            .or_default()
            .record_update(sample.timestamp);
        self.state.saved_at = Some(now);
        self.store.save(&self.state)?;
        Ok(decision)
    }

    /// Pulls and applies one update from a transport-neutral external source.
    ///
    /// # Errors
    ///
    /// Returns an error when the source or the shared ingestion pipeline fails.
    pub async fn ingest_external_source_once(
        &mut self,
        source: &dyn ExternalLocationSource,
        now: DateTime<Utc>,
    ) -> Result<Option<ArbitrationDecision>, RuntimeError> {
        let Some(update) = source
            .next_update()
            .await
            .map_err(|error| RuntimeError::ExternalSource(error.to_string()))?
        else {
            return Ok(None);
        };
        self.ingest_external_update(update, now).await.map(Some)
    }

    /// Requests a location from an unhealthy external source when its throttle
    /// permits another request.
    ///
    /// # Errors
    ///
    /// Returns an error when timing configuration is invalid or the requester
    /// rejects the request.
    pub async fn request_external_location(
        &mut self,
        source_id: &str,
        device_id: &str,
        requester: &dyn ExternalLocationRequester,
        alive_interval: Duration,
        request_throttle: Duration,
        now: DateTime<Utc>,
    ) -> Result<bool, RuntimeError> {
        let alive_interval = chrono_duration(alive_interval)?;
        let request_throttle = chrono_duration(request_throttle)?;
        let health = self
            .state
            .external_source_health
            .entry(source_id.to_owned())
            .or_default();
        if health.is_healthy(now, alive_interval) || !health.can_request(now, request_throttle) {
            return Ok(false);
        }
        health.record_request(now);
        if let Err(error) = requester.request_location(device_id).await {
            if let Some(health) = self.state.external_source_health.get_mut(source_id) {
                health.record_error();
            }
            self.state.saved_at = Some(now);
            self.store.save(&self.state)?;
            return Err(RuntimeError::ExternalRequest(error.to_string()));
        }
        self.state.saved_at = Some(now);
        self.store.save(&self.state)?;
        Ok(true)
    }

    /// Executes one five-second decision tick. All devices due under the same
    /// account are coalesced into one family-wide provider call.
    ///
    /// # Errors
    ///
    /// Returns an error only for event or persistence failures. Provider errors
    /// are emitted as typed warnings so one account cannot stop other accounts.
    pub async fn tick(&mut self, now: DateTime<Utc>) -> Result<(), RuntimeError> {
        let prefetch = chrono_duration(self.config.prefetch_window)?;
        let due_before = now + prefetch;
        let due_accounts: Vec<_> = self
            .accounts
            .iter()
            .filter(|(_, account)| self.account_is_due(account, now, due_before))
            .map(|(account_id, account)| {
                (
                    account_id.clone(),
                    Arc::clone(&account.provider),
                    account.configured_device_ids.clone(),
                )
            })
            .collect();

        for (account_id, provider, configured_device_ids) in due_accounts {
            let request = LocationRequest {
                family: true,
                selected_device: configured_device_ids
                    .as_ref()
                    .filter(|device_ids| device_ids.len() == 1)
                    .and_then(|device_ids| device_ids.first().cloned()),
            };
            match provider.locate(&request).await {
                Ok(snapshots) => {
                    let snapshots = match configured_device_ids {
                        Some(device_ids) => snapshots
                            .into_iter()
                            .filter(|snapshot| device_ids.contains(&snapshot.id))
                            .collect(),
                        None => snapshots,
                    };
                    self.apply_account_refresh(&account_id, snapshots, now)
                        .await?;
                }
                Err(error) => {
                    let retry_state = self.accounts.get_mut(&account_id).map(|account| {
                        account.authentication_error_count =
                            account.authentication_error_count.saturating_add(1);
                        let retry = retry_interval_seconds(account.authentication_error_count);
                        account.next_discovery_at =
                            now + TimeDelta::seconds(i64::try_from(retry).unwrap_or(i64::MAX));
                        for device_id in &account.device_ids {
                            if let Some(device) = self.state.devices.get_mut(device_id) {
                                device.authentication_error_count =
                                    account.authentication_error_count;
                                device.next_update_at = Some(account.next_discovery_at);
                            }
                        }
                        (
                            account.authentication_error_count,
                            account.next_discovery_at,
                            account.device_ids.clone(),
                        )
                    });
                    if let Some((error_count, next_discovery_at, device_ids)) = retry_state {
                        self.state.accounts.insert(
                            account_id.clone(),
                            AccountTrackingState {
                                device_ids,
                                authentication_error_count: error_count,
                                next_discovery_at: Some(next_discovery_at),
                            },
                        );
                    }
                    if error.kind == ProviderErrorKind::Authentication {
                        self.emit_at(
                            now,
                            &TrackingEvent::AuthenticationRequired {
                                account: account_id.clone(),
                            },
                        )?;
                    }
                    self.emit_at(
                        now,
                        &TrackingEvent::Warning {
                            message: format!("account {account_id} refresh failed: {error}"),
                        },
                    )?;
                }
            }
        }

        self.state.saved_at = Some(now);
        self.store.save(&self.state)?;
        Ok(())
    }

    /// Runs decision ticks until the watch channel requests shutdown, then
    /// persists state one final time.
    ///
    /// # Errors
    ///
    /// Returns an error for tick, event, or persistence failures.
    pub async fn run(&mut self, mut shutdown: watch::Receiver<bool>) -> Result<(), RuntimeError> {
        let mut interval = tokio::time::interval(self.config.tick_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                _ = interval.tick() => self.tick(self.clock.now()).await?,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        self.state.saved_at = Some(self.clock.now());
        self.store.save(&self.state)?;
        Ok(())
    }

    fn account_is_due(
        &self,
        account: &AccountRuntime,
        now: DateTime<Utc>,
        due_before: DateTime<Utc>,
    ) -> bool {
        if account.authentication_error_count > 0 {
            return account.next_discovery_at <= now;
        }
        if account.device_ids.is_empty() {
            return account.next_discovery_at <= due_before;
        }
        account.device_ids.iter().any(|device_id| {
            self.state.devices.get(device_id).is_some_and(|state| {
                !state.paused
                    && state
                        .next_update_at
                        .is_none_or(|next_update| next_update <= due_before)
            })
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn apply_account_refresh(
        &mut self,
        account_id: &str,
        snapshots: Vec<DeviceSnapshot>,
        now: DateTime<Utc>,
    ) -> Result<(), RuntimeError> {
        self.apply_refresh(Some(account_id), snapshots, now, false, None)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn apply_refresh(
        &mut self,
        account_id: Option<&str>,
        snapshots: Vec<DeviceSnapshot>,
        now: DateTime<Utc>,
        external_update: bool,
        external_trigger: Option<ExternalTrigger>,
    ) -> Result<(), RuntimeError> {
        let default_interval = chrono_duration(self.config.default_update_interval)?;
        let maximum_interval_seconds = self.config.maximum_update_interval.as_secs();
        let nearby_exit = matches!(external_trigger, Some(ExternalTrigger::ZoneExited(_)))
            .then(|| {
                let source_device_id = snapshots.first()?.id.clone();
                let group = self.state.devices.get(&source_device_id)?.nearby_group?;
                let members = self
                    .state
                    .devices
                    .values()
                    .filter(|state| {
                        state.device_id != source_device_id && state.nearby_group == Some(group)
                    })
                    .map(|state| state.device_id.clone())
                    .collect::<Vec<_>>();
                (!members.is_empty()).then_some((source_device_id, members))
            })
            .flatten();
        let mut refreshed_ids = BTreeSet::new();
        for snapshot in snapshots {
            let mut tracked_from_zone_ids = self.config.pass_through.tracked_from_zones.clone();
            if let Some(base_zone_id) = &self.config.base_zone_id {
                tracked_from_zone_ids.insert(base_zone_id.clone());
            }
            let DeviceSnapshot {
                id: device_id,
                name,
                model,
                availability,
                battery,
                location,
                family_shared,
                raw,
            } = snapshot;
            let offline = !matches!(availability, DeviceAvailability::Online);
            refreshed_ids.insert(device_id.clone());
            let mut quality = None;
            let mut policy_events = Vec::new();
            let mut selected_regular_zone = None;
            let mut closest_zone_id = None;
            let mut closest_distance_km = None;
            let mut observation = None;
            let previous_zone = self
                .state
                .devices
                .get(&device_id)
                .and_then(|state| state.current_zone.clone());
            let update_location_source = location
                .as_ref()
                .map_or(LocationSourceKind::Apple, |location| {
                    location.source.clone()
                });
            let update_timestamp = location.as_ref().map_or(now, |location| location.timestamp);

            {
                let state = self
                    .state
                    .devices
                    .entry(device_id.clone())
                    .or_insert_with(|| DeviceTrackingState::new(&device_id));
                state.name = Some(name);
                state.model = model;
                state.family_shared = family_shared;
                state.raw_device = raw;
                state.away_time_zone_offset_hours = self
                    .config
                    .away_time_zone_offsets
                    .get(&device_id)
                    .copied()
                    .unwrap_or_default();
                let was_offline = state.availability.as_ref().is_some_and(|availability| {
                    !matches!(availability, DeviceAvailability::Online)
                });
                state.update_availability(availability, now);
                if offline && !was_offline {
                    policy_events.push(TrackingEvent::DeviceOffline {
                        device_id: device_id.clone(),
                    });
                }
                if let Some(location) = location {
                    let tracked_interval_seconds =
                        state.last_update_at.map_or_else(Vec::new, |last| {
                            state
                                .track_from_zones
                                .values()
                                .filter_map(|track| track.next_update_at)
                                .chain(state.next_update_at)
                                .filter_map(|next| {
                                    u64::try_from(next.signed_duration_since(last).num_seconds())
                                        .ok()
                                })
                                .collect()
                        });
                    let old_location_threshold_seconds =
                        calculate_old_location_threshold(&OldLocationContext {
                            approaching_tracked_zone: state.track_from_zones.values().any(
                                |track| {
                                    track.direction == icloud_tracking::Direction::Towards
                                        && track
                                            .last_distance_km
                                            .is_some_and(|distance| distance < 1.0)
                                },
                            ) || (state.direction
                                == icloud_tracking::Direction::Towards
                                && state
                                    .last_zone_distance_km
                                    .is_some_and(|distance| distance < 1.0)),
                            old_location_count: state.consecutive_bad_updates,
                            tracked_interval_seconds,
                            in_zone: state.current_zone.is_some(),
                            distance_from_zone_km: state.last_zone_distance_km.unwrap_or_default(),
                            pass_through_timer_active: state.zone_transition.pending_zone.is_some(),
                            configured_maximum_seconds: self.config.old_location_maximum_seconds,
                            adjustment_seconds: self.config.old_location_adjustment_seconds,
                        })?;
                    let evaluated = LocationQualityPolicy {
                        old_location_threshold_seconds,
                        ..self.config.location_quality
                    }
                    .evaluate(
                        state.current_location.as_ref(),
                        &location,
                        now,
                        state.consecutive_bad_updates,
                    )?;
                    state.apply_location(location, &evaluated);
                    state.location_quality = Some(evaluated.clone());
                    quality = Some(evaluated);
                }
                if let Some(battery) = battery {
                    state.update_battery(battery, update_timestamp);
                    if state.battery_updated_at == Some(update_timestamp) {
                        state.battery_source = Some(update_location_source);
                    }
                }

                let location_was_accepted = quality
                    .as_ref()
                    .is_some_and(|quality| !matches!(quality, LocationQuality::Rejected(_)));
                if location_was_accepted {
                    if let Some(location) = state.current_location.as_ref() {
                        let accuracy = location.horizontal_accuracy_meters.unwrap_or_default();
                        let selection = self.config.zones.select(location.coordinates, accuracy)?;
                        state.zone_distances_km = selection
                            .distances
                            .iter()
                            .map(|distance| {
                                (distance.zone_id.clone(), distance.distance_meters / 1_000.0)
                            })
                            .collect();
                        let tracking_zone = self
                            .config
                            .base_zone_id
                            .as_deref()
                            .and_then(|base_zone_id| {
                                selection
                                    .distances
                                    .iter()
                                    .find(|distance| distance.zone_id == base_zone_id)
                            })
                            .or_else(|| selection.distances.first());
                        closest_distance_km =
                            tracking_zone.map(|distance| distance.distance_meters / 1_000.0);
                        closest_zone_id = tracking_zone.map(|distance| distance.zone_id.clone());
                        let selected_zone = match external_trigger.as_ref() {
                            Some(ExternalTrigger::ZoneEntered(zone_id)) => Some(zone_id.clone()),
                            Some(ExternalTrigger::ZoneExited(zone_id))
                                if state.current_zone.as_deref() == Some(zone_id.as_str()) =>
                            {
                                None
                            }
                            Some(ExternalTrigger::ZoneExited(_)) => state.current_zone.clone(),
                            Some(ExternalTrigger::Manual | ExternalTrigger::Background) | None => {
                                selection.selected_zone.clone()
                            }
                        };
                        policy_events.extend(state.zone_transition.update(
                            &device_id,
                            selected_zone.as_deref(),
                            now,
                            &self.config.pass_through,
                        ));
                        state.current_zone = state.zone_transition.current_zone.clone();
                        selected_regular_zone.clone_from(&state.current_zone);
                        observation = Some((
                            location.coordinates,
                            state.distance_moved_meters.unwrap_or_default(),
                        ));
                    }
                }
            }

            if let Some((location, distance_moved_meters)) = observation {
                let location_is_good = matches!(quality, Some(LocationQuality::Good));
                policy_events.extend(self.state.stationary_zones.observe(
                    StationaryObservation {
                        device_id: &device_id,
                        location,
                        observed_at: now,
                        distance_moved_meters,
                        location_is_good,
                        monitored_only: false,
                    },
                    self.config.stationary,
                    &self.config.zones,
                )?);
            }

            let mut route_duration_seconds = None;
            let mut route_distance_km = None;
            let nearby_route = observation.and_then(|(destination, _)| {
                self.state.devices.values().find_map(|state| {
                    let location = state.current_location.as_ref()?;
                    let same_route_zone =
                        state.route_zone_id.as_deref() == closest_zone_id.as_deref();
                    let current_route = state.route_updated_at == Some(now);
                    let close_enough = location.coordinates.distance_meters(destination) <= 25.0;
                    let accurate_enough =
                        location.horizontal_accuracy_meters.unwrap_or(f64::INFINITY) <= 25.0;
                    (state.device_id != device_id
                        && same_route_zone
                        && current_route
                        && close_enough
                        && accurate_enough)
                        .then_some((state.route_distance_km?, state.route_duration_seconds))
                })
            });
            if let Some((distance_km, duration_seconds)) = nearby_route {
                route_distance_km = Some(distance_km);
                route_duration_seconds = duration_seconds;
                closest_distance_km = route_distance_km;
            } else if let (Some(provider), Some((destination, _)), Some(zone_id)) = (
                self.route_provider.as_ref(),
                observation,
                closest_zone_id.as_deref(),
            ) {
                let origin = self
                    .config
                    .zones
                    .zones()
                    .iter()
                    .find(|zone| zone.id == zone_id)
                    .map(icloud_tracking::Zone::center)
                    .transpose()?;
                if let Some(origin) = origin {
                    let mut estimate = None;
                    if let Some(history) = self.route_history.as_ref() {
                        match history
                            .lookup(&RouteHistoryQuery {
                                zone_id: zone_id.to_owned(),
                                destination,
                                maximum_distance_meters: 100.0,
                            })
                            .await
                        {
                            Ok(entry) => estimate = entry.map(|entry| entry.estimate),
                            Err(error) => policy_events.push(TrackingEvent::Warning {
                                message: format!("route-history lookup failed: {error}"),
                            }),
                        }
                    }
                    if estimate.is_none() {
                        match provider
                            .route(&RouteRequest {
                                origin,
                                destination,
                                departure: now,
                            })
                            .await
                        {
                            Ok(calculated) => {
                                if let Some(history) = self.route_history.as_ref() {
                                    if let Err(error) = history
                                        .store(&RouteHistoryEntry {
                                            id: None,
                                            zone_id: zone_id.to_owned(),
                                            destination,
                                            estimate: calculated.clone(),
                                            recorded_at: now,
                                            use_count: 1,
                                        })
                                        .await
                                    {
                                        policy_events.push(TrackingEvent::Warning {
                                            message: format!("route-history store failed: {error}"),
                                        });
                                    }
                                }
                                estimate = Some(calculated);
                            }
                            Err(error) => policy_events.push(TrackingEvent::Warning {
                                message: format!("route calculation failed: {error}"),
                            }),
                        }
                    }
                    if let Some(estimate) = estimate {
                        route_duration_seconds = estimate.duration_seconds;
                        route_distance_km = Some(estimate.distance_km);
                        closest_distance_km = route_distance_km;
                    }
                }
            }

            let stationary_zone = self
                .state
                .stationary_zones
                .device_zone(&device_id)
                .map(str::to_owned);
            if closest_distance_km.is_none() {
                closest_distance_km = self
                    .state
                    .stationary_zones
                    .zones
                    .iter()
                    .find(|zone| stationary_zone.as_deref() == Some(zone.id.as_str()))
                    .and_then(|zone| {
                        self.state.devices[&device_id]
                            .current_location
                            .as_ref()
                            .map(|location| {
                                location.coordinates.distance_meters(zone.center) / 1_000.0
                            })
                    });
            }

            let final_zone = stationary_zone.or(selected_regular_zone);
            let state_changed = !policy_events.is_empty();
            let in_stationary_zone = final_zone
                .as_deref()
                .is_some_and(|zone| zone.starts_with("stationary_"));
            let distance_from_zone_km = closest_distance_km.unwrap_or_default();
            let state = self
                .state
                .devices
                .get_mut(&device_id)
                .expect("inserted above");
            let previous_route_duration_seconds = state.route_duration_seconds;
            if route_distance_km.is_some() {
                state.route_zone_id.clone_from(&closest_zone_id);
                state.route_distance_km = route_distance_km;
                state.route_duration_seconds = route_duration_seconds;
                state.route_updated_at = Some(now);
            }
            state.current_zone = final_zone;
            let direction = determine_direction(
                DirectionInput {
                    in_zone: state.current_zone.is_some(),
                    current_distance_km: distance_from_zone_km,
                    previous_distance_km: state.last_zone_distance_km,
                    current_travel_time_seconds: route_duration_seconds
                        .map(|seconds| Duration::from_secs(seconds).as_secs_f64()),
                    previous_travel_time_seconds: previous_route_duration_seconds
                        .map(|seconds| Duration::from_secs(seconds).as_secs_f64()),
                    previous_direction: state.direction,
                    went_beyond_three_km: state.went_beyond_three_km,
                },
                &state.direction_history,
            )?;
            state.direction_history.record(direction);
            state.direction = direction.direction;
            state.last_zone_distance_km = Some(distance_from_zone_km);
            state.went_beyond_three_km |= distance_from_zone_km >= 3.0;
            self.state
                .zone_occupancy
                .update(&device_id, state.current_zone.as_deref());

            let location_old = matches!(
                quality,
                Some(
                    LocationQuality::Grace(icloud_tracking::RejectionReason::OldLocation)
                        | LocationQuality::Rejected(icloud_tracking::RejectionReason::OldLocation)
                )
            );
            let gps_poor = matches!(
                quality,
                Some(
                    LocationQuality::Grace(icloud_tracking::RejectionReason::PoorGps)
                        | LocationQuality::Rejected(icloud_tracking::RejectionReason::PoorGps)
                )
            );
            let decision = IntervalPolicy::determine(TrackingIntervalContext {
                state_changed,
                in_zone: state.current_zone.is_some(),
                was_in_zone: previous_zone.is_some(),
                in_stationary_zone,
                stationary_zone_is_small: in_stationary_zone,
                external_exit_trigger_recent: matches!(
                    external_trigger,
                    Some(ExternalTrigger::ZoneExited(_))
                ),
                external_update,
                location_old,
                gps_poor,
                location_good: matches!(quality, Some(LocationQuality::Good)),
                offline,
                pass_through_timer_active: state.zone_transition.pending_zone.is_some(),
                battery_percent: state
                    .battery
                    .as_ref()
                    .and_then(|battery| battery.level_percent),
                distance_from_zone_km,
                direction: state.direction,
                went_beyond_three_km: state.went_beyond_three_km,
                waze_enabled: route_duration_seconds.is_some(),
                waze_travel_seconds: route_duration_seconds,
                travel_time_factor: self.config.travel_time_factor,
                in_zone_interval_seconds: self.config.in_zone_interval.as_secs(),
                stationary_interval_seconds: self.config.stationary_interval.as_secs(),
                exit_zone_interval_seconds: self.config.exit_zone_interval.as_secs(),
                old_location_threshold_seconds: self
                    .config
                    .location_quality
                    .old_location_threshold_seconds,
                fixed_interval_seconds: self
                    .config
                    .fixed_interval
                    .map_or(0, |value| value.as_secs()),
                maximum_interval_seconds,
                error_count: state.consecutive_bad_updates,
            })?;
            let mut interval_seconds = decision.seconds;
            state
                .track_from_zones
                .retain(|zone_id, _| tracked_from_zone_ids.contains(zone_id));
            for zone_id in &tracked_from_zone_ids {
                let Some(distance_from_target_km) = state.zone_distances_km.get(zone_id).copied()
                else {
                    continue;
                };
                let in_target_zone = state.current_zone.as_deref() == Some(zone_id.as_str());
                let was_in_target_zone = previous_zone.as_deref() == Some(zone_id.as_str());
                let track = state.track_from_zones.entry(zone_id.clone()).or_default();
                track.zone_id.clone_from(zone_id);
                let target_direction = if closest_zone_id.as_deref() == Some(zone_id.as_str()) {
                    track.direction_history.record(direction);
                    direction
                } else {
                    let direction = determine_direction(
                        DirectionInput {
                            in_zone: in_target_zone,
                            current_distance_km: distance_from_target_km,
                            previous_distance_km: track.last_distance_km,
                            current_travel_time_seconds: None,
                            previous_travel_time_seconds: None,
                            previous_direction: track.direction,
                            went_beyond_three_km: track.went_beyond_three_km,
                        },
                        &track.direction_history,
                    )?;
                    track.direction_history.record(direction);
                    direction
                };
                track.direction = target_direction.direction;
                track.last_distance_km = Some(distance_from_target_km);
                track.went_beyond_three_km |= distance_from_target_km >= 3.0;
                let target_decision = if closest_zone_id.as_deref() == Some(zone_id.as_str()) {
                    decision
                } else {
                    IntervalPolicy::determine(TrackingIntervalContext {
                        state_changed: state_changed && (in_target_zone || was_in_target_zone),
                        in_zone: in_target_zone,
                        was_in_zone: was_in_target_zone,
                        in_stationary_zone,
                        stationary_zone_is_small: in_stationary_zone,
                        external_exit_trigger_recent: matches!(
                            external_trigger,
                            Some(ExternalTrigger::ZoneExited(_))
                        ),
                        external_update,
                        location_old,
                        gps_poor,
                        location_good: matches!(quality, Some(LocationQuality::Good)),
                        offline,
                        pass_through_timer_active: state.zone_transition.pending_zone.is_some(),
                        battery_percent: state
                            .battery
                            .as_ref()
                            .and_then(|battery| battery.level_percent),
                        distance_from_zone_km: distance_from_target_km,
                        direction: track.direction,
                        went_beyond_three_km: track.went_beyond_three_km,
                        waze_enabled: false,
                        waze_travel_seconds: None,
                        travel_time_factor: self.config.travel_time_factor,
                        in_zone_interval_seconds: self.config.in_zone_interval.as_secs(),
                        stationary_interval_seconds: self.config.stationary_interval.as_secs(),
                        exit_zone_interval_seconds: self.config.exit_zone_interval.as_secs(),
                        old_location_threshold_seconds: self
                            .config
                            .location_quality
                            .old_location_threshold_seconds,
                        fixed_interval_seconds: self
                            .config
                            .fixed_interval
                            .map_or(0, |value| value.as_secs()),
                        maximum_interval_seconds,
                        error_count: state.consecutive_bad_updates,
                    })?
                };
                interval_seconds = interval_seconds.min(target_decision.seconds);
                if !state.paused {
                    track.next_update_at = Some(
                        now + TimeDelta::seconds(
                            i64::try_from(target_decision.seconds).unwrap_or(i64::MAX),
                        ),
                    );
                }
            }
            interval_seconds = if offline {
                let location_age = state
                    .current_location
                    .as_ref()
                    .map_or(u64::MAX, |location| {
                        u64::try_from(
                            now.signed_duration_since(location.timestamp)
                                .num_seconds()
                                .max(0),
                        )
                        .unwrap_or_default()
                    });
                offline_interval_seconds(location_age)
            } else {
                interval_seconds
            };
            if !state.paused {
                state.next_update_at = Some(
                    now + TimeDelta::seconds(i64::try_from(interval_seconds).unwrap_or(i64::MAX)),
                );
                if offline {
                    for track in state.track_from_zones.values_mut() {
                        track.next_update_at = state.next_update_at;
                    }
                }
            }
            for event in policy_events {
                self.emit_at(now, &event)?;
            }
            self.emit_at(now, &TrackingEvent::DeviceUpdated { device_id })?;
        }
        self.update_nearby_devices()?;
        if let Some((_source_device_id, members)) = nearby_exit {
            let at = now + TimeDelta::minutes(2);
            let mut scheduled = Vec::new();
            for device_id in members {
                let Some(state) = self.state.devices.get_mut(&device_id) else {
                    continue;
                };
                if state.paused
                    || state
                        .next_update_at
                        .is_some_and(|next_update| next_update <= at)
                {
                    continue;
                }
                state.next_update_at = Some(at);
                for track in state.track_from_zones.values_mut() {
                    track.next_update_at = Some(at);
                }
                scheduled.push(device_id);
            }
            for device_id in scheduled {
                self.emit_at(now, &TrackingEvent::TrackingScheduled { device_id, at })?;
            }
        }
        if let Some(account) = account_id.and_then(|account_id| self.accounts.get_mut(account_id)) {
            account.device_ids = account
                .configured_device_ids
                .clone()
                .unwrap_or(refreshed_ids);
            account.authentication_error_count = 0;
            for device_id in &account.device_ids {
                if let Some(device) = self.state.devices.get_mut(device_id) {
                    device.authentication_error_count = 0;
                    if device.next_update_at.is_none() {
                        device.next_update_at = Some(now + default_interval);
                    }
                }
            }
            account.next_discovery_at = account
                .device_ids
                .iter()
                .filter_map(|device_id| self.state.devices.get(device_id)?.next_update_at)
                .min()
                .unwrap_or(now + default_interval);
            if let Some(account_id) = account_id {
                self.state.accounts.insert(
                    account_id.into(),
                    AccountTrackingState {
                        device_ids: account.device_ids.clone(),
                        authentication_error_count: 0,
                        next_discovery_at: Some(account.next_discovery_at),
                    },
                );
            }
        }
        Ok(())
    }

    fn update_nearby_devices(&mut self) -> Result<(), RuntimeError> {
        let candidates: Vec<_> = self
            .state
            .devices
            .values()
            .filter_map(|state| {
                let location = state.current_location.as_ref()?;
                Some((
                    state.device_id.clone(),
                    location.coordinates,
                    location.horizontal_accuracy_meters,
                    state.current_zone.clone(),
                    matches!(state.availability, Some(DeviceAvailability::Online)),
                    state
                        .model
                        .as_deref()
                        .is_some_and(|model| model.to_ascii_lowercase().contains("watch")),
                ))
            })
            .collect();
        let inputs: Vec<_> = candidates
            .iter()
            .map(
                |(device_id, location, accuracy, zone, online, is_watch)| NearbyDevice {
                    device_id,
                    location: *location,
                    horizontal_accuracy_meters: *accuracy,
                    current_zone: zone.as_deref(),
                    tracked: true,
                    online: *online,
                    is_watch: *is_watch,
                },
            )
            .collect();
        let groups = group_nearby_devices(&inputs, NearbyDevicePolicy::default())?;

        for state in self.state.devices.values_mut() {
            state.nearby_group = None;
            state.nearby_device_id = None;
            state.nearby_device_distance_meters = None;
        }
        for group in groups {
            for device_id in &group.device_ids {
                let Some(origin) = self.state.devices[device_id]
                    .current_location
                    .as_ref()
                    .map(|location| location.coordinates)
                else {
                    continue;
                };
                let nearest = group
                    .device_ids
                    .iter()
                    .filter(|candidate| *candidate != device_id)
                    .filter_map(|candidate| {
                        self.state.devices[candidate]
                            .current_location
                            .as_ref()
                            .map(|location| {
                                (
                                    candidate.clone(),
                                    origin.distance_meters(location.coordinates),
                                )
                            })
                    })
                    .min_by(|left, right| left.1.total_cmp(&right.1));
                if let Some((nearest_id, distance)) = nearest {
                    let state = self
                        .state
                        .devices
                        .get_mut(device_id)
                        .expect("nearby group contains known device");
                    state.nearby_group = Some(group.id);
                    state.nearby_device_id = Some(nearest_id);
                    state.nearby_device_distance_meters = Some(distance);
                }
            }
        }
        Ok(())
    }

    fn selected_devices_mut(
        &mut self,
        device_id: Option<&str>,
    ) -> impl Iterator<Item = &mut DeviceTrackingState> {
        self.state
            .devices
            .values_mut()
            .filter(move |device| device_id.is_none_or(|selected| device.device_id == selected))
    }

    fn emit_at(
        &mut self,
        occurred_at: DateTime<Utc>,
        event: &TrackingEvent,
    ) -> Result<(), RuntimeError> {
        self.state.record_event(occurred_at, event.clone());
        self.events.emit(event).map_err(RuntimeError::Event)
    }

    fn persist_at(&mut self, now: DateTime<Utc>) -> Result<(), RuntimeError> {
        self.state.saved_at = Some(now);
        self.store.save(&self.state)?;
        Ok(())
    }
}

fn validate_config(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    if config.tick_interval.is_zero()
        || config.prefetch_window.is_zero()
        || config.default_update_interval.as_secs() < 5
        || config.maximum_update_interval.as_secs() < 5
        || config.maximum_update_interval < config.default_update_interval
        || config.in_zone_interval.as_secs() < 5
        || config.stationary_interval.as_secs() < 5
        || config.exit_zone_interval.as_secs() < 5
        || !config.travel_time_factor.is_finite()
        || config.travel_time_factor <= 0.0
        || !config
            .location_quality
            .gps_accuracy_threshold_meters
            .is_finite()
        || config.location_quality.gps_accuracy_threshold_meters < 0.0
        || config.stationary.still_seconds == 0
        || !config.stationary.radius_meters.is_finite()
        || config.stationary.radius_meters <= 0.0
        || !config.stationary.movement_limit_meters.is_finite()
        || config.stationary.movement_limit_meters < 0.0
        || config
            .fixed_interval
            .is_some_and(|interval| interval.as_secs() < 300)
    {
        return Err(RuntimeError::InvalidConfiguration(
            "runtime policy durations must be valid and at least five seconds".into(),
        ));
    }
    if config.base_zone_id.as_deref().is_some_and(|base_zone_id| {
        !config
            .zones
            .zones()
            .iter()
            .any(|zone| zone.id == base_zone_id && !zone.passive)
    }) {
        return Err(RuntimeError::InvalidConfiguration(
            "base zone must reference a configured zone".into(),
        ));
    }
    if config
        .pass_through
        .tracked_from_zones
        .iter()
        .any(|zone_id| {
            !config
                .zones
                .zones()
                .iter()
                .any(|zone| zone.id == *zone_id && !zone.passive)
        })
    {
        return Err(RuntimeError::InvalidConfiguration(
            "tracked-from zones must reference configured zones".into(),
        ));
    }
    Ok(())
}

fn chrono_duration(duration: Duration) -> Result<TimeDelta, RuntimeError> {
    TimeDelta::from_std(duration)
        .map_err(|error| RuntimeError::InvalidConfiguration(error.to_string()))
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid runtime configuration: {0}")]
    InvalidConfiguration(String),
    #[error("unknown account: {0}")]
    UnknownAccount(String),
    #[error("unknown device: {0}")]
    UnknownDevice(String),
    #[error("external location request failed: {0}")]
    ExternalRequest(String),
    #[error("external location source failed: {0}")]
    ExternalSource(String),
    #[error("external zone trigger references unknown zone: {0}")]
    InvalidExternalTrigger(String),
    #[error("external battery percentage must be within 0..=100")]
    InvalidExternalBattery,
    #[error(transparent)]
    Tracking(#[from] icloud_tracking::TrackingError),
    #[error(transparent)]
    Event(icloud_location_core::EventSinkError),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::TimeZone;
    use icloud_location_core::{
        BatterySnapshot, BoxFuture, Coordinates, DeviceAvailability, EventSinkError,
        ExternalTrigger, LocationSample, LocationSourceKind, ProviderError,
    };
    use icloud_routing::{RouteEstimate, RouteStatus, RoutingError};
    use icloud_tracking::Zone;

    use super::*;

    struct FakeProvider {
        calls: AtomicUsize,
        snapshots: Vec<DeviceSnapshot>,
        fail: bool,
    }

    impl LocationProvider for FakeProvider {
        fn locate<'a>(
            &'a self,
            _request: &'a LocationRequest,
        ) -> BoxFuture<'a, Result<Vec<DeviceSnapshot>, ProviderError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.fail {
                    Err(ProviderError {
                        kind: ProviderErrorKind::Other,
                        message: "injected failure".into(),
                    })
                } else {
                    Ok(self.snapshots.clone())
                }
            })
        }
    }

    struct CapturingProvider {
        requests: Mutex<Vec<LocationRequest>>,
        snapshots: Vec<DeviceSnapshot>,
    }

    struct AuthenticationFailureProvider;

    impl LocationProvider for AuthenticationFailureProvider {
        fn locate<'a>(
            &'a self,
            _request: &'a LocationRequest,
        ) -> BoxFuture<'a, Result<Vec<DeviceSnapshot>, ProviderError>> {
            Box::pin(async {
                Err(ProviderError {
                    kind: ProviderErrorKind::Authentication,
                    message: "credentials required".into(),
                })
            })
        }
    }

    impl LocationProvider for CapturingProvider {
        fn locate<'a>(
            &'a self,
            request: &'a LocationRequest,
        ) -> BoxFuture<'a, Result<Vec<DeviceSnapshot>, ProviderError>> {
            self.requests.lock().unwrap().push(request.clone());
            Box::pin(async { Ok(self.snapshots.clone()) })
        }
    }

    #[derive(Default)]
    struct MemoryStore(Mutex<TrackingState>);

    impl TrackingStateStore for MemoryStore {
        fn load(&self) -> Result<TrackingState, icloud_tracking::TrackingError> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn save(&self, state: &TrackingState) -> Result<(), icloud_tracking::TrackingError> {
            *self.0.lock().unwrap() = state.clone();
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryEvents(Mutex<Vec<TrackingEvent>>);

    impl EventSink for MemoryEvents {
        fn emit(&self, event: &TrackingEvent) -> Result<(), EventSinkError> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn snapshot_at(id: &str, timestamp: DateTime<Utc>, coordinates: Coordinates) -> DeviceSnapshot {
        DeviceSnapshot {
            id: id.into(),
            name: id.into(),
            model: None,
            availability: DeviceAvailability::Online,
            battery: Some(BatterySnapshot {
                level_percent: Some(80),
                ..BatterySnapshot::default()
            }),
            location: Some(LocationSample {
                coordinates,
                horizontal_accuracy_meters: Some(5.0),
                vertical_accuracy_meters: None,
                timestamp,
                source: LocationSourceKind::Apple,
                is_old: false,
            }),
            family_shared: Some(false),
            raw: serde_json::json!({ "id": id }),
        }
    }

    fn snapshot(id: &str, timestamp: DateTime<Utc>) -> DeviceSnapshot {
        snapshot_at(id, timestamp, Coordinates::new(10.0, 20.0).unwrap())
    }

    fn home_zone_config() -> RuntimeConfig {
        RuntimeConfig {
            zones: ZoneSet::new(vec![Zone {
                id: "home".into(),
                latitude: 10.0,
                longitude: 20.0,
                radius_meters: 100.0,
                passive: false,
            }])
            .unwrap(),
            ..RuntimeConfig::default()
        }
    }

    #[tokio::test]
    async fn coalesces_two_due_devices_into_one_account_refresh() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: vec![snapshot("one", now), snapshot("two", now)],
            fail: false,
        });
        let store = Arc::new(MemoryStore::default());
        let events = Arc::new(MemoryEvents::default());
        let mut runtime = TrackingRuntime::new(RuntimeConfig::default(), store, events).unwrap();
        runtime.register_account(
            "account",
            provider.clone(),
            ["one".to_owned(), "two".to_owned()],
        );

        runtime.tick(now).await.unwrap();

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.state.devices.len(), 2);
    }

    #[tokio::test]
    async fn restricts_a_configured_account_to_its_selected_devices() {
        let now = Utc.timestamp_opt(1_500, 0).unwrap();
        let provider = Arc::new(CapturingProvider {
            requests: Mutex::new(Vec::new()),
            snapshots: vec![snapshot("selected", now), snapshot("not-selected", now)],
        });
        let mut runtime = TrackingRuntime::new(
            RuntimeConfig::default(),
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        runtime.register_account("account", provider.clone(), ["selected".into()]);

        runtime.tick(now).await.unwrap();

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].selected_device.as_deref(), Some("selected"));
        assert!(runtime.state.devices.contains_key("selected"));
        assert!(!runtime.state.devices.contains_key("not-selected"));
    }

    #[tokio::test]
    async fn one_account_failure_does_not_block_another_account() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let failed = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: Vec::new(),
            fail: true,
        });
        let working = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: vec![snapshot("working-device", now)],
            fail: false,
        });
        let store = Arc::new(MemoryStore::default());
        let events = Arc::new(MemoryEvents::default());
        let mut runtime = TrackingRuntime::new(
            RuntimeConfig::default(),
            store,
            Arc::clone(&events) as Arc<dyn EventSink>,
        )
        .unwrap();
        runtime.register_account("broken", failed, Vec::new());
        runtime.register_account("working", working, Vec::new());

        runtime.tick(now).await.unwrap();

        assert!(runtime.state.devices.contains_key("working-device"));
        assert!(
            events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, TrackingEvent::Warning { .. }))
        );
    }

    #[tokio::test]
    async fn emits_a_typed_authentication_event_for_account_failures() {
        let now = Utc.timestamp_opt(1_700, 0).unwrap();
        let events = Arc::new(MemoryEvents::default());
        let mut runtime = TrackingRuntime::new(
            RuntimeConfig::default(),
            Arc::new(MemoryStore::default()),
            Arc::clone(&events) as Arc<dyn EventSink>,
        )
        .unwrap();
        runtime.register_account(
            "account",
            Arc::new(AuthenticationFailureProvider),
            Vec::new(),
        );

        runtime.tick(now).await.unwrap();

        assert!(events.0.lock().unwrap().iter().any(|event| matches!(
            event,
            TrackingEvent::AuthenticationRequired { account } if account == "account"
        )));
    }

    #[tokio::test]
    async fn authentication_backoff_survives_prefetch_ticks_and_restart() {
        let now = Utc.timestamp_opt(2_000, 0).unwrap();
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: Vec::new(),
            fail: true,
        });
        let store = Arc::new(MemoryStore::default());
        let mut runtime = TrackingRuntime::with_clock(
            RuntimeConfig::default(),
            Arc::clone(&store) as Arc<dyn TrackingStateStore>,
            Arc::new(MemoryEvents::default()),
            Arc::new(FixedClock(now)),
        )
        .unwrap();
        runtime.register_account("account", provider.clone(), ["device".into()]);

        runtime.tick(now).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.state.devices["device"].next_update_at,
            Some(now + TimeDelta::seconds(5))
        );
        runtime.tick(now + TimeDelta::seconds(4)).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        runtime.tick(now + TimeDelta::seconds(5)).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            runtime.state.devices["device"].next_update_at,
            Some(now + TimeDelta::seconds(20))
        );

        let restarted_provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: Vec::new(),
            fail: true,
        });
        let mut restarted = TrackingRuntime::new(
            RuntimeConfig::default(),
            store,
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        restarted.register_account("account", restarted_provider.clone(), ["device".into()]);
        restarted.tick(now + TimeDelta::seconds(19)).await.unwrap();
        assert_eq!(restarted_provider.calls.load(Ordering::SeqCst), 0);
        restarted.tick(now + TimeDelta::seconds(20)).await.unwrap();
        assert_eq!(restarted_provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn discovery_account_backoff_and_membership_survive_restart() {
        let now = Utc.timestamp_opt(2_500, 0).unwrap();
        let store = Arc::new(MemoryStore::default());
        let discovery_provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: vec![snapshot("discovered", now)],
            fail: false,
        });
        let mut runtime = TrackingRuntime::new(
            RuntimeConfig::default(),
            Arc::clone(&store) as Arc<dyn TrackingStateStore>,
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        runtime.register_account("account", discovery_provider, Vec::new());
        runtime.tick(now).await.unwrap();
        assert_eq!(
            runtime.state.accounts["account"].device_ids,
            BTreeSet::from(["discovered".into()])
        );

        let failed_provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: Vec::new(),
            fail: true,
        });
        let mut restarted = TrackingRuntime::new(
            RuntimeConfig::default(),
            Arc::clone(&store) as Arc<dyn TrackingStateStore>,
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        restarted.register_account("account", failed_provider.clone(), Vec::new());
        assert!(
            restarted.accounts["account"]
                .device_ids
                .contains("discovered")
        );
        restarted
            .locate_now("account", now + TimeDelta::seconds(1))
            .unwrap();
        restarted.tick(now + TimeDelta::seconds(1)).await.unwrap();
        assert_eq!(failed_provider.calls.load(Ordering::SeqCst), 1);

        let second_restart_provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: Vec::new(),
            fail: true,
        });
        let mut second_restart = TrackingRuntime::new(
            RuntimeConfig::default(),
            store,
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        second_restart.register_account("account", second_restart_provider.clone(), Vec::new());
        second_restart
            .tick(now + TimeDelta::seconds(5))
            .await
            .unwrap();
        assert_eq!(second_restart_provider.calls.load(Ordering::SeqCst), 0);
        second_restart
            .tick(now + TimeDelta::seconds(6))
            .await
            .unwrap();
        assert_eq!(second_restart_provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pausing_a_known_device_suppresses_the_account_refresh_deadline() {
        let now = Utc.timestamp_opt(2_750, 0).unwrap();
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: vec![snapshot("device", now)],
            fail: false,
        });
        let mut runtime = TrackingRuntime::with_clock(
            RuntimeConfig::default(),
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
            Arc::new(FixedClock(now)),
        )
        .unwrap();
        runtime.register_account("account", provider.clone(), ["device".into()]);

        runtime.pause(Some("device")).unwrap();
        runtime.tick(now).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

        runtime.resume(Some("device"), now).unwrap();
        runtime.tick(now).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pause_resume_and_manual_schedule_update_durable_state() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let store = Arc::new(MemoryStore::default());
        let events = Arc::new(MemoryEvents::default());
        let mut runtime = TrackingRuntime::with_clock(
            RuntimeConfig::default(),
            Arc::clone(&store) as Arc<dyn TrackingStateStore>,
            Arc::clone(&events) as Arc<dyn EventSink>,
            Arc::new(FixedClock(now)),
        )
        .unwrap();
        runtime
            .state
            .devices
            .insert("device".into(), DeviceTrackingState::new("device"));
        runtime.register_account(
            "account",
            Arc::new(FakeProvider {
                calls: AtomicUsize::new(0),
                snapshots: Vec::new(),
                fail: false,
            }),
            ["device".into()],
        );

        runtime.pause(Some("device")).unwrap();
        assert!(runtime.state.devices["device"].paused);
        runtime.resume(Some("device"), now).unwrap();
        assert!(!runtime.state.devices["device"].paused);
        assert_eq!(runtime.state.devices["device"].next_update_at, Some(now));
        runtime.locate_now("account", now).unwrap();
        runtime
            .schedule("device", now + TimeDelta::minutes(5))
            .unwrap();
        assert_eq!(
            runtime.state.devices["device"].next_update_at,
            Some(now + TimeDelta::minutes(5))
        );
        assert_eq!(
            store.0.lock().unwrap().devices["device"].next_update_at,
            Some(now + TimeDelta::minutes(5))
        );
        let emitted = events.0.lock().unwrap();
        assert!(emitted.iter().any(|event| matches!(
            event,
            TrackingEvent::TrackingPaused { device_id } if device_id.as_deref() == Some("device")
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TrackingEvent::TrackingResumed { device_id } if device_id.as_deref() == Some("device")
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TrackingEvent::TrackingLocateRequested { account } if account == "account"
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TrackingEvent::TrackingScheduled { device_id, at }
                if device_id == "device" && *at == now + TimeDelta::minutes(5)
        )));
        assert!(matches!(
            runtime.schedule("missing", now),
            Err(RuntimeError::UnknownDevice(device_id)) if device_id == "missing"
        ));
    }

    #[tokio::test]
    async fn records_offline_duration_and_emits_only_the_offline_transition() {
        let now = Utc.timestamp_opt(1_250, 0).unwrap();
        let events = Arc::new(MemoryEvents::default());
        let mut runtime = TrackingRuntime::new(
            RuntimeConfig::default(),
            Arc::new(MemoryStore::default()),
            Arc::clone(&events) as Arc<dyn EventSink>,
        )
        .unwrap();
        let mut offline = snapshot("device", now);
        offline.availability = DeviceAvailability::Offline;
        offline.location = None;

        runtime
            .apply_account_refresh("account", vec![offline.clone()], now)
            .await
            .unwrap();
        runtime
            .apply_account_refresh("account", vec![offline], now + TimeDelta::seconds(30))
            .await
            .unwrap();

        let device = &runtime.state.devices["device"];
        assert_eq!(device.offline_since, Some(now));
        assert_eq!(
            device.offline_duration_seconds(now + TimeDelta::seconds(30)),
            30
        );
        runtime
            .ingest_external_update(
                ExternalLocationUpdate {
                    device_id: "device".into(),
                    sample: LocationSample {
                        coordinates: Coordinates::new(10.0, 20.0).unwrap(),
                        horizontal_accuracy_meters: Some(5.0),
                        vertical_accuracy_meters: None,
                        timestamp: now + TimeDelta::seconds(40),
                        source: LocationSourceKind::External("mobile".into()),
                        is_old: false,
                    },
                    battery: Some(BatterySnapshot {
                        level_percent: Some(70),
                        ..BatterySnapshot::default()
                    }),
                    trigger: None,
                },
                now + TimeDelta::seconds(40),
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.state.devices["device"].availability,
            Some(DeviceAvailability::Online)
        );
        assert_eq!(runtime.state.devices["device"].offline_since, None);
        assert_eq!(
            events
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, TrackingEvent::DeviceOffline { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn prefetches_at_fifteen_seconds_and_persists_graceful_shutdown() {
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            snapshots: vec![snapshot("device", now + TimeDelta::seconds(1))],
            fail: false,
        });
        let store = Arc::new(MemoryStore::default());
        let mut runtime = TrackingRuntime::with_clock(
            RuntimeConfig::default(),
            Arc::clone(&store) as Arc<dyn TrackingStateStore>,
            Arc::new(MemoryEvents::default()),
            Arc::new(FixedClock(now)),
        )
        .unwrap();
        runtime.register_account("account", provider.clone(), ["device".into()]);
        runtime
            .accounts
            .get_mut("account")
            .unwrap()
            .next_discovery_at = now + TimeDelta::hours(1);
        let mut state = DeviceTrackingState::new("device");
        state.next_update_at = Some(now + TimeDelta::seconds(16));
        runtime.state.devices.insert("device".into(), state);

        runtime.tick(now).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        runtime.tick(now + TimeDelta::seconds(1)).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        runtime.pause(Some("device")).unwrap();
        let (_shutdown_sender, shutdown_receiver) = watch::channel(true);
        runtime.run(shutdown_receiver).await.unwrap();
        let persisted = store.load().unwrap();
        assert_eq!(persisted.saved_at, Some(now));
        assert!(persisted.devices["device"].paused);
        assert!(
            persisted
                .event_history
                .iter()
                .any(|entry| matches!(entry.event, TrackingEvent::TrackingPaused { .. }))
        );
    }

    #[tokio::test]
    async fn applies_delayed_zone_entry_exit_and_direction() {
        let now = Utc.timestamp_opt(10_000, 0).unwrap();
        let config = RuntimeConfig {
            zones: ZoneSet::new(vec![Zone {
                id: "home".into(),
                latitude: 10.0,
                longitude: 20.0,
                radius_meters: 100.0,
                passive: false,
            }])
            .unwrap(),
            pass_through: PassThroughPolicy {
                delay_seconds: 60,
                ..PassThroughPolicy::default()
            },
            stationary: StationaryPolicy {
                enabled: false,
                ..StationaryPolicy::default()
            },
            ..RuntimeConfig::default()
        };
        let events = Arc::new(MemoryEvents::default());
        let mut runtime = TrackingRuntime::new(
            config,
            Arc::new(MemoryStore::default()),
            Arc::clone(&events) as Arc<dyn EventSink>,
        )
        .unwrap();
        runtime.register_account(
            "account",
            Arc::new(FakeProvider {
                calls: AtomicUsize::new(0),
                snapshots: Vec::new(),
                fail: false,
            }),
            Vec::new(),
        );

        runtime
            .apply_account_refresh("account", vec![snapshot("device", now)], now)
            .await
            .unwrap();
        assert!(runtime.state.devices["device"].current_zone.is_none());
        runtime
            .apply_account_refresh(
                "account",
                vec![snapshot("device", now + TimeDelta::seconds(60))],
                now + TimeDelta::seconds(60),
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.state.devices["device"].current_zone.as_deref(),
            Some("home")
        );
        runtime
            .apply_account_refresh(
                "account",
                vec![snapshot_at(
                    "device",
                    now + TimeDelta::seconds(61),
                    Coordinates::new(10.01, 20.01).unwrap(),
                )],
                now + TimeDelta::seconds(61),
            )
            .await
            .unwrap();
        assert!(runtime.state.devices["device"].current_zone.is_none());
        assert_eq!(
            runtime.state.devices["device"].direction,
            icloud_tracking::Direction::AwayFrom
        );
        let emitted = events.0.lock().unwrap();
        assert!(emitted.iter().any(|event| matches!(
            event,
            TrackingEvent::ZoneEntered { zone_id, .. } if zone_id == "home"
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TrackingEvent::ZoneExited { zone_id, .. } if zone_id == "home"
        )));
        let entered = emitted
            .iter()
            .position(|event| matches!(event, TrackingEvent::ZoneEntered { .. }))
            .unwrap();
        let exited = emitted
            .iter()
            .position(|event| matches!(event, TrackingEvent::ZoneExited { .. }))
            .unwrap();
        assert!(entered < exited);
    }

    struct FakeRouteProvider(AtomicUsize);

    impl RouteProvider for FakeRouteProvider {
        fn route<'a>(
            &'a self,
            _request: &'a RouteRequest,
        ) -> BoxFuture<'a, Result<RouteEstimate, RoutingError>> {
            let call = self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(RouteEstimate {
                    status: RouteStatus::Used,
                    distance_km: if call == 0 { 5.0 } else { 4.0 },
                    duration_seconds: Some(600),
                    provider: "fake".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn route_provider_updates_arrival_direction_and_travel_interval() {
        let now = Utc.timestamp_opt(20_000, 0).unwrap();
        let config = RuntimeConfig {
            zones: ZoneSet::new(vec![Zone {
                id: "home".into(),
                latitude: 10.0,
                longitude: 20.0,
                radius_meters: 10.0,
                passive: false,
            }])
            .unwrap(),
            stationary: StationaryPolicy {
                enabled: false,
                ..StationaryPolicy::default()
            },
            ..RuntimeConfig::default()
        };
        let mut runtime = TrackingRuntime::new(
            config,
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        runtime.register_account(
            "account",
            Arc::new(FakeProvider {
                calls: AtomicUsize::new(0),
                snapshots: Vec::new(),
                fail: false,
            }),
            Vec::new(),
        );
        let routes = Arc::new(FakeRouteProvider(AtomicUsize::new(0)));
        runtime.set_routing(routes.clone(), None);
        let away = Coordinates::new(10.04, 20.0).unwrap();

        runtime
            .apply_account_refresh("account", vec![snapshot_at("device", now, away)], now)
            .await
            .unwrap();
        let second_now = now + TimeDelta::seconds(1);
        runtime
            .apply_account_refresh(
                "account",
                vec![snapshot_at("device", second_now, away)],
                second_now,
            )
            .await
            .unwrap();

        let state = &runtime.state.devices["device"];
        assert_eq!(routes.0.load(Ordering::SeqCst), 2);
        assert_eq!(state.route_duration_seconds, Some(600));
        assert_eq!(state.route_distance_km, Some(4.0));
        assert_eq!(state.direction, icloud_tracking::Direction::Towards);
        assert_eq!(
            state.next_update_at,
            Some(second_now + TimeDelta::seconds(300))
        );
        assert_eq!(
            runtime.state.snapshot(second_now).devices[0].arrival_at,
            Some(second_now + TimeDelta::seconds(600))
        );
    }

    #[tokio::test]
    async fn uses_configured_base_zone_and_reuses_routes_for_nearby_devices() {
        let now = Utc.timestamp_opt(30_000, 0).unwrap();
        let destination = Coordinates::new(10.04, 20.0).unwrap();
        let config = RuntimeConfig {
            zones: ZoneSet::new(vec![
                Zone {
                    id: "home".into(),
                    latitude: 10.0,
                    longitude: 20.0,
                    radius_meters: 10.0,
                    passive: false,
                },
                Zone {
                    id: "office".into(),
                    latitude: destination.latitude,
                    longitude: destination.longitude,
                    radius_meters: 100.0,
                    passive: false,
                },
            ])
            .unwrap(),
            base_zone_id: Some("home".into()),
            stationary: StationaryPolicy {
                enabled: false,
                ..StationaryPolicy::default()
            },
            ..RuntimeConfig::default()
        };
        let mut runtime = TrackingRuntime::new(
            config,
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        runtime.register_account(
            "account",
            Arc::new(FakeProvider {
                calls: AtomicUsize::new(0),
                snapshots: Vec::new(),
                fail: false,
            }),
            Vec::new(),
        );
        let routes = Arc::new(FakeRouteProvider(AtomicUsize::new(0)));
        runtime.set_routing(routes.clone(), None);

        runtime
            .apply_account_refresh(
                "account",
                vec![
                    snapshot_at("phone", now, destination),
                    snapshot_at("tablet", now, destination),
                ],
                now,
            )
            .await
            .unwrap();

        assert_eq!(routes.0.load(Ordering::SeqCst), 1);
        for device_id in ["phone", "tablet"] {
            let device = &runtime.state.devices[device_id];
            assert_eq!(device.current_zone.as_deref(), Some("office"));
            assert_eq!(device.route_zone_id.as_deref(), Some("home"));
            assert_eq!(device.route_distance_km, Some(5.0));
            assert!(device.nearby_group.is_some());
            assert_eq!(device.zone_distances_km.len(), 2);
        }
    }

    #[tokio::test]
    async fn chooses_the_earliest_interval_across_tracked_from_zones() {
        let now = Utc.timestamp_opt(40_000, 0).unwrap();
        let config = RuntimeConfig {
            zones: ZoneSet::new(vec![
                Zone {
                    id: "home".into(),
                    latitude: 10.0,
                    longitude: 20.0,
                    radius_meters: 10.0,
                    passive: false,
                },
                Zone {
                    id: "work".into(),
                    latitude: 10.1,
                    longitude: 20.0,
                    radius_meters: 10.0,
                    passive: false,
                },
            ])
            .unwrap(),
            base_zone_id: Some("home".into()),
            pass_through: PassThroughPolicy {
                tracked_from_zones: BTreeSet::from(["home".into(), "work".into()]),
                ..PassThroughPolicy::default()
            },
            stationary: StationaryPolicy {
                enabled: false,
                ..StationaryPolicy::default()
            },
            ..RuntimeConfig::default()
        };
        let mut runtime = TrackingRuntime::new(
            config,
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        runtime.register_account(
            "account",
            Arc::new(FakeProvider {
                calls: AtomicUsize::new(0),
                snapshots: Vec::new(),
                fail: false,
            }),
            Vec::new(),
        );

        runtime
            .apply_account_refresh(
                "account",
                vec![snapshot_at(
                    "device",
                    now,
                    Coordinates::new(10.05, 20.0).unwrap(),
                )],
                now,
            )
            .await
            .unwrap();
        let second_now = now + TimeDelta::seconds(1);
        runtime
            .apply_account_refresh(
                "account",
                vec![snapshot_at(
                    "device",
                    second_now,
                    Coordinates::new(10.09, 20.0).unwrap(),
                )],
                second_now,
            )
            .await
            .unwrap();

        let device = &runtime.state.devices["device"];
        assert_eq!(device.direction, icloud_tracking::Direction::AwayFrom);
        assert_eq!(
            device.track_from_zones["work"].direction,
            icloud_tracking::Direction::Towards
        );
        assert_eq!(
            device.next_update_at,
            Some(second_now + TimeDelta::seconds(15))
        );
        assert_eq!(device.track_from_zones.len(), 2);
    }

    struct FakeExternalRequester(AtomicUsize);

    impl ExternalLocationRequester for FakeExternalRequester {
        fn request_location<'a>(
            &'a self,
            _device_id: &'a str,
        ) -> BoxFuture<'a, Result<(), ProviderError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct QueuedExternalSource(Mutex<Option<ExternalLocationUpdate>>);

    impl ExternalLocationSource for QueuedExternalSource {
        fn next_update(
            &self,
        ) -> BoxFuture<'_, Result<Option<ExternalLocationUpdate>, ProviderError>> {
            let update = self.0.lock().unwrap().take();
            Box::pin(async move { Ok(update) })
        }
    }

    #[tokio::test]
    async fn ingests_external_zone_trigger_and_throttles_callback_requests() {
        let now = Utc.timestamp_opt(30_000, 0).unwrap();
        let mut runtime = TrackingRuntime::new(
            home_zone_config(),
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        let update = ExternalLocationUpdate {
            device_id: "device".into(),
            sample: LocationSample {
                coordinates: Coordinates::new(10.0, 20.0).unwrap(),
                horizontal_accuracy_meters: Some(5.0),
                vertical_accuracy_meters: None,
                timestamp: now,
                source: LocationSourceKind::External("mobile".into()),
                is_old: false,
            },
            battery: None,
            trigger: Some(ExternalTrigger::ZoneEntered("home".into())),
        };

        let mut unknown_zone = update.clone();
        unknown_zone.trigger = Some(ExternalTrigger::ZoneEntered("unknown".into()));
        assert!(matches!(
            runtime.ingest_external_update(unknown_zone, now).await,
            Err(RuntimeError::InvalidExternalTrigger(zone_id)) if zone_id == "unknown"
        ));
        let mut invalid_battery = update.clone();
        invalid_battery.battery = Some(BatterySnapshot {
            level_percent: Some(101),
            ..BatterySnapshot::default()
        });
        assert!(matches!(
            runtime.ingest_external_update(invalid_battery, now).await,
            Err(RuntimeError::InvalidExternalBattery)
        ));

        assert!(matches!(
            runtime.ingest_external_update(update, now).await.unwrap(),
            ArbitrationDecision::Accept(_)
        ));
        assert_eq!(
            runtime.state.devices["device"].current_zone.as_deref(),
            Some("home")
        );
        let requester = FakeExternalRequester(AtomicUsize::new(0));
        assert!(
            !runtime
                .request_external_location(
                    "mobile",
                    "device",
                    &requester,
                    Duration::from_secs(300),
                    Duration::from_secs(60),
                    now,
                )
                .await
                .unwrap()
        );
        assert!(
            runtime
                .request_external_location(
                    "mobile",
                    "device",
                    &requester,
                    Duration::from_secs(300),
                    Duration::from_secs(60),
                    now + TimeDelta::seconds(301),
                )
                .await
                .unwrap()
        );
        assert_eq!(requester.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pulls_external_adapters_through_the_public_source_interface() {
        let now = Utc.timestamp_opt(40_000, 0).unwrap();
        let source = QueuedExternalSource(Mutex::new(Some(ExternalLocationUpdate {
            device_id: "device".into(),
            sample: LocationSample {
                coordinates: Coordinates::new(10.0, 20.0).unwrap(),
                horizontal_accuracy_meters: Some(5.0),
                vertical_accuracy_meters: None,
                timestamp: now,
                source: LocationSourceKind::External("adapter".into()),
                is_old: false,
            },
            battery: None,
            trigger: Some(ExternalTrigger::Background),
        })));
        let mut runtime = TrackingRuntime::new(
            RuntimeConfig::default(),
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();

        assert!(matches!(
            runtime
                .ingest_external_source_once(&source, now)
                .await
                .unwrap(),
            Some(ArbitrationDecision::Accept(_))
        ));
        assert_eq!(
            runtime
                .ingest_external_source_once(&source, now)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn applies_the_full_zone_and_interval_pipeline_to_external_gps() {
        let now = Utc.timestamp_opt(50_000, 0).unwrap();
        let config = RuntimeConfig {
            zones: ZoneSet::new(vec![Zone {
                id: "home".into(),
                latitude: 10.0,
                longitude: 20.0,
                radius_meters: 100.0,
                passive: false,
            }])
            .unwrap(),
            base_zone_id: Some("home".into()),
            pass_through: PassThroughPolicy {
                tracked_from_zones: BTreeSet::from(["home".into()]),
                ..PassThroughPolicy::default()
            },
            stationary: StationaryPolicy {
                enabled: false,
                ..StationaryPolicy::default()
            },
            ..RuntimeConfig::default()
        };
        let mut runtime = TrackingRuntime::new(
            config,
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        let update = ExternalLocationUpdate {
            device_id: "device".into(),
            sample: LocationSample {
                coordinates: Coordinates::new(10.0, 20.0).unwrap(),
                horizontal_accuracy_meters: Some(5.0),
                vertical_accuracy_meters: None,
                timestamp: now,
                source: LocationSourceKind::External("webhook".into()),
                is_old: false,
            },
            battery: Some(BatterySnapshot {
                level_percent: Some(70),
                ..BatterySnapshot::default()
            }),
            trigger: None,
        };

        runtime.ingest_external_update(update, now).await.unwrap();

        let device = &runtime.state.devices["device"];
        assert_eq!(device.current_zone.as_deref(), Some("home"));
        assert_eq!(device.direction, icloud_tracking::Direction::InZone);
        assert_eq!(
            device.track_from_zones["home"].direction,
            icloud_tracking::Direction::InZone
        );
        assert_eq!(device.next_update_at, Some(now + TimeDelta::seconds(180)));
        assert_eq!(device.battery.as_ref().unwrap().level_percent, Some(70));
        assert_eq!(
            device.battery_source,
            Some(LocationSourceKind::External("webhook".into()))
        );
    }

    #[tokio::test]
    async fn an_older_apple_refresh_cannot_replace_a_newer_external_location() {
        let now = Utc.timestamp_opt(60_000, 0).unwrap();
        let mut runtime = TrackingRuntime::new(
            RuntimeConfig::default(),
            Arc::new(MemoryStore::default()),
            Arc::new(MemoryEvents::default()),
        )
        .unwrap();
        let external_coordinates = Coordinates::new(11.0, 21.0).unwrap();
        runtime
            .ingest_external_update(
                ExternalLocationUpdate {
                    device_id: "device".into(),
                    sample: LocationSample {
                        coordinates: external_coordinates,
                        horizontal_accuracy_meters: Some(5.0),
                        vertical_accuracy_meters: None,
                        timestamp: now,
                        source: LocationSourceKind::External("webhook".into()),
                        is_old: false,
                    },
                    battery: Some(BatterySnapshot {
                        level_percent: Some(70),
                        ..BatterySnapshot::default()
                    }),
                    trigger: None,
                },
                now,
            )
            .await
            .unwrap();

        runtime
            .apply_account_refresh(
                "account",
                vec![snapshot_at(
                    "device",
                    now - TimeDelta::seconds(1),
                    Coordinates::new(10.0, 20.0).unwrap(),
                )],
                now,
            )
            .await
            .unwrap();

        let location = runtime.state.devices["device"]
            .current_location
            .as_ref()
            .unwrap();
        assert_eq!(location.coordinates, external_coordinates);
        assert_eq!(
            location.source,
            LocationSourceKind::External("webhook".into())
        );
        assert_eq!(
            runtime.state.devices["device"]
                .battery
                .as_ref()
                .unwrap()
                .level_percent,
            Some(70)
        );
        assert_eq!(
            runtime.state.devices["device"].battery_source,
            Some(LocationSourceKind::External("webhook".into()))
        );
    }

    #[tokio::test]
    async fn an_external_zone_exit_schedules_nearby_devices_within_two_minutes() {
        let now = Utc.timestamp_opt(70_000, 0).unwrap();
        let events = Arc::new(MemoryEvents::default());
        let mut runtime = TrackingRuntime::new(
            home_zone_config(),
            Arc::new(MemoryStore::default()),
            Arc::clone(&events) as Arc<dyn EventSink>,
        )
        .unwrap();
        runtime
            .apply_account_refresh(
                "account",
                vec![snapshot("phone", now), snapshot("tablet", now)],
                now,
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.state.devices["phone"].nearby_group,
            runtime.state.devices["tablet"].nearby_group
        );
        runtime
            .state
            .devices
            .get_mut("tablet")
            .unwrap()
            .next_update_at = Some(now + TimeDelta::hours(1));

        runtime
            .ingest_external_update(
                ExternalLocationUpdate {
                    device_id: "phone".into(),
                    sample: LocationSample {
                        coordinates: Coordinates::new(10.0, 20.0).unwrap(),
                        horizontal_accuracy_meters: Some(5.0),
                        vertical_accuracy_meters: None,
                        timestamp: now + TimeDelta::seconds(10),
                        source: LocationSourceKind::External("mobile".into()),
                        is_old: false,
                    },
                    battery: None,
                    trigger: Some(ExternalTrigger::ZoneExited("home".into())),
                },
                now + TimeDelta::seconds(10),
            )
            .await
            .unwrap();

        let scheduled_at = now + TimeDelta::seconds(130);
        assert_eq!(
            runtime.state.devices["tablet"].next_update_at,
            Some(scheduled_at)
        );
        assert!(events.0.lock().unwrap().iter().any(|event| matches!(
            event,
            TrackingEvent::TrackingScheduled { device_id, at }
                if device_id == "tablet" && *at == scheduled_at
        )));
    }
}
