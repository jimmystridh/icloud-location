//! Apple account and Find My HTTP client.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use cookie_store::CookieExpiration;
use icloud_location_core::CredentialProvider;
use reqwest::header::{
    ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, ORIGIN, REFERER, USER_AGENT,
};
use reqwest::{RequestBuilder, StatusCode};
use reqwest_cookie_store::CookieStoreMutex;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::coordinates::ChinaCoordinates;
use crate::error::{Error, Result};
use crate::model::{Device, devices_from_response};
use crate::security_key::{SecurityKeyAuthenticator, SecurityKeyRequest};
use crate::session::{SessionData, SessionStore};
use crate::srp::{AppleSrp, SrpInitResponse};

const OAUTH_CLIENT_ID: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const REFRESH_ENDPOINT: &str = "/fmipservice/client/web/refreshClient";
const PLAY_SOUND_ENDPOINT: &str = "/fmipservice/client/web/playSound";
const DISPLAY_MESSAGE_ENDPOINT: &str = "/fmipservice/client/web/sendMessage";
const LOST_DEVICE_ENDPOINT: &str = "/fmipservice/client/web/lostDevice";
const TRUST_COOKIE_NAME: &str = "X-APPLE-WEBAUTH-HSA-TRUST";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    #[default]
    Global,
    China {
        coordinates: ChinaCoordinates,
    },
}

#[derive(Clone, Debug)]
pub struct ClientBuilder {
    username: String,
    password: Option<SecretString>,
    session_root: Option<PathBuf>,
    region: Region,
    timeout: Duration,
}

impl ClientBuilder {
    #[must_use]
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: None,
            session_root: None,
            region: Region::Global,
            timeout: Duration::from_secs(30),
        }
    }

    #[must_use]
    pub fn password(mut self, password: impl Into<SecretString>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Resolves a password from a caller-provided credential source without
    /// persisting it in session state.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential provider cannot read its source.
    pub fn credential_provider(mut self, provider: &dyn CredentialProvider) -> Result<Self> {
        if self.password.is_none() {
            self.password = provider
                .password(self.username.trim())
                .map_err(|error| Error::CredentialProvider(error.to_string()))?;
        }
        Ok(self)
    }

    #[must_use]
    pub fn session_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_root = Some(path.into());
        self
    }

    #[must_use]
    pub fn region(mut self, region: Region) -> Self {
        self.region = region;
        self
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builds a client and loads any session previously saved for the account.
    ///
    /// # Errors
    ///
    /// Returns an error when the username is empty, session files cannot be read,
    /// endpoint URLs are invalid, or the HTTP client cannot be constructed.
    pub fn build(self) -> Result<ICloudClient> {
        let username = self.username.trim().to_owned();
        if username.is_empty() {
            return Err(Error::UnexpectedResponse(
                "Apple account username cannot be empty".into(),
            ));
        }

        let endpoints = Endpoints::new(self.region)?;
        let session_store = SessionStore::new(self.session_root, &username)?;
        let mut session = session_store.load_state()?;
        if session.client_id.is_empty() {
            session.client_id = format!("auth-{}", uuid::Uuid::new_v4());
        }
        let cookies = session_store.load_cookies()?;

        let home_origin = endpoints.home.origin().ascii_serialization();
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            ORIGIN,
            HeaderValue::from_str(&home_origin).map_err(invalid_header)?,
        );
        default_headers.insert(
            REFERER,
            HeaderValue::from_str(&home_origin).map_err(invalid_header)?,
        );
        default_headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("icloud-location/", env!("CARGO_PKG_VERSION"))),
        );
        let http = reqwest::Client::builder()
            .default_headers(default_headers)
            .cookie_provider(Arc::clone(&cookies))
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .timeout(self.timeout)
            .build()?;

        Ok(ICloudClient {
            username,
            password: self.password,
            region: self.region,
            endpoints,
            http,
            cookies,
            session_store,
            session,
            account: None,
            challenge: None,
        })
    }
}

pub struct ICloudClient {
    username: String,
    password: Option<SecretString>,
    region: Region,
    endpoints: Endpoints,
    http: reqwest::Client,
    cookies: Arc<CookieStoreMutex>,
    session_store: SessionStore,
    session: SessionData,
    account: Option<Account>,
    challenge: Option<TwoFactorChallenge>,
}

impl ICloudClient {
    #[must_use]
    pub fn builder(username: impl Into<String>) -> ClientBuilder {
        ClientBuilder::new(username)
    }

    pub fn set_password(&mut self, password: impl Into<SecretString>) {
        self.password = Some(password.into());
    }

    #[must_use]
    pub fn account(&self) -> Option<&Account> {
        self.account.as_ref()
    }

    #[must_use]
    pub fn challenge(&self) -> Option<&TwoFactorChallenge> {
        self.challenge.as_ref()
    }

    #[must_use]
    pub fn cached_challenge(&self) -> Option<&TwoFactorChallenge> {
        self.session.challenge_metadata.as_ref()
    }

    #[must_use]
    pub fn snapshot_trusted_session(&self) -> TrustedSessionSnapshot {
        TrustedSessionSnapshot {
            account_country: self.session.account_country.clone(),
            session_id: self.session.session_id.clone(),
            session_token: self.session.session_token.clone(),
            trust_token: self.session.trust_token.clone(),
            scnt: self.session.scnt.clone(),
            dsid: self.session.dsid.clone(),
            findme_url: self.session.findme_url.clone(),
            account_name: self.session.account_name.clone(),
        }
    }

    /// Restores and verifies a previously captured trusted session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAuthenticated`] when the snapshot has no trust token.
    /// Network, Apple API, and persistence failures are also returned.
    pub async fn restore_trusted_session(
        &mut self,
        snapshot: TrustedSessionSnapshot,
    ) -> Result<AuthenticationStatus> {
        if snapshot.trust_token.as_deref().is_none_or(str::is_empty) {
            return Err(Error::NotAuthenticated);
        }
        self.session.account_country = snapshot.account_country;
        self.session.session_id = snapshot.session_id;
        self.session.session_token = snapshot.session_token;
        self.session.trust_token = snapshot.trust_token;
        self.session.scnt = snapshot.scnt;
        self.session.dsid = snapshot.dsid;
        self.session.findme_url = snapshot.findme_url;
        self.session.account_name = snapshot.account_name;
        self.persist()?;

        let data = self.account_login().await?;
        self.finish_account_login(&data, false).await
    }

    /// Invalidates local trusted-session credentials while retaining cached
    /// non-secret challenge metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the cookie lock is poisoned or state cannot be saved.
    pub fn untrust_session(&mut self) -> Result<()> {
        self.session.clear_authentication();
        self.cookies
            .lock()
            .map_err(|_| Error::Session("cookie store lock is poisoned".into()))?
            .clear();
        self.account = None;
        self.challenge = None;
        self.persist()
    }

    /// Reports the persisted Apple trust-cookie expiry relative to an injected
    /// time so callers and tests can make deterministic reauthentication choices.
    ///
    /// # Errors
    ///
    /// Returns an error if the cookie store lock is poisoned.
    pub fn trust_cookie_status(&self, now: DateTime<Utc>) -> Result<Option<TrustCookieStatus>> {
        let cookies = self
            .cookies
            .lock()
            .map_err(|_| Error::Session("cookie store lock is poisoned".into()))?;
        let expires_at = cookies
            .iter_any()
            .find(|cookie| cookie.name().eq_ignore_ascii_case(TRUST_COOKIE_NAME))
            .and_then(|cookie| match cookie.expires {
                CookieExpiration::AtUtc(expires) => {
                    DateTime::from_timestamp(expires.unix_timestamp(), expires.nanosecond())
                }
                CookieExpiration::SessionEnd => None,
            });
        Ok(expires_at.map(|expires_at| {
            let days_remaining = expires_at.signed_duration_since(now).num_days();
            TrustCookieStatus {
                expires_at,
                days_remaining,
                reauthentication_recommended: days_remaining <= 45,
            }
        }))
    }

    /// Authenticates with a saved token or performs an Apple GSA/SRP password sign-in.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CredentialsRequired`] if password authentication is necessary
    /// and no password was supplied. Network, protocol, API, and session persistence
    /// failures are also returned.
    pub async fn authenticate(&mut self) -> Result<AuthenticationStatus> {
        self.authenticate_at(Utc::now()).await
    }

