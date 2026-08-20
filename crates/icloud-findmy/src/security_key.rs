use icloud_location_core::BoxFuture;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecurityKeyRequest {
    pub challenge: String,
    pub credential_ids: Vec<String>,
    pub relying_party_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecurityKeyAssertion {
    pub client_data: Vec<u8>,
    pub signature: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub user_handle: Option<Vec<u8>>,
    pub credential_id: Vec<u8>,
}

pub trait SecurityKeyAuthenticator: Send {
    fn get_assertion<'a>(
        &'a mut self,
        request: &'a SecurityKeyRequest,
    ) -> BoxFuture<'a, Result<SecurityKeyAssertion, SecurityKeyError>>;
}

#[derive(Debug, Error)]
#[error("security-key authentication failed: {message}")]
pub struct SecurityKeyError {
    pub message: String,
}

#[cfg(feature = "security-key")]
#[derive(Debug, Default)]
pub struct UsbSecurityKeyAuthenticator;

#[cfg(feature = "security-key")]
impl UsbSecurityKeyAuthenticator {
    /// Reports whether a FIDO2 USB HID authenticator is currently connected.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system's USB transport is unavailable.
    pub async fn has_connected_key(&self) -> Result<bool, SecurityKeyError> {
        use webauthn_authenticator_rs::transport::{AnyTransport, Transport};

        let transport = AnyTransport::new().await.map_err(security_key_error)?;
        transport
            .tokens()
            .await
            .map(|tokens| !tokens.is_empty())
            .map_err(security_key_error)
    }
}

#[cfg(feature = "security-key")]
impl SecurityKeyAuthenticator for UsbSecurityKeyAuthenticator {
    fn get_assertion<'a>(
        &'a mut self,
        request: &'a SecurityKeyRequest,
    ) -> BoxFuture<'a, Result<SecurityKeyAssertion, SecurityKeyError>> {
        Box::pin(async move {
            use webauthn_authenticator_rs::ctap2::CtapAuthenticator;
            use webauthn_authenticator_rs::prelude::{
                RequestChallengeResponse, Url, WebauthnAuthenticator,
            };
            use webauthn_authenticator_rs::transport::{AnyTransport, Transport};
            use webauthn_authenticator_rs::ui::Cli;

            let options: RequestChallengeResponse = serde_json::from_value(serde_json::json!({
                "publicKey": {
                    "challenge": request.challenge,
                    "rpId": request.relying_party_id,
                    "allowCredentials": request.credential_ids.iter().map(|credential_id| {
                        serde_json::json!({
                            "type": "public-key",
                            "id": credential_id,
                            "transports": ["usb"]
                        })
                    }).collect::<Vec<_>>(),
                    "userVerification": "discouraged"
                }
            }))
            .map_err(security_key_error)?;
            let transport = AnyTransport::new().await.map_err(security_key_error)?;
            let mut tokens = transport.tokens().await.map_err(security_key_error)?;
            if tokens.is_empty() {
                return Err(SecurityKeyError {
                    message: "no FIDO2 USB HID authenticator is connected".into(),
                });
            }
            let ui = Cli {};
            let authenticator = CtapAuthenticator::new(tokens.remove(0), &ui)
                .await
                .ok_or_else(|| SecurityKeyError {
                    message: "the connected USB key does not support CTAP2".into(),
                })?;
            let credential = WebauthnAuthenticator::new(authenticator)
                .do_authentication(
                    Url::parse("https://apple.com").map_err(security_key_error)?,
                    options,
                )
                .map_err(security_key_error)?;
            Ok(SecurityKeyAssertion {
                client_data: credential.response.client_data_json.as_slice().to_vec(),
                signature: credential.response.signature.as_slice().to_vec(),
                authenticator_data: credential.response.authenticator_data.as_slice().to_vec(),
                user_handle: credential
                    .response
                    .user_handle
                    .map(|value| value.as_slice().to_vec()),
                credential_id: credential.raw_id.as_slice().to_vec(),
            })
        })
    }
}

#[cfg(feature = "security-key")]
fn security_key_error(error: impl std::fmt::Display) -> SecurityKeyError {
    SecurityKeyError {
        message: error.to_string(),
    }
}

#[cfg(all(test, feature = "security-key"))]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a FIDO2 USB HID authenticator"]
    async fn detects_an_opt_in_usb_hid_authenticator() {
        assert!(
            UsbSecurityKeyAuthenticator
                .has_connected_key()
                .await
                .unwrap()
        );
    }
}
