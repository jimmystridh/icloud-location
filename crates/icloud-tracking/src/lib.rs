//! Deterministic tracking, interval, and zone policy.

mod direction;
mod external;
mod interval;
mod nearby;
mod quality;
mod state;
mod stationary;
mod zone;
mod zone_state;

pub use direction::{
    DirectionDecision, DirectionHistory, DirectionInput, DirectionObservation, determine_direction,
};
pub use external::{
    ArbitrationDecision, ExternalArbitrationPolicy, ExternalRejectionReason, ExternalSourceHealth,
};
pub use interval::{
    Direction, IntervalDecision, IntervalPolicy, IntervalReason, TrackingIntervalContext,
    TrackingIntervalDecision, TrackingIntervalReason, offline_interval_seconds,
    retry_interval_seconds,
};
pub use nearby::{NearbyDevice, NearbyDeviceGroup, NearbyDevicePolicy, group_nearby_devices};
pub use quality::{
    LocationQuality, LocationQualityPolicy, OldLocationContext, RejectionReason,
    calculate_old_location_threshold,
};
pub use state::{
    AccountTrackingState, DeviceTrackingSnapshot, DeviceTrackingState, JsonTrackingStore,
    TrackFromZoneSnapshot, TrackFromZoneState, TrackingSnapshot, TrackingState, TrackingStateStore,
};
pub use stationary::{
    StationaryObservation, StationaryPolicy, StationaryZone, StationaryZoneManager,
};
pub use zone::{Zone, ZoneDistance, ZoneSelection, ZoneSet, zones_have_same_center};
pub use zone_state::{PassThroughPolicy, PendingZone, ZoneOccupancy, ZoneTransitionState};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TrackingError {
    #[error("invalid tracking input: {0}")]
    InvalidInput(String),

    #[error("duplicate zone ID: {0}")]
    DuplicateZone(String),

    #[error("tracking persistence failed: {0}")]
    Persistence(String),
}
