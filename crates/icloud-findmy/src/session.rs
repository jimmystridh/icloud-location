//! Private disk and portable in-memory Apple session storage.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use reqwest_cookie_store::{CookieStore, CookieStoreMutex};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::client::TwoFactorChallenge;
use crate::error::{Error, Result};

const PORTABLE_SESSION_FORMAT_VERSION: u32 = 1;
const MAX_PORTABLE_SESSION_BYTES: usize = 256 * 1024;
const MAX_PORTABLE_COOKIE_COUNT: usize = 256;
const MAX_PORTABLE_RAW_COOKIE_BYTES: usize = 8 * 1024;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct SessionData {
    pub client_id: String,
    pub account_country: Option<String>,
    pub session_id: Option<String>,
    pub session_token: Option<String>,
    pub trust_token: Option<String>,
    pub scnt: Option<String>,
    pub dsid: Option<String>,
    pub findme_url: Option<String>,
    pub account_name: Option<String>,
    pub challenge_metadata: Option<TwoFactorChallenge>,
    pub pending_terms_locale: Option<String>,
    pub last_trusted_at: Option<DateTime<Utc>>,
    pub last_authentication_method: Option<String>,
}

impl SessionData {
    pub fn has_session_token(&self) -> bool {
        self.session_token
            .as_ref()
            .is_some_and(|token| !token.is_empty())
    }

    pub fn clear_authentication(&mut self) {
        self.account_country = None;
        self.session_id = None;
        self.session_token = None;
        self.trust_token = None;
        self.scnt = None;
        self.dsid = None;
        self.findme_url = None;
        self.account_name = None;
        self.pending_terms_locale = None;
    }
}

/// A validated, account-bound archive for carrying an Apple session between
/// stateless client instances.
///
/// The archive contains authentication tokens and cookies. Its account binding
/// prevents accidental cross-account reuse but does not authenticate its
/// contents. Callers must protect it with authenticated encryption before
/// durable storage and must not log its bytes. It never contains the Apple
/// account password.
pub struct PortableSession {
    archive: Vec<u8>,
}

impl PortableSession {
    /// The current portable archive format version.
    pub const FORMAT_VERSION: u32 = PORTABLE_SESSION_FORMAT_VERSION;

    /// The largest portable archive accepted or produced by this crate.
    pub const MAX_BYTES: usize = MAX_PORTABLE_SESSION_BYTES;

    /// Validates and takes ownership of a portable archive received from
    /// external storage.
    ///
    /// Account binding is checked later by [`crate::ClientBuilder::build`], when
    /// the expected Apple account username is available.
    ///
    /// # Errors
    ///
    /// Returns an error when the archive is empty, oversized, malformed, has an
    /// unsupported version, or contains an invalid cookie store.
    pub fn from_bytes(mut bytes: Vec<u8>) -> Result<Self> {
        let result = (|| {
            let mut envelope = decode_portable_envelope(&bytes)?;
            let cookies = cookie_store_from_value(&envelope.cookies)?;
            envelope.cookies = cookie_store_to_value(&CookieStoreMutex::new(cookies))?;
            let mut archive = serde_json::to_vec(&envelope)?;
            if let Err(error) = ensure_portable_size(&archive) {
                archive.zeroize();
                return Err(error);
            }
            Ok(Self { archive })
        })();
        bytes.zeroize();
        result
    }

    /// Validates and copies a portable archive borrowed from external storage.
    ///
    /// Prefer [`Self::from_bytes`] when ownership is available so the original
    /// sensitive buffer is covered by this type's zeroizing drop behavior.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::from_bytes`].
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes(bytes.to_vec())
    }

    /// Returns the validated archive bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.archive
    }

    /// Returns the archive size in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.archive.len()
    }

    /// Returns whether the archive is empty.
    ///
    /// A successfully constructed archive is never empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.archive.is_empty()
    }

    pub(crate) fn capture(
        username: &str,
        state: &SessionData,
        cookies: &CookieStoreMutex,
    ) -> Result<Self> {
        let envelope = PortableSessionEnvelopeV1 {
            version: PORTABLE_SESSION_FORMAT_VERSION,
            account_binding: portable_account_binding(username),
            session: PortableSessionDataV1::from(state),
            cookies: cookie_store_to_value(cookies)?,
        };
        let mut archive = serde_json::to_vec(&envelope)?;
        if let Err(error) = ensure_portable_size(&archive) {
            archive.zeroize();
            return Err(error);
        }
        Ok(Self { archive })
    }

    pub(crate) fn restore_for(
        &self,
        username: &str,
    ) -> Result<(SessionData, Arc<CookieStoreMutex>)> {
        let envelope = decode_portable_envelope(&self.archive)?;
        if envelope.account_binding != portable_account_binding(username) {
            return Err(Error::PortableSessionAccountMismatch);
        }
        let cookies = cookie_store_from_value(&envelope.cookies)?;
        Ok((
            envelope.session.into(),
            Arc::new(CookieStoreMutex::new(cookies)),
        ))
    }
}

