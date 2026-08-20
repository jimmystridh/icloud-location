//! Find My protocol and session errors.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Apple account password is required because no valid session is available")]
    CredentialsRequired,

    #[error("the Apple account requires acceptance of updated iCloud terms at https://icloud.com")]
    TermsOfUseRequired,

    #[error("the Apple account is not authenticated")]
    NotAuthenticated,

    #[error("the Apple account requires two-factor authentication")]
    TwoFactorRequired,

    #[error("the Find My service is temporarily unavailable")]
    FindMyUnavailable,

    #[error("the Apple account is locked")]
    AccountLocked,

    #[error("lost mode requires explicit confirmation")]
    LostModeConfirmationRequired,

    #[error("the verification code was rejected")]
    VerificationCodeRejected,

    #[error("security-key authentication failed: {0}")]
    SecurityKey(String),

    #[error("credential provider failed: {0}")]
    CredentialProvider(String),

    #[error("unsupported Apple SRP protocol: {0}")]
    UnsupportedSrpProtocol(String),

    #[error("invalid SRP response: {0}")]
    InvalidSrp(String),

    #[error("Apple API request failed (HTTP {status}{code}): {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },

    #[error("unexpected Apple API response: {0}")]
    UnexpectedResponse(String),

    #[error("session storage error: {0}")]
    Session(String),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Base64(#[from] base64::DecodeError),

    #[error(transparent)]
    Url(#[from] url::ParseError),
}

pub type Result<T> = std::result::Result<T, Error>;