    /// Authenticates with an injected time for deterministic proactive-session
    /// refresh decisions.
    ///
    /// # Errors
    ///
    /// Returns authentication, network, protocol, API, or persistence errors.
    pub async fn authenticate_at(&mut self, now: DateTime<Utc>) -> Result<AuthenticationStatus> {
        if self.session.has_session_token() {
            if self.password.is_some()
                && self
                    .trust_cookie_status(now)?
                    .is_some_and(|status| status.reauthentication_recommended)
            {
                return self.authenticate_with_password().await;
            }
            match self.account_login().await {
                Ok(data) => return self.finish_account_login(&data, false).await,
                Err(error) if authentication_expired(&error) => {
                    self.session.clear_authentication();
                    self.persist()?;
                }
                Err(error) => return Err(error),
            }
        }

        self.authenticate_with_password().await
    }

    /// Forces a password-backed SRP refresh when credentials are available,
    /// otherwise validates the existing trusted session.
    ///
    /// # Errors
    ///
    /// Returns authentication, network, protocol, API, or persistence errors.
    pub async fn refresh_session(&mut self) -> Result<AuthenticationStatus> {
        if self.password.is_some() {
            self.authenticate_with_password().await
        } else {
            self.validate_session().await
        }
    }

    async fn authenticate_with_password(&mut self) -> Result<AuthenticationStatus> {
        self.password_sign_in().await?;
        self.session.last_authentication_method = Some("password_srp".into());
        let data = self.account_login().await?;
        self.finish_account_login(&data, true).await
    }

    /// Validates the currently saved Apple session without using a password.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAuthenticated`] when no saved token exists. Network,
    /// Apple API, response parsing, and persistence failures are also returned.
    pub async fn validate_session(&mut self) -> Result<AuthenticationStatus> {
        if !self.session.has_session_token() {
            return Err(Error::NotAuthenticated);
        }
        let request = self
            .http
            .post(self.endpoints.setup.join("validate")?)
            .query(&self.request_parameters())
            .json(&Value::Null);
        let data = self.send_json(request, &[200]).await?;
        self.finish_account_login(&data, true).await
    }

    /// Validates the configured username and password with Apple's lightweight
    /// SRP init/complete exchange without performing account login or Find My.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CredentialsRequired`] when no password is configured,
    /// or the Apple protocol/API error when validation fails.
    pub async fn validate_credentials(&mut self) -> Result<()> {
        self.password_sign_in().await
    }

    /// Requests a two-factor code from a trusted device or by SMS.
    ///
    /// # Errors
    ///
    /// Returns an error when the authentication session is incomplete, Apple rejects
    /// the requested method, the network request fails, or the session cannot be saved.
    pub async fn request_verification_code(&mut self, method: VerificationMethod) -> Result<()> {
        let headers = self.auth_headers()?;
        let request = match method {
            VerificationMethod::TrustedDevice => self
                .http
                .put(
                    self.endpoints
                        .auth
                        .join("verify/trusteddevice/securitycode")?,
                )
                .headers(headers),
            VerificationMethod::Sms { phone_id } => self
                .http
                .put(self.endpoints.auth.join("verify/phone")?)
                .headers(headers)
                .json(&json!({
                    "phoneNumber": { "id": phone_id },
                    "mode": "sms"
                })),
        };

        self.send_json(request, &[200, 204]).await?;
        Ok(())
    }

    /// Submits a two-factor code, trusts the session, and completes account login.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VerificationCodeRejected`] for an invalid code. Network,
    /// protocol, API, and session persistence failures are also returned.
    pub async fn verify_verification_code(
        &mut self,
        method: VerificationMethod,
        code: &str,
    ) -> Result<AuthenticationStatus> {
        let code = code.trim();
        if code.is_empty() {
            return Err(Error::VerificationCodeRejected);
        }

        let headers = self.auth_headers()?;
        let request = match method {
            VerificationMethod::TrustedDevice => self
                .http
                .post(
                    self.endpoints
                        .auth
                        .join("verify/trusteddevice/securitycode")?,
                )
                .headers(headers)
                .json(&json!({ "securityCode": { "code": code } })),
            VerificationMethod::Sms { phone_id } => self
                .http
                .post(self.endpoints.auth.join("verify/phone/securitycode")?)
                .headers(headers)
                .json(&json!({
                    "phoneNumber": { "id": phone_id },
                    "securityCode": { "code": code },
                    "mode": "sms"
                })),
        };

        if let Err(error) = self.send_json(request, &[200, 204]).await {
            if verification_rejected(&error) {
                return Err(Error::VerificationCodeRejected);
            }
            return Err(error);
        }

        let trust_request = self
            .http
            .get(self.endpoints.auth.join("2sv/trust")?)
            .headers(self.auth_headers()?);
        self.send_json(trust_request, &[200, 204]).await?;

        let data = self.account_login().await?;
        let status = self.finish_account_login(&data, false).await?;
        if matches!(status, AuthenticationStatus::TwoFactorRequired(_)) {
            return Err(Error::VerificationCodeRejected);
        }
        self.session.last_trusted_at = Some(Utc::now());
        self.session.last_authentication_method = Some(
            match method {
                VerificationMethod::TrustedDevice => "trusted_device",
                VerificationMethod::Sms { .. } => "sms",
            }
            .into(),
        );
        self.persist()?;
        Ok(status)
    }