impl Clone for PortableSession {
    fn clone(&self) -> Self {
        Self {
            archive: self.archive.clone(),
        }
    }
}

impl fmt::Debug for PortableSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortableSession")
            .field("format_version", &PORTABLE_SESSION_FORMAT_VERSION)
            .field("byte_len", &self.archive.len())
            .field("contents", &"[REDACTED]")
            .finish()
    }
}

impl Drop for PortableSession {
    fn drop(&mut self) {
        self.archive.zeroize();
    }
}

impl TryFrom<&[u8]> for PortableSession {
    type Error = Error;

    fn try_from(bytes: &[u8]) -> Result<Self> {
        Self::from_slice(bytes)
    }
}

impl TryFrom<Vec<u8>> for PortableSession {
    type Error = Error;

    fn try_from(bytes: Vec<u8>) -> Result<Self> {
        Self::from_bytes(bytes)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PortableSessionEnvelopeV1 {
    version: u32,
    account_binding: String,
    session: PortableSessionDataV1,
    cookies: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PortableSessionDataV1 {
    client_id: String,
    account_country: Option<String>,
    session_id: Option<String>,
    session_token: Option<String>,
    trust_token: Option<String>,
    scnt: Option<String>,
    dsid: Option<String>,
    findme_url: Option<String>,
    account_name: Option<String>,
    challenge_metadata: Option<TwoFactorChallenge>,
    pending_terms_locale: Option<String>,
    last_trusted_at: Option<DateTime<Utc>>,
    last_authentication_method: Option<String>,
}

impl From<&SessionData> for PortableSessionDataV1 {
    fn from(state: &SessionData) -> Self {
        Self {
            client_id: state.client_id.clone(),
            account_country: state.account_country.clone(),
            session_id: state.session_id.clone(),
            session_token: state.session_token.clone(),
            trust_token: state.trust_token.clone(),
            scnt: state.scnt.clone(),
            dsid: state.dsid.clone(),
            findme_url: state.findme_url.clone(),
            account_name: state.account_name.clone(),
            challenge_metadata: state.challenge_metadata.clone(),
            pending_terms_locale: state.pending_terms_locale.clone(),
            last_trusted_at: state.last_trusted_at,
            last_authentication_method: state.last_authentication_method.clone(),
        }
    }
}

impl From<PortableSessionDataV1> for SessionData {
    fn from(state: PortableSessionDataV1) -> Self {
        Self {
            client_id: state.client_id,
            account_country: state.account_country,
            session_id: state.session_id,
            session_token: state.session_token,
            trust_token: state.trust_token,
            scnt: state.scnt,
            dsid: state.dsid,
            findme_url: state.findme_url,
            account_name: state.account_name,
            challenge_metadata: state.challenge_metadata,
            pending_terms_locale: state.pending_terms_locale,
            last_trusted_at: state.last_trusted_at,
            last_authentication_method: state.last_authentication_method,
        }
    }
}

fn decode_portable_envelope(bytes: &[u8]) -> Result<PortableSessionEnvelopeV1> {
    ensure_portable_size(bytes)?;
    let envelope: PortableSessionEnvelopeV1 = serde_json::from_slice(bytes)
        .map_err(|error| Error::InvalidPortableSession(error.to_string()))?;
    if envelope.version != PORTABLE_SESSION_FORMAT_VERSION {
        return Err(Error::UnsupportedPortableSessionVersion(envelope.version));
    }
    if envelope.account_binding.len() != 64
        || !envelope
            .account_binding
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::InvalidPortableSession(
            "account binding is malformed".into(),
        ));
    }
    Ok(envelope)
}

fn ensure_portable_size(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Err(Error::InvalidPortableSession("archive is empty".into()));
    }
    if bytes.len() > MAX_PORTABLE_SESSION_BYTES {
        return Err(Error::PortableSessionTooLarge {
            max_bytes: MAX_PORTABLE_SESSION_BYTES,
        });
    }
    Ok(())
}

