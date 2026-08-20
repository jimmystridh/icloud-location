//! Apple authentication, session persistence, and Find My device operations.

mod client;
mod coordinates;
mod error;
mod model;
mod provider;
mod security_key;
mod session;
mod srp;

pub use client::{
    Account, AuthenticationStatus, ClientBuilder, DisplayMessageRequest, ICloudClient,
    LocateOptions, LostModeConfirmation, LostModeRequest, Region, TermsAcceptanceConfirmation,
    TrustCookieStatus, TrustedPhoneNumber, TrustedSessionSnapshot, TwoFactorChallenge,
    VerificationMethod,
};
pub use coordinates::ChinaCoordinates;
pub use error::{Error, Result};
pub use model::{Battery, Device, DeviceKind, DeviceStatus, Location};
pub use provider::FindMyProvider;
#[cfg(feature = "security-key")]
pub use security_key::UsbSecurityKeyAuthenticator;
pub use security_key::{SecurityKeyAssertion, SecurityKeyAuthenticator, SecurityKeyRequest};