    /// Completes Apple's `WebAuthn` assertion ceremony with a caller-supplied
    /// security-key authenticator, trusts the session, and completes login.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed Apple options, authenticator failures,
    /// rejected assertions, or an incomplete account login.
    pub async fn authenticate_with_security_key(
        &mut self,
        authenticator: &mut dyn SecurityKeyAuthenticator,
    ) -> Result<AuthenticationStatus> {
        let options_request = self
            .http
            .get(self.endpoints.auth.clone())
            .headers(self.auth_headers()?);
        let options = self.send_json(options_request, &[200]).await?;
        let challenge = options
            .pointer("/fsaChallenge/challenge")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::UnexpectedResponse("security-key options have no challenge".into())
            })?
            .to_owned();
        let credential_ids = options
            .pointer("/fsaChallenge/keyHandles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if credential_ids.is_empty() {
            return Err(Error::UnexpectedResponse(
                "security-key options have no credential IDs".into(),
            ));
        }
        let relying_party_id = options
            .pointer("/fsaChallenge/rpId")
            .and_then(Value::as_str)
            .unwrap_or("apple.com")
            .to_owned();
        let assertion = authenticator
            .get_assertion(&SecurityKeyRequest {
                challenge: challenge.clone(),
                credential_ids,
                relying_party_id: relying_party_id.clone(),
            })
            .await
            .map_err(|error| Error::SecurityKey(error.to_string()))?;
        let verification_request = self
            .http
            .post(self.endpoints.auth.join("verify/security/key")?)
            .headers(self.auth_headers()?)
            .json(&json!({
                "challenge": challenge,
                "clientData": STANDARD.encode(assertion.client_data),
                "signatureData": STANDARD.encode(assertion.signature),
                "authenticatorData": STANDARD.encode(assertion.authenticator_data),
                "userHandle": assertion.user_handle.map(|value| STANDARD.encode(value)),
                "credentialID": STANDARD.encode(assertion.credential_id),
                "rpId": relying_party_id,
            }));
        self.send_json(verification_request, &[200, 204]).await?;
        let trust_request = self
            .http
            .get(self.endpoints.auth.join("2sv/trust")?)
            .headers(self.auth_headers()?);
        self.send_json(trust_request, &[200, 204]).await?;
        let data = self.account_login().await?;
        let status = self.finish_account_login(&data, false).await?;
        self.session.last_trusted_at = Some(Utc::now());
        self.session.last_authentication_method = Some("security_key".into());
        self.persist()?;
        Ok(status)
    }

    /// Fetches and accepts Apple's current iCloud terms after explicit caller
    /// confirmation, then repeats account login.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TermsOfUseRequired`] unless a confirmation token is
    /// supplied, or when Apple still requires acceptance after the operation.
    pub async fn accept_terms(
        &mut self,
        confirmation: TermsAcceptanceConfirmation,
    ) -> Result<AuthenticationStatus> {
        if !confirmation.confirmed {
            return Err(Error::TermsOfUseRequired);
        }
        let locale = self
            .session
            .pending_terms_locale
            .as_deref()
            .unwrap_or("en_US");
        let terms_request = self
            .http
            .post(self.endpoints.setup.join("getTerms")?)
            .query(&self.request_parameters())
            .json(&json!({ "locale": locale }));
        let terms = self.send_json(terms_request, &[200]).await?;
        let version = terms
            .pointer("/iCloudTerms/version")
            .cloned()
            .ok_or_else(|| {
                Error::UnexpectedResponse("getTerms response has no iCloud terms version".into())
            })?;
        let repair_request = self
            .http
            .get(self.endpoints.setup.join("repairDone")?)
            .query(&self.request_parameters())
            .json(&json!({ "acceptedICloudTerms": version }));
        self.send_json(repair_request, &[200, 204]).await?;

        let data = self.account_login().await?;
        let status = self.finish_account_login(&data, false).await?;
        if matches!(status, AuthenticationStatus::TermsOfUseRequired) {
            return Err(Error::TermsOfUseRequired);
        }
        Ok(status)
    }

    /// Refreshes Find My and returns normalized device and location data.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAuthenticated`] when account login has not completed.
    /// Network, Apple API, response parsing, and session persistence failures are
    /// also returned.
    pub async fn locate_devices(&mut self, options: LocateOptions) -> Result<Vec<Device>> {
        match self.locate_devices_once(&options).await {
            Ok(devices) => Ok(devices),
            Err(error)
                if authentication_expired(&error) || matches!(error, Error::NotAuthenticated) =>
            {
                match self.authenticate().await? {
                    AuthenticationStatus::Authenticated(_) => {}
                    AuthenticationStatus::TwoFactorRequired(_) => {
                        return Err(Error::TwoFactorRequired);
                    }
                    AuthenticationStatus::TermsOfUseRequired => {
                        return Err(Error::TermsOfUseRequired);
                    }
                }
                self.locate_devices_once(&options).await
            }
            Err(Error::Api { status: 501, .. }) => Err(Error::FindMyUnavailable),
            Err(error) => Err(error),
        }
    }

    async fn locate_devices_once(&mut self, options: &LocateOptions) -> Result<Vec<Device>> {
        let findme_url = self
            .session
            .findme_url
            .as_deref()
            .ok_or(Error::NotAuthenticated)?;
        let url = Url::parse(findme_url)?.join(REFRESH_ENDPOINT)?;
        let request = self
            .http
            .post(url)
            .query(&self.request_parameters())
            .json(&json!({
                "clientContext": {
                    "fmly": options.family,
                    "shouldLocate": true,
                    "selectedDevice": options.selected_device.as_deref(),
                    "deviceListVersion": 1
                },
                "accountCountryCode": self.session.account_country,
                "dsWebAuthToken": self.session.session_token,
                "trustToken": self.session.trust_token.as_deref().unwrap_or_default(),
                "extended_login": true
            }));
        let response = self.send_json(request, &[200]).await?;

        devices_from_response(&response, self.china_coordinates())
    }

    /// Sends a single, non-retried request to play a sound on a device.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is incomplete or Apple rejects the
    /// action. State-changing requests are intentionally never replayed.
    pub async fn play_sound(&mut self, device_id: &str, subject: &str) -> Result<()> {
        let payload = json!({
            "device": required_action_value(device_id, "device ID")?,
            "subject": required_action_value(subject, "subject")?,
            "clientContext": { "fmly": true }
        });
        self.send_find_my_action(PLAY_SOUND_ENDPOINT, payload).await
    }

    /// Sends a single, non-retried request to display a message on a device.
    ///
    /// # Errors
    ///
    /// Returns an error when required text is empty, the session is incomplete,
    /// or Apple rejects the action.
    pub async fn display_message(&mut self, request: &DisplayMessageRequest) -> Result<()> {
        let payload = json!({
            "device": required_action_value(&request.device_id, "device ID")?,
            "subject": required_action_value(&request.subject, "subject")?,
            "sound": request.sound,
            "userText": true,
            "text": required_action_value(&request.message, "message")?
        });
        self.send_find_my_action(DISPLAY_MESSAGE_ENDPOINT, payload)
            .await
    }

    /// Enables lost mode with a single, non-retried request.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LostModeConfirmationRequired`] unless the caller passes
    /// a confirmation token created by [`LostModeConfirmation::confirm`].
    pub async fn enable_lost_mode(
        &mut self,
        request: &LostModeRequest,
        confirmation: LostModeConfirmation,
    ) -> Result<()> {
        if !confirmation.confirmed {
            return Err(Error::LostModeConfirmationRequired);
        }
        let payload = json!({
            "text": required_action_value(&request.message, "message")?,
            "userText": true,
            "ownerNbr": request.phone_number.trim(),
            "lostModeEnabled": true,
            "trackingEnabled": true,
            "device": required_action_value(&request.device_id, "device ID")?,
            "passcode": request.new_passcode.trim()
        });
        self.send_find_my_action(LOST_DEVICE_ENDPOINT, payload)
            .await
    }

    async fn send_find_my_action(&mut self, endpoint: &str, payload: Value) -> Result<()> {
        let findme_url = self
            .session
            .findme_url
            .as_deref()
            .ok_or(Error::NotAuthenticated)?;
        let url = Url::parse(findme_url)?.join(endpoint)?;
        let request = self
            .http
            .post(url)
            .query(&self.request_parameters())
            .json(&payload);
        match self.send_json(request, &[200]).await {
            Err(Error::Api { status: 501, .. }) => Err(Error::FindMyUnavailable),
            result => result.map(|_| ()),
        }
    }

    /// Removes the locally persisted tokens and cookies for this account.
    ///
    /// # Errors
    ///
    /// Returns an error when the session directory cannot be removed.
    pub fn clear_session(&mut self) -> Result<()> {
        self.session_store.clear()?;
        let client_id = self.session.client_id.clone();
        self.session = SessionData {
            client_id,
            ..SessionData::default()
        };
        self.account = None;
        self.challenge = None;
        Ok(())
    }

    async fn password_sign_in(&mut self) -> Result<()> {
        let password = self.password.clone().ok_or(Error::CredentialsRequired)?;
        let srp = AppleSrp::new(&self.username)?;
        let init_request = self
            .http
            .post(self.endpoints.auth.join("signin/init")?)
            .headers(self.auth_headers()?)
            .json(&json!({
                "a": srp.public_key_base64(),
                "accountName": self.username,
                "protocols": ["s2k", "s2k_fo"]
            }));
        let init_data = self.send_json(init_request, &[200]).await?;
        let init_response: SrpInitResponse = serde_json::from_value(init_data)?;
        let proof = srp.proof(
            &password,
            &init_response,
            self.session.trust_token.as_deref(),
        )?;
        let complete_request = self
            .http
            .post(self.endpoints.auth.join("signin/complete")?)
            .query(&[("isRememberMeEnabled", "true")])
            .headers(self.auth_headers()?)
            .json(&proof);
        self.send_json(complete_request, &[200, 409]).await?;
        Ok(())
    }

    async fn account_login(&mut self) -> Result<Value> {
        let request = self
            .http
            .post(self.endpoints.setup.join("accountLogin")?)
            .query(&self.request_parameters())
            .json(&json!({
                "accountCountryCode": self.session.account_country,
                "dsWebAuthToken": self.session.session_token,
                "extended_login": true,
                "trustToken": self.session.trust_token.as_deref().unwrap_or_default(),
                "appName": "icloud-location"
            }));
        self.send_json(request, &[200]).await
    }

    async fn finish_account_login(
        &mut self,
        data: &Value,
        fetch_challenge_details: bool,
    ) -> Result<AuthenticationStatus> {
        if data
            .get("termsUpdateNeeded")
            .and_then(Value::as_bool)
            .unwrap_or_default()
        {
            self.session.pending_terms_locale = data
                .pointer("/dsInfo/languageCode")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            self.persist()?;
            return Ok(AuthenticationStatus::TermsOfUseRequired);
        }

        let ds_info = data.get("dsInfo").unwrap_or(&Value::Null);
        if ds_info
            .get("locked")
            .and_then(Value::as_bool)
            .unwrap_or_default()
        {
            return Err(Error::AccountLocked);
        }
        if let Some(dsid) = ds_info.get("dsid").and_then(value_as_string) {
            self.session.dsid = Some(dsid);
        }
        if let Some(name) = ds_info.get("fullName").and_then(Value::as_str) {
            self.session.account_name = Some(name.to_owned());
        }
        if let Some(url) = data
            .pointer("/webservices/findme/url")
            .and_then(Value::as_str)
        {
            self.session.findme_url = Some(url.to_owned());
        }

        let challenge_required = data
            .get("hsaChallengeRequired")
            .and_then(Value::as_bool)
            .unwrap_or_default();
        let trusted_browser = data
            .get("hsaTrustedBrowser")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let hsa_version = ds_info
            .get("hsaVersion")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let needs_two_factor = hsa_version == 2 && (challenge_required || !trusted_browser);

        self.persist()?;
        if needs_two_factor {
            let challenge = if fetch_challenge_details {
                self.fetch_challenge_details()
                    .await
                    .ok()
                    .filter(TwoFactorChallenge::has_methods)
                    .or_else(|| self.session.challenge_metadata.clone())
                    .unwrap_or_default()
            } else {
                self.session.challenge_metadata.clone().unwrap_or_default()
            };
            self.session.challenge_metadata = Some(challenge.for_persistence());
            self.persist()?;
            self.challenge = Some(challenge.clone());
            return Ok(AuthenticationStatus::TwoFactorRequired(challenge));
        }

        if self.session.findme_url.is_none() {
            return Err(Error::UnexpectedResponse(
                "account login did not provide the Find My service URL".into(),
            ));
        }

        let account = Account {
            username: self.username.clone(),
            name: self.session.account_name.clone(),
            country: self.session.account_country.clone(),
            locked: false,
        };
        self.account = Some(account.clone());
        self.challenge = None;
        self.session.pending_terms_locale = None;
        self.persist()?;
        Ok(AuthenticationStatus::Authenticated(account))
    }

    async fn fetch_challenge_details(&mut self) -> Result<TwoFactorChallenge> {
        let request = self
            .http
            .get(self.endpoints.auth.clone())
            .headers(self.auth_headers()?);
        let data = self.send_json(request, &[200]).await?;
        Ok(TwoFactorChallenge::from_apple(&data))
    }

    fn auth_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/javascript"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        insert_static_header(&mut headers, "x-apple-oauth-client-id", OAUTH_CLIENT_ID);
        insert_static_header(&mut headers, "x-apple-oauth-client-type", "firstPartyAuth");
        insert_static_header(
            &mut headers,
            "x-apple-oauth-redirect-uri",
            "https://www.icloud.com",
        );
        insert_static_header(&mut headers, "x-apple-oauth-require-grant-code", "true");
        insert_static_header(&mut headers, "x-apple-oauth-response-mode", "web_message");
        insert_static_header(&mut headers, "x-apple-oauth-response-type", "code");
        insert_static_header(&mut headers, "x-apple-widget-key", OAUTH_CLIENT_ID);
        insert_header(&mut headers, "x-apple-oauth-state", &self.session.client_id)?;
        if let Some(scnt) = self.session.scnt.as_deref() {
            insert_header(&mut headers, "scnt", scnt)?;
        }
        if let Some(session_id) = self.session.session_id.as_deref() {
            insert_header(&mut headers, "x-apple-id-session-id", session_id)?;
        }
        Ok(headers)
    }

    fn request_parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("clientBuildNumber", "2021Project52".into()),
            ("clientMasteringNumber", "2021B29".into()),
            ("ckjsBuildVersion", "17DProjectDev77".into()),
            (
                "clientId",
                self.session
                    .client_id
                    .strip_prefix("auth-")
                    .unwrap_or(&self.session.client_id)
                    .to_owned(),
            ),
        ]
    }

    fn china_coordinates(&self) -> ChinaCoordinates {
        match self.region {
            Region::Global => ChinaCoordinates::Unchanged,
            Region::China { coordinates } => coordinates,
        }
    }

    async fn send_json(&mut self, request: RequestBuilder, allowed: &[u16]) -> Result<Value> {
        let response = request.send().await?;
        let status = response.status();
        self.capture_session_headers(response.headers())?;
        let body = response.text().await?;
        self.persist()?;
        let data = if body.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&body).unwrap_or_else(|_| Value::String(body.clone()))
        };

        if status.is_success() {
            if let Some(error) = embedded_api_error(status, &data) {
                return Err(error);
            }
            return Ok(data);
        }
        if allowed.contains(&status.as_u16()) {
            return Ok(data);
        }

        Err(api_error(status, &data, &body))
    }

    fn capture_session_headers(&mut self, headers: &HeaderMap) -> Result<()> {
        capture_header(
            headers,
            "x-apple-id-account-country",
            &mut self.session.account_country,
        )?;
        capture_header(
            headers,
            "x-apple-id-session-id",
            &mut self.session.session_id,
        )?;
        capture_header(
            headers,
            "x-apple-session-token",
            &mut self.session.session_token,
        )?;
        capture_header(
            headers,
            "x-apple-twosv-trust-token",
            &mut self.session.trust_token,
        )?;
        capture_header(headers, "scnt", &mut self.session.scnt)?;
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        self.session_store.save(&self.session, &self.cookies)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Account {
    pub username: String,
    pub name: Option<String>,
    pub country: Option<String>,
    pub locked: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", content = "details", rename_all = "snake_case")]
pub enum AuthenticationStatus {
    Authenticated(Account),
    TwoFactorRequired(TwoFactorChallenge),
    TermsOfUseRequired,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TwoFactorChallenge {
    pub trusted_phone_numbers: Vec<TrustedPhoneNumber>,
    pub security_key_names: Vec<String>,
}

impl TwoFactorChallenge {
    fn has_methods(&self) -> bool {
        !self.trusted_phone_numbers.is_empty() || !self.security_key_names.is_empty()
    }

    fn for_persistence(&self) -> Self {
        Self {
            trusted_phone_numbers: self
                .trusted_phone_numbers
                .iter()
                .map(|phone| TrustedPhoneNumber {
                    id: phone.id,
                    last_two_digits: None,
                    number_with_dial_code: None,
                })
                .collect(),
            security_key_names: self.security_key_names.clone(),
        }
    }

    fn from_apple(data: &Value) -> Self {
        let phone_values = data
            .get("trustedPhoneNumbers")
            .or_else(|| data.pointer("/phoneNumberVerification/trustedPhoneNumbers"))
            .and_then(Value::as_array);
        let trusted_phone_numbers = phone_values
            .into_iter()
            .flatten()
            .filter_map(TrustedPhoneNumber::from_apple)
            .collect();
        let security_key_names = data
            .get("keyNames")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| {
                value.as_str().map(ToOwned::to_owned).or_else(|| {
                    value
                        .get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
            })
            .collect();

        Self {
            trusted_phone_numbers,
            security_key_names,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TrustedPhoneNumber {
    pub id: u64,
    pub last_two_digits: Option<String>,
    pub number_with_dial_code: Option<String>,
}

pub struct TrustedSessionSnapshot {
    account_country: Option<String>,
    session_id: Option<String>,
    session_token: Option<String>,
    trust_token: Option<String>,
    scnt: Option<String>,
    dsid: Option<String>,
    findme_url: Option<String>,
    account_name: Option<String>,
}

impl TrustedSessionSnapshot {
    #[must_use]
    pub fn has_trust_token(&self) -> bool {
        self.trust_token
            .as_deref()
            .is_some_and(|token| !token.is_empty())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrustCookieStatus {
    pub expires_at: DateTime<Utc>,
    pub days_remaining: i64,
    pub reauthentication_recommended: bool,
}

impl TrustedPhoneNumber {
    fn from_apple(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_u64()?,
            last_two_digits: value
                .get("lastTwoDigits")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            number_with_dial_code: value
                .get("numberWithDialCode")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationMethod {
    TrustedDevice,
    Sms { phone_id: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayMessageRequest {
    pub device_id: String,
    pub subject: String,
    pub message: String,
    pub sound: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LostModeRequest {
    pub device_id: String,
    pub phone_number: String,
    pub message: String,
    pub new_passcode: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LostModeConfirmation {
    confirmed: bool,
}

impl LostModeConfirmation {
    #[must_use]
    pub const fn confirm() -> Self {
        Self { confirmed: true }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TermsAcceptanceConfirmation {
    confirmed: bool,
}

impl TermsAcceptanceConfirmation {
    #[must_use]
    pub const fn confirm() -> Self {
        Self { confirmed: true }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocateOptions {
    pub family: bool,
    pub selected_device: Option<String>,
}

impl Default for LocateOptions {
    fn default() -> Self {
        Self::family()
    }
}

impl LocateOptions {
    #[must_use]
    pub fn family() -> Self {
        Self {
            family: true,
            selected_device: None,
        }
    }

    #[must_use]
    pub fn owner() -> Self {
        Self {
            family: false,
            selected_device: None,
        }
    }

    #[must_use]
    pub fn selected(mut self, device_id: impl Into<String>) -> Self {
        self.selected_device = Some(device_id.into());
        self
    }
}

struct Endpoints {
    home: Url,
    setup: Url,
    auth: Url,
}

impl Endpoints {
    fn new(region: Region) -> Result<Self> {
        let (home, setup) = match region {
            Region::Global => (
                "https://www.icloud.com/",
                "https://setup.icloud.com/setup/ws/1/",
            ),
            Region::China { .. } => (
                "https://www.icloud.com.cn/",
                "https://setup.icloud.com.cn/setup/ws/1/",
            ),
        };
        Ok(Self {
            home: Url::parse(home)?,
            setup: Url::parse(setup)?,
            auth: Url::parse("https://idmsa.apple.com/appleauth/auth/")?,
        })
    }
}

fn api_error(status: StatusCode, data: &Value, body: &str) -> Error {
    let code = ["errorCode", "serverErrorCode"]
        .into_iter()
        .find_map(|key| data.get(key).filter(|value| meaningful_apple_value(value)))
        .map(Value::to_string)
        .map_or_else(String::new, |code| format!(", code {code}"));
    let message = ["error", "errorMessage", "reason", "errorReason"]
        .into_iter()
        .find_map(|key| data.get(key).filter(|value| meaningful_apple_value(value)))
        .and_then(value_as_string)
        .unwrap_or_else(|| {
            if matches!(data, Value::String(_)) {
                let trimmed = body.trim();
                if !trimmed.is_empty() {
                    return trimmed.chars().take(500).collect();
                }
            }
            status
                .canonical_reason()
                .unwrap_or("unknown error")
                .to_owned()
        });
    Error::Api {
        status: status.as_u16(),
        code,
        message,
    }
}

fn authentication_expired(error: &Error) -> bool {
    matches!(
        error,
        Error::Api {
            status: 401 | 403 | 421 | 450 | 500,
            ..
        }
    )
}

fn embedded_api_error(status: StatusCode, data: &Value) -> Option<Error> {
    let reason = ["error", "errorMessage", "reason", "errorReason"]
        .into_iter()
        .find_map(|key| data.get(key).filter(|value| meaningful_apple_value(value)))?;
    if matches!(reason, Value::Number(number) if number.as_i64() == Some(1))
        || reason.as_str() == Some("2fa Already Processed")
    {
        return None;
    }
    Some(api_error(status, data, ""))
}

fn meaningful_apple_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_i64() != Some(0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn verification_rejected(error: &Error) -> bool {
    matches!(
        error,
        Error::Api {
            status: 400 | 401,
            ..
        }
    ) || matches!(error, Error::Api { code, .. } if code.contains("-21669"))
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn capture_header(headers: &HeaderMap, name: &str, destination: &mut Option<String>) -> Result<()> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(invalid_header)?;
    if let Some(value) = headers.get(name) {
        *destination = Some(value.to_str().map_err(invalid_header)?.to_owned());
    }
    Ok(())
}

fn insert_static_header(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    );
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_str(value).map_err(invalid_header)?,
    );
    Ok(())
}

fn invalid_header(error: impl std::fmt::Display) -> Error {
    Error::UnexpectedResponse(format!("invalid HTTP header: {error}"))
}

fn required_action_value<'a>(value: &'a str, name: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::UnexpectedResponse(format!(
            "Find My action {name} cannot be empty"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct StaticCredentialProvider;

    impl CredentialProvider for StaticCredentialProvider {
        fn password(
            &self,
            account: &str,
        ) -> std::result::Result<Option<SecretString>, icloud_location_core::CredentialError>
        {
            assert_eq!(account, "test@example.invalid");
            Ok(Some(SecretString::from("provider-password")))
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        let header_end = loop {
            let bytes_read = stream.read(&mut buffer).unwrap();
            assert_ne!(bytes_read, 0, "request ended before its headers");
            request.extend_from_slice(&buffer[..bytes_read]);
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or_default();
        while request.len() < header_end + content_length {
            let bytes_read = stream.read(&mut buffer).unwrap();
            assert_ne!(bytes_read, 0, "request ended before its body");
            request.extend_from_slice(&buffer[..bytes_read]);
        }
        String::from_utf8(request).unwrap()
    }

    fn respond(stream: &mut TcpStream, status: u16, body: &Value) {
        let reason = match status {
            200 => "OK",
            421 => "Misdirected Request",
            501 => "Not Implemented",
            _ => "Error",
        };
        let body = body.to_string();
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    fn temporary_root(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "icloud-location-{test_name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn parses_two_factor_challenge() {
        let challenge = TwoFactorChallenge::from_apple(&json!({
            "phoneNumberVerification": {
                "trustedPhoneNumbers": [
                    { "id": 1, "lastTwoDigits": "42", "numberWithDialCode": "+•• ••• ••42" },
                    { "id": 2, "lastTwoDigits": "17", "numberWithDialCode": "+•• ••• ••17" }
                ]
            },
            "keyNames": ["Blue key", { "name": "Backup key" }]
        }));

        assert_eq!(challenge.trusted_phone_numbers.len(), 2);
        assert_eq!(challenge.trusted_phone_numbers[0].id, 1);
        assert_eq!(
            challenge.trusted_phone_numbers[0]
                .last_two_digits
                .as_deref(),
            Some("42")
        );
        assert_eq!(challenge.security_key_names, ["Blue key", "Backup key"]);
    }

    #[tokio::test]
    async fn requests_and_verifies_sms_for_the_selected_phone() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let body = if index == 3 {
                    json!({
                        "dsInfo": {
                            "dsid": "12345",
                            "hsaVersion": 2,
                            "locked": false
                        },
                        "hsaChallengeRequired": false,
                        "hsaTrustedBrowser": true,
                        "webservices": {
                            "findme": { "url": format!("http://{address}/") }
                        }
                    })
                } else {
                    Value::Null
                };
                respond(&mut stream, 200, &body);
            }
            requests
        });
        let root = temporary_root("sms-state-machine-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        let local = Url::parse(&format!("http://{address}/")).unwrap();
        client.endpoints.auth = local.join("appleauth/auth/").unwrap();
        client.endpoints.setup = local.join("setup/ws/1/").unwrap();
        client.session.session_id = Some("session-id".into());
        client.session.scnt = Some("scnt".into());
        client.session.session_token = Some("session-token".into());
        client.session.account_country = Some("SE".into());

        client
            .request_verification_code(VerificationMethod::Sms { phone_id: 2 })
            .await
            .unwrap();
        let status = client
            .verify_verification_code(VerificationMethod::Sms { phone_id: 2 }, "123456")
            .await
            .unwrap();
        let requests = server.join().unwrap();

        assert!(matches!(status, AuthenticationStatus::Authenticated(_)));
        assert!(requests[0].starts_with("PUT /appleauth/auth/verify/phone"));
        assert!(requests[0].contains("\"phoneNumber\":{\"id\":2}"));
        assert!(requests[1].starts_with("POST /appleauth/auth/verify/phone/securitycode"));
        assert!(requests[1].contains("\"code\":\"123456\""));
        assert!(requests[1].contains("\"phoneNumber\":{\"id\":2}"));
        assert!(requests[2].starts_with("GET /appleauth/auth/2sv/trust"));
        assert!(requests[3].starts_with("POST /setup/ws/1/accountLogin"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn validate_reports_invalid_and_two_factor_required_sessions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut invalid_stream, _) = listener.accept().unwrap();
            let invalid_request = read_request(&mut invalid_stream);
            respond(
                &mut invalid_stream,
                401,
                &json!({ "error": "invalid session" }),
            );
            let (mut two_factor_stream, _) = listener.accept().unwrap();
            let two_factor_request = read_request(&mut two_factor_stream);
            respond(
                &mut two_factor_stream,
                200,
                &json!({
                    "dsInfo": { "hsaVersion": 2, "locked": false },
                    "hsaChallengeRequired": true,
                    "hsaTrustedBrowser": false
                }),
            );
            [invalid_request, two_factor_request]
        });
        let local = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();

        let invalid_root = temporary_root("invalid-validate-test");
        let mut invalid = ClientBuilder::new("invalid@example.invalid")
            .session_root(&invalid_root)
            .build()
            .unwrap();
        invalid.endpoints.setup = local.clone();
        invalid.session.session_token = Some("invalid-token".into());
        invalid.session.account_country = Some("SE".into());
        let error = invalid.validate_session().await.unwrap_err();
        assert!(matches!(error, Error::Api { status: 401, .. }));

        let two_factor_root = temporary_root("two-factor-validate-test");
        let mut two_factor = ClientBuilder::new("two-factor@example.invalid")
            .session_root(&two_factor_root)
            .build()
            .unwrap();
        two_factor.endpoints.setup = local;
        two_factor.endpoints.auth = Url::parse("http://127.0.0.1:9/appleauth/auth/").unwrap();
        two_factor.session.session_token = Some("two-factor-token".into());
        two_factor.session.account_country = Some("SE".into());
        two_factor.session.challenge_metadata = Some(TwoFactorChallenge {
            trusted_phone_numbers: vec![
                TrustedPhoneNumber {
                    id: 1,
                    last_two_digits: Some("42".into()),
                    number_with_dial_code: None,
                },
                TrustedPhoneNumber {
                    id: 2,
                    last_two_digits: Some("17".into()),
                    number_with_dial_code: None,
                },
            ],
            security_key_names: Vec::new(),
        });
        let status = two_factor.validate_session().await.unwrap();
        let AuthenticationStatus::TwoFactorRequired(challenge) = status else {
            panic!("expected two-factor challenge");
        };
        assert_eq!(challenge.trusted_phone_numbers.len(), 2);

        let requests = server.join().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.contains("/validate?"))
        );
        fs::remove_dir_all(invalid_root).unwrap();
        fs::remove_dir_all(two_factor_root).unwrap();
    }

    #[test]
    fn global_and_china_endpoints_are_distinct() {
        let global = Endpoints::new(Region::Global).unwrap();
        let china = Endpoints::new(Region::China {
            coordinates: ChinaCoordinates::Gcj02,
        })
        .unwrap();

        assert_eq!(global.setup.host_str(), Some("setup.icloud.com"));
        assert_eq!(china.setup.host_str(), Some("setup.icloud.com.cn"));
    }

    #[test]
    fn locate_options_default_to_family_devices() {
        let options = LocateOptions::default();
        assert!(options.family);
        assert!(options.selected_device.is_none());
    }

    #[test]
    fn resolves_credentials_through_the_platform_neutral_provider() {
        let root = temporary_root("credential-provider-test");
        let client = ClientBuilder::new(" test@example.invalid ")
            .session_root(&root)
            .credential_provider(&StaticCredentialProvider)
            .unwrap()
            .build()
            .unwrap();

        assert!(client.password.is_some());
        assert!(!root.exists());
    }

    #[test]
    fn detects_an_error_inside_a_successful_response() {
        let error = embedded_api_error(
            StatusCode::OK,
            &json!({ "errorReason": "AUTHENTICATION_FAILED", "errorCode": -1 }),
        )
        .unwrap();

        assert!(matches!(error, Error::Api { status: 200, .. }));
    }

    #[test]
    fn api_errors_do_not_include_unrecognized_json_fields() {
        let error = api_error(
            StatusCode::UNAUTHORIZED,
            &json!({ "dsWebAuthToken": "sensitive-token" }),
            r#"{"dsWebAuthToken":"sensitive-token"}"#,
        );

        assert!(!error.to_string().contains("sensitive-token"));
        assert!(error.to_string().contains("Unauthorized"));
    }

    #[tokio::test]
    async fn sends_a_find_my_refresh_and_normalizes_the_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let header_end = loop {
                let bytes_read = stream.read(&mut buffer).unwrap();
                assert_ne!(bytes_read, 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..bytes_read]);
                if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let bytes_read = stream.read(&mut buffer).unwrap();
                assert_ne!(bytes_read, 0, "request ended before its body");
                request.extend_from_slice(&buffer[..bytes_read]);
            }

            let response = json!({
                "content": [{
                    "id": "device-id",
                    "name": "Test iPhone",
                    "deviceStatus": 200,
                    "batteryLevel": 0.42,
                    "location": {
                        "latitude": 59.3293,
                        "longitude": 18.0686,
                        "horizontalAccuracy": 5.0,
                        "timeStamp": 1_750_000_000_123_i64
                    }
                }]
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .unwrap();
            request
        });

        let root = std::env::temp_dir().join(format!(
            "icloud-location-http-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.session.findme_url = Some(format!("http://{address}/"));
        client.session.account_country = Some("SE".into());
        client.session.session_token = Some("test-token".into());
        client.session.trust_token = Some("test-trust-token".into());

        let devices = client
            .locate_devices(LocateOptions::family().selected("device-id"))
            .await
            .unwrap();
        let request = String::from_utf8(server.join().unwrap()).unwrap();

        assert!(request.starts_with("POST /fmipservice/client/web/refreshClient?"));
        assert!(request.contains("clientBuildNumber=2021Project52"));
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let origin = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("origin").then(|| value.trim())
            })
            .unwrap();
        assert_eq!(origin, "https://www.icloud.com");
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body.pointer("/clientContext/fmly"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            body.pointer("/clientContext/selectedDevice"),
            Some(&Value::String("device-id".into()))
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "device-id");
        assert_eq!(devices[0].battery.as_ref().unwrap().level_percent, Some(42));
        assert!((devices[0].location.as_ref().unwrap().latitude - 59.3293).abs() < f64::EPSILON);

        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn validates_a_saved_session_without_a_password() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            respond(
                &mut stream,
                200,
                &json!({
                    "dsInfo": {
                        "dsid": "12345",
                        "fullName": "Test Person",
                        "hsaVersion": 2,
                        "locked": false
                    },
                    "hsaChallengeRequired": false,
                    "hsaTrustedBrowser": true,
                    "webservices": {
                        "findme": { "url": format!("http://{address}/") }
                    }
                }),
            );
            request
        });
        let root = temporary_root("validate-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
        client.session.session_token = Some("saved-token".into());
        client.session.account_country = Some("SE".into());

        let status = client.validate_session().await.unwrap();
        let request = server.join().unwrap();

        assert!(matches!(status, AuthenticationStatus::Authenticated(_)));
        assert!(request.starts_with("POST /setup/ws/1/validate?"));
        assert!(request.ends_with("\r\n\r\nnull"));
        assert!(client.password.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn applies_the_configured_request_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            thread::sleep(std::time::Duration::from_millis(100));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnull"
            );
            request
        });
        let root = temporary_root("timeout-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .timeout(std::time::Duration::from_millis(20))
            .build()
            .unwrap();
        client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
        client.session.session_token = Some("saved-token".into());
        client.session.account_country = Some("SE".into());

        let error = client.validate_session().await.unwrap_err();

        assert!(matches!(error, Error::Http(error) if error.is_timeout()));
        assert!(server.join().unwrap().contains("/validate?"));
        if root.exists() {
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_a_malformed_successful_find_my_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json"
            )
            .unwrap();
            request
        });
        let root = temporary_root("malformed-find-my-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.session.findme_url = Some(format!("http://{address}/"));
        client.session.session_token = Some("saved-token".into());
        client.session.account_country = Some("SE".into());

        let error = client
            .locate_devices(LocateOptions::family())
            .await
            .unwrap_err();

        assert!(matches!(error, Error::UnexpectedResponse(_)));
        assert!(server.join().unwrap().contains("/refreshClient?"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn validates_good_and_rejects_bad_credentials_with_lightweight_srp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let response = if index == 0 {
                    json!({
                        "iteration": 1,
                        "salt": STANDARD.encode([0_u8; 16]),
                        "protocol": "s2k",
                        "b": STANDARD.encode([1_u8]),
                        "c": "challenge"
                    })
                } else {
                    Value::Null
                };
                respond(&mut stream, 200, &response);
            }
            requests
        });
        let root = temporary_root("credential-valid-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .password("valid-password")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.auth = Url::parse(&format!("http://{address}/appleauth/auth/")).unwrap();

        client.validate_credentials().await.unwrap();
        let requests = server.join().unwrap();

        assert!(requests[0].starts_with("POST /appleauth/auth/signin/init"));
        assert!(requests[1].starts_with("POST /appleauth/auth/signin/complete"));
        fs::remove_dir_all(root).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            respond(&mut stream, 401, &json!({ "error": "invalid credentials" }));
            request
        });
        let root = temporary_root("credential-invalid-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .password("invalid-password")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.auth = Url::parse(&format!("http://{address}/appleauth/auth/")).unwrap();

        let error = client.validate_credentials().await.unwrap_err();

        assert!(matches!(error, Error::Api { status: 401, .. }));
        assert!(server.join().unwrap().contains("signin/init"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn completes_mocked_security_key_options_assertion_and_submission() {
        struct MockSecurityKey {
            request: Option<SecurityKeyRequest>,
        }

        impl SecurityKeyAuthenticator for MockSecurityKey {
            fn get_assertion<'a>(
                &'a mut self,
                request: &'a SecurityKeyRequest,
            ) -> icloud_location_core::BoxFuture<
                'a,
                std::result::Result<
                    crate::SecurityKeyAssertion,
                    crate::security_key::SecurityKeyError,
                >,
            > {
                self.request = Some(request.clone());
                Box::pin(async {
                    Ok(crate::SecurityKeyAssertion {
                        client_data: b"client-data".to_vec(),
                        signature: b"signature".to_vec(),
                        authenticator_data: b"authenticator-data".to_vec(),
                        user_handle: Some(b"user".to_vec()),
                        credential_id: b"credential".to_vec(),
                    })
                })
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let body = match index {
                    0 => json!({
                        "fsaChallenge": {
                            "challenge": "Y2hhbGxlbmdl",
                            "keyHandles": ["Y3JlZGVudGlhbA"],
                            "rpId": "apple.com"
                        }
                    }),
                    3 => json!({
                        "dsInfo": {
                            "dsid": "12345",
                            "hsaVersion": 2,
                            "locked": false
                        },
                        "hsaChallengeRequired": false,
                        "hsaTrustedBrowser": true,
                        "webservices": {
                            "findme": { "url": format!("http://{address}/") }
                        }
                    }),
                    _ => Value::Null,
                };
                respond(&mut stream, 200, &body);
            }
            requests
        });
        let root = temporary_root("security-key-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        let local = Url::parse(&format!("http://{address}/")).unwrap();
        client.endpoints.auth = local.join("appleauth/auth/").unwrap();
        client.endpoints.setup = local.join("setup/ws/1/").unwrap();
        let mut key = MockSecurityKey { request: None };

        let status = client
            .authenticate_with_security_key(&mut key)
            .await
            .unwrap();
        let requests = server.join().unwrap();

        assert!(matches!(status, AuthenticationStatus::Authenticated(_)));
        assert_eq!(key.request.unwrap().relying_party_id, "apple.com");
        assert!(requests[0].starts_with("GET /appleauth/auth/"));
        assert!(requests[1].contains("/appleauth/auth/verify/security/key"));
        let submission: Value = serde_json::from_str(
            requests[1]
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(submission["clientData"], STANDARD.encode(b"client-data"));
        assert_eq!(submission["credentialID"], STANDARD.encode(b"credential"));
        assert!(requests[2].contains("/appleauth/auth/2sv/trust"));
        assert!(requests[3].contains("/setup/ws/1/accountLogin"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reauthenticates_and_replays_a_find_my_read_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                match index {
                    0 => respond(&mut stream, 421, &json!({ "error": "expired" })),
                    1 => respond(
                        &mut stream,
                        200,
                        &json!({
                            "dsInfo": {
                                "dsid": "12345",
                                "fullName": "Test Person",
                                "hsaVersion": 2,
                                "locked": false
                            },
                            "hsaChallengeRequired": false,
                            "hsaTrustedBrowser": true,
                            "webservices": {
                                "findme": { "url": format!("http://{address}/") }
                            }
                        }),
                    ),
                    _ => respond(
                        &mut stream,
                        200,
                        &json!({
                            "content": [{
                                "id": "device-id",
                                "name": "Test iPhone",
                                "deviceStatus": 200
                            }]
                        }),
                    ),
                }
            }
            requests
        });
        let root = temporary_root("replay-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
        client.session.findme_url = Some(format!("http://{address}/"));
        client.session.session_token = Some("saved-token".into());
        client.session.account_country = Some("SE".into());

        let devices = client
            .locate_devices(LocateOptions::family())
            .await
            .unwrap();
        let requests = server.join().unwrap();

        assert_eq!(devices[0].id, "device-id");
        assert!(requests[0].starts_with("POST /fmipservice/client/web/refreshClient?"));
        assert!(requests[1].starts_with("POST /setup/ws/1/accountLogin?"));
        assert!(requests[2].starts_with("POST /fmipservice/client/web/refreshClient?"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn authenticates_before_the_first_find_my_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let body = if index == 0 {
                    json!({
                        "dsInfo": {
                            "dsid": "12345",
                            "hsaVersion": 2,
                            "locked": false
                        },
                        "hsaChallengeRequired": false,
                        "hsaTrustedBrowser": true,
                        "webservices": {
                            "findme": { "url": format!("http://{address}/") }
                        }
                    })
                } else {
                    json!({
                        "content": [{
                            "id": "device-id",
                            "name": "Test iPhone",
                            "deviceStatus": 200
                        }]
                    })
                };
                respond(&mut stream, 200, &body);
            }
            requests
        });
        let root = temporary_root("initial-find-my-authentication-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
        client.session.session_token = Some("saved-token".into());
        client.session.account_country = Some("SE".into());

        let devices = client
            .locate_devices(LocateOptions::family())
            .await
            .unwrap();
        let requests = server.join().unwrap();

        assert_eq!(devices[0].id, "device-id");
        assert!(requests[0].starts_with("POST /setup/ws/1/accountLogin?"));
        assert!(requests[1].starts_with("POST /fmipservice/client/web/refreshClient?"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovers_find_my_reads_from_450_and_500_once() {
        for expired_status in [450, 500] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let mut requests = Vec::new();
                for index in 0..3 {
                    let (mut stream, _) = listener.accept().unwrap();
                    requests.push(read_request(&mut stream));
                    match index {
                        0 => respond(&mut stream, expired_status, &json!({ "error": "expired" })),
                        1 => respond(
                            &mut stream,
                            200,
                            &json!({
                                "dsInfo": {
                                    "dsid": "12345",
                                    "hsaVersion": 2,
                                    "locked": false
                                },
                                "hsaChallengeRequired": false,
                                "hsaTrustedBrowser": true,
                                "webservices": {
                                    "findme": { "url": format!("http://{address}/") }
                                }
                            }),
                        ),
                        _ => respond(
                            &mut stream,
                            200,
                            &json!({
                                "content": [{
                                    "id": "device-id",
                                    "name": "Test iPhone",
                                    "deviceStatus": 200
                                }]
                            }),
                        ),
                    }
                }
                requests
            });
            let root = temporary_root(&format!("replay-{expired_status}-test"));
            let mut client = ClientBuilder::new("test@example.invalid")
                .session_root(&root)
                .build()
                .unwrap();
            client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
            client.session.findme_url = Some(format!("http://{address}/"));
            client.session.session_token = Some("saved-token".into());

            let devices = client
                .locate_devices(LocateOptions::family())
                .await
                .unwrap();

            assert_eq!(devices[0].id, "device-id");
            assert_eq!(server.join().unwrap().len(), 3);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn reports_account_lock_and_find_my_service_unavailability() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            respond(
                &mut stream,
                200,
                &json!({
                    "dsInfo": { "hsaVersion": 2, "locked": true }
                }),
            );
            request
        });
        let root = temporary_root("locked-account-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
        client.session.session_token = Some("saved-token".into());

        let error = client.authenticate().await.unwrap_err();

        assert!(matches!(error, Error::AccountLocked));
        assert!(server.join().unwrap().contains("accountLogin"));
        fs::remove_dir_all(root).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            respond(&mut stream, 501, &json!({ "error": "unavailable" }));
            request
        });
        let root = temporary_root("find-my-unavailable-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.session.findme_url = Some(format!("http://{address}/"));
        client.session.session_token = Some("saved-token".into());

        let error = client
            .locate_devices(LocateOptions::family())
            .await
            .unwrap_err();

        assert!(matches!(error, Error::FindMyUnavailable));
        assert!(server.join().unwrap().contains(REFRESH_ENDPOINT));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn sends_exact_find_my_action_payloads_after_lost_mode_confirmation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                respond(&mut stream, 200, &Value::Null);
            }
            requests
        });
        let root = temporary_root("actions-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.session.findme_url = Some(format!("http://{address}/"));
        client.session.session_token = Some("saved-token".into());

        client.play_sound("device-id", "Find Me").await.unwrap();
        client
            .display_message(&DisplayMessageRequest {
                device_id: "device-id".into(),
                subject: "Hello".into(),
                message: "Please call".into(),
                sound: true,
            })
            .await
            .unwrap();
        let lost_request = LostModeRequest {
            device_id: "device-id".into(),
            phone_number: "+4612345".into(),
            message: "Lost phone".into(),
            new_passcode: "123456".into(),
        };
        let rejected = client
            .enable_lost_mode(&lost_request, LostModeConfirmation::default())
            .await;
        assert!(matches!(rejected, Err(Error::LostModeConfirmationRequired)));
        client
            .enable_lost_mode(&lost_request, LostModeConfirmation::confirm())
            .await
            .unwrap();
        let requests = server.join().unwrap();

        let paths_and_bodies: Vec<_> = requests
            .iter()
            .map(|request| {
                let (headers, body) = request.split_once("\r\n\r\n").unwrap();
                let path = headers.lines().next().unwrap();
                (path, serde_json::from_str::<Value>(body).unwrap())
            })
            .collect();
        assert!(paths_and_bodies[0].0.contains(PLAY_SOUND_ENDPOINT));
        assert_eq!(paths_and_bodies[0].1["clientContext"]["fmly"], true);
        assert!(paths_and_bodies[1].0.contains(DISPLAY_MESSAGE_ENDPOINT));
        assert_eq!(paths_and_bodies[1].1["text"], "Please call");
        assert!(paths_and_bodies[2].0.contains(LOST_DEVICE_ENDPOINT));
        assert_eq!(paths_and_bodies[2].1["lostModeEnabled"], true);
        assert_eq!(paths_and_bodies[2].1["trackingEnabled"], true);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn does_not_replay_a_state_changing_find_my_action() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            respond(&mut stream, 421, &json!({ "error": "expired" }));
            request
        });
        let root = temporary_root("no-action-replay-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.session.findme_url = Some(format!("http://{address}/"));
        client.session.session_token = Some("saved-token".into());

        let result = client.play_sound("device-id", "Find Me").await;
        let request = server.join().unwrap();

        assert!(matches!(result, Err(Error::Api { status: 421, .. })));
        assert!(request.contains(PLAY_SOUND_ENDPOINT));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn caches_challenge_metadata_across_restart() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            respond(
                &mut stream,
                200,
                &json!({
                    "trustedPhoneNumbers": [{
                        "id": 7,
                        "lastTwoDigits": "42",
                        "numberWithDialCode": "+•• ••• ••42"
                    }],
                    "keyNames": ["Primary key"]
                }),
            );
            request
        });
        let root = temporary_root("challenge-cache-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.auth = Url::parse(&format!("http://{address}/appleauth/auth/")).unwrap();
        client.session.session_token = Some("saved-token".into());
        let status = client
            .finish_account_login(
                &json!({
                    "dsInfo": { "hsaVersion": 2, "locked": false },
                    "hsaChallengeRequired": true,
                    "hsaTrustedBrowser": false
                }),
                true,
            )
            .await
            .unwrap();
        let request = server.join().unwrap();
        assert!(matches!(status, AuthenticationStatus::TwoFactorRequired(_)));
        assert!(request.starts_with("GET /appleauth/auth/"));
        drop(client);

        let restarted = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        let cached = restarted.cached_challenge().unwrap();
        assert_eq!(cached.trusted_phone_numbers[0].id, 7);
        assert_eq!(cached.trusted_phone_numbers[0].last_two_digits, None);
        assert_eq!(cached.trusted_phone_numbers[0].number_with_dial_code, None);
        assert_eq!(cached.security_key_names, ["Primary key"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn snapshots_untrusts_and_restores_a_trusted_session() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            respond(
                &mut stream,
                200,
                &json!({
                    "dsInfo": {
                        "dsid": "12345",
                        "fullName": "Test Person",
                        "hsaVersion": 2,
                        "locked": false
                    },
                    "hsaChallengeRequired": false,
                    "hsaTrustedBrowser": true,
                    "webservices": {
                        "findme": { "url": format!("http://{address}/") }
                    }
                }),
            );
            request
        });
        let root = temporary_root("trusted-snapshot-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
        client.session.account_country = Some("SE".into());
        client.session.session_token = Some("saved-token".into());
        client.session.trust_token = Some("saved-trust-token".into());
        client.session.dsid = Some("12345".into());
        let snapshot = client.snapshot_trusted_session();
        assert!(snapshot.has_trust_token());

        client.untrust_session().unwrap();
        assert!(!client.session.has_session_token());
        let status = client.restore_trusted_session(snapshot).await.unwrap();
        let request = server.join().unwrap();

        assert!(matches!(status, AuthenticationStatus::Authenticated(_)));
        assert!(request.contains("saved-trust-token"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_trust_cookie_expiry_and_reauthentication_window() {
        let root = temporary_root("trust-expiry-test");
        let client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        let apple_url = Url::parse("https://apple.com/").unwrap();
        client
            .cookies
            .lock()
            .unwrap()
            .parse(
                "X-APPLE-WEBAUTH-HSA-TRUST=value; Domain=apple.com; Path=/; Expires=Thu, 30 Oct 2026 12:00:00 GMT; Secure",
                &apple_url,
            )
            .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let status = client.trust_cookie_status(now).unwrap().unwrap();

        assert_eq!(status.days_remaining, 29);
        assert!(status.reauthentication_recommended);
    }

    #[tokio::test]
    async fn proactively_reauthenticates_when_the_trust_cookie_nears_expiry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let response = match index {
                    0 => json!({
                        "iteration": 1,
                        "salt": STANDARD.encode([0_u8; 16]),
                        "protocol": "s2k",
                        "b": STANDARD.encode([1_u8]),
                        "c": "challenge"
                    }),
                    2 => json!({
                        "dsInfo": {
                            "dsid": "12345",
                            "hsaVersion": 2,
                            "locked": false
                        },
                        "hsaChallengeRequired": false,
                        "hsaTrustedBrowser": true,
                        "webservices": {
                            "findme": { "url": format!("http://{address}/") }
                        }
                    }),
                    _ => Value::Null,
                };
                respond(&mut stream, 200, &response);
            }
            requests
        });
        let root = temporary_root("proactive-reauth-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .password("valid-password")
            .session_root(&root)
            .build()
            .unwrap();
        let local = Url::parse(&format!("http://{address}/")).unwrap();
        client.endpoints.auth = local.join("appleauth/auth/").unwrap();
        client.endpoints.setup = local.join("setup/ws/1/").unwrap();
        client.session.session_token = Some("saved-token".into());
        client
            .cookies
            .lock()
            .unwrap()
            .parse(
                "X-APPLE-WEBAUTH-HSA-TRUST=value; Domain=apple.com; Path=/; Expires=Thu, 30 Oct 2026 12:00:00 GMT; Secure",
                &Url::parse("https://apple.com/").unwrap(),
            )
            .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let status = client.authenticate_at(now).await.unwrap();
        let requests = server.join().unwrap();

        assert!(matches!(status, AuthenticationStatus::Authenticated(_)));
        assert!(requests[0].contains("signin/init"));
        assert!(requests[1].contains("signin/complete"));
        assert!(requests[2].contains("accountLogin"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn accepts_terms_only_after_explicit_confirmation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                match index {
                    0 => respond(
                        &mut stream,
                        200,
                        &json!({ "iCloudTerms": { "version": 628_201 } }),
                    ),
                    1 => respond(&mut stream, 200, &Value::Null),
                    _ => respond(
                        &mut stream,
                        200,
                        &json!({
                            "dsInfo": {
                                "dsid": "12345",
                                "fullName": "Test Person",
                                "hsaVersion": 2,
                                "locked": false
                            },
                            "hsaChallengeRequired": false,
                            "hsaTrustedBrowser": true,
                            "webservices": {
                                "findme": { "url": format!("http://{address}/") }
                            }
                        }),
                    ),
                }
            }
            requests
        });
        let root = temporary_root("terms-test");
        let mut client = ClientBuilder::new("test@example.invalid")
            .session_root(&root)
            .build()
            .unwrap();
        client.endpoints.setup = Url::parse(&format!("http://{address}/setup/ws/1/")).unwrap();
        client.session.session_token = Some("saved-token".into());
        let pending = client
            .finish_account_login(
                &json!({
                    "termsUpdateNeeded": true,
                    "dsInfo": { "languageCode": "sv_SE" }
                }),
                false,
            )
            .await
            .unwrap();
        assert!(matches!(pending, AuthenticationStatus::TermsOfUseRequired));
        let rejected = client
            .accept_terms(TermsAcceptanceConfirmation::default())
            .await;
        assert!(matches!(rejected, Err(Error::TermsOfUseRequired)));

        let status = client
            .accept_terms(TermsAcceptanceConfirmation::confirm())
            .await
            .unwrap();
        let requests = server.join().unwrap();

        assert!(matches!(status, AuthenticationStatus::Authenticated(_)));
        assert!(requests[0].starts_with("POST /setup/ws/1/getTerms?"));
        assert!(requests[0].ends_with("\r\n\r\n{\"locale\":\"sv_SE\"}"));
        assert!(requests[1].starts_with("GET /setup/ws/1/repairDone?"));
        assert!(requests[1].contains("628201"));
        assert!(requests[2].starts_with("POST /setup/ws/1/accountLogin?"));
        fs::remove_dir_all(root).unwrap();
    }
}