fn cookie_store_to_value(cookies: &CookieStoreMutex) -> Result<Value> {
    let cookies = cookies
        .lock()
        .map_err(|_| Error::Session("cookie store lock is poisoned".into()))?;
    let mut encoded = Vec::new();
    cookie_store::serde::json::save_incl_expired_and_nonpersistent(&cookies, &mut encoded)
        .map_err(|error| Error::Session(format!("failed to export cookies: {error}")))?;
    let value = ensure_portable_size(&encoded)
        .and_then(|()| {
            serde_json::from_slice(&encoded)
                .map_err(|error| Error::InvalidPortableSession(error.to_string()))
        })
        .and_then(|value| {
            validate_portable_cookies(&value)?;
            Ok(value)
        });
    encoded.zeroize();
    value
}

fn cookie_store_from_value(value: &Value) -> Result<CookieStore> {
    validate_portable_cookies(value)?;
    let mut encoded = serde_json::to_vec(value)?;
    let cookies = ensure_portable_size(&encoded).and_then(|()| {
        cookie_store::serde::json::load_all(BufReader::new(Cursor::new(&encoded)))
            .map_err(|_| Error::InvalidPortableSession("cookie store is malformed".into()))
    });
    encoded.zeroize();
    cookies
}

fn validate_portable_cookies(value: &Value) -> Result<()> {
    let cookies = value
        .as_array()
        .ok_or_else(|| Error::InvalidPortableSession("cookie store is malformed".into()))?;
    if cookies.len() > MAX_PORTABLE_COOKIE_COUNT {
        return Err(Error::InvalidPortableSession(
            "cookie store has too many entries".into(),
        ));
    }
    if cookies.iter().any(|cookie| {
        cookie
            .get("raw_cookie")
            .and_then(Value::as_str)
            .is_none_or(|raw| raw.len() > MAX_PORTABLE_RAW_COOKIE_BYTES)
    }) {
        return Err(Error::InvalidPortableSession(
            "cookie entry is malformed or oversized".into(),
        ));
    }
    Ok(())
}

fn portable_account_binding(username: &str) -> String {
    let digest = Sha256::digest(username.trim().to_lowercase().as_bytes());
    let mut binding = String::with_capacity(64);
    for byte in digest {
        write!(binding, "{byte:02x}").expect("writing to a String cannot fail");
    }
    binding
}

pub(crate) enum SessionStore {
    Disk(DiskSessionStore),
    Memory,
}

pub(crate) struct DiskSessionStore {
    account_directory: PathBuf,
    state_path: PathBuf,
    cookies_path: PathBuf,
}

impl SessionStore {
    pub fn new(root: Option<PathBuf>, username: &str) -> Result<Self> {
        let root = match root {
            Some(root) => root,
            None => ProjectDirs::from("io", "icloud-location", "icloud-location")
                .ok_or_else(|| Error::Session("platform has no user data directory".into()))?
                .data_local_dir()
                .to_path_buf(),
        };
        let account_directory = root.join("accounts").join(account_key(username));

        Ok(Self::Disk(DiskSessionStore {
            state_path: account_directory.join("session.json"),
            cookies_path: account_directory.join("cookies.json"),
            account_directory,
        }))
    }

    #[must_use]
    pub const fn memory() -> Self {
        Self::Memory
    }

    pub fn load_state(&self) -> Result<SessionData> {
        let Self::Disk(store) = self else {
            return Ok(SessionData::default());
        };
        if !store.state_path.exists() {
            return Ok(SessionData::default());
        }

        let reader = BufReader::new(File::open(&store.state_path)?);
        serde_json::from_reader(reader).map_err(Error::from)
    }

    pub fn load_cookies(&self) -> Result<Arc<CookieStoreMutex>> {
        let Self::Disk(store) = self else {
            return Ok(Arc::new(CookieStoreMutex::new(CookieStore::new())));
        };
        let cookies = if store.cookies_path.exists() {
            let reader = BufReader::new(File::open(&store.cookies_path)?);
            cookie_store::serde::json::load(reader)
                .map_err(|error| Error::Session(format!("failed to load cookies: {error}")))?
        } else {
            CookieStore::new()
        };

        Ok(Arc::new(CookieStoreMutex::new(cookies)))
    }

    pub fn save(&self, state: &SessionData, cookies: &CookieStoreMutex) -> Result<()> {
        let Self::Disk(store) = self else {
            return Ok(());
        };
        create_private_directory(&store.account_directory)?;

        let state_temporary = temporary_path(&store.state_path);
        {
            let mut writer = BufWriter::new(create_private_file(&state_temporary)?);
            serde_json::to_writer_pretty(&mut writer, state)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        fs::rename(state_temporary, &store.state_path)?;

        let cookies_temporary = temporary_path(&store.cookies_path);
        {
            let mut writer = BufWriter::new(create_private_file(&cookies_temporary)?);
            let cookies = cookies
                .lock()
                .map_err(|_| Error::Session("cookie store lock is poisoned".into()))?;
            cookie_store::serde::json::save_incl_expired_and_nonpersistent(&cookies, &mut writer)
                .map_err(|error| Error::Session(format!("failed to save cookies: {error}")))?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        fs::rename(cookies_temporary, &store.cookies_path)?;

        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        let Self::Disk(store) = self else {
            return Ok(());
        };
        if store.account_directory.exists() {
            fs::remove_dir_all(&store.account_directory)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn account_directory(&self) -> &Path {
        match self {
            Self::Disk(store) => &store.account_directory,
            Self::Memory => panic!("an in-memory session has no account directory"),
        }
    }

    #[cfg(test)]
    pub fn state_path(&self) -> &Path {
        match self {
            Self::Disk(store) => &store.state_path,
            Self::Memory => panic!("an in-memory session has no state path"),
        }
    }

    #[cfg(test)]
    pub fn cookies_path(&self) -> &Path {
        match self {
            Self::Disk(store) => &store.cookies_path,
            Self::Memory => panic!("an in-memory session has no cookies path"),
        }
    }
}

fn account_key(username: &str) -> String {
    let digest = Sha256::digest(username.trim().to_lowercase().as_bytes());
    let mut key = String::with_capacity(24);
    for byte in &digest[..12] {
        write!(key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    key
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map_or_else(|| "session".into(), std::ffi::OsStr::to_os_string);
    file_name.push(".tmp");
    path.with_file_name(file_name)
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    set_directory_permissions(path)?;
    Ok(())
}

fn create_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options.open(path)?;
    set_file_permissions(path)?;
    Ok(file)
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn account_storage_does_not_expose_the_username() {
        let root = std::env::temp_dir().join(format!(
            "icloud-location-session-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = SessionStore::new(Some(root.clone()), "alice@example.com").unwrap();

        assert!(
            !store
                .account_directory()
                .to_string_lossy()
                .contains("alice")
        );
        assert!(!store.account_directory().to_string_lossy().contains('@'));
        assert!(store.account_directory().starts_with(root));
    }

    #[test]
    fn session_storage_is_atomic_private_and_never_contains_a_password() {
        let root = std::env::temp_dir().join(format!(
            "icloud-location-session-persistence-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = SessionStore::new(Some(root.clone()), "alice@example.com").unwrap();
        let state = SessionData {
            client_id: "client-id".into(),
            session_token: Some("session-token".into()),
            trust_token: Some("trust-token".into()),
            ..SessionData::default()
        };
        let cookies = CookieStoreMutex::new(CookieStore::new());

        store.save(&state, &cookies).unwrap();
        fs::write(temporary_path(store.state_path()), b"{partial").unwrap();

        assert_eq!(
            store.load_state().unwrap().session_token.as_deref(),
            Some("session-token")
        );
        for path in [store.state_path(), store.cookies_path()] {
            let contents = fs::read_to_string(path).unwrap();
            assert!(!contents.contains("correct horse battery staple"));
            assert!(!contents.contains("alice@example.com"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.account_directory())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(store.state_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(store.cookies_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
