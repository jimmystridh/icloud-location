//! Private persistent Apple session storage.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use reqwest_cookie_store::{CookieStore, CookieStoreMutex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::TwoFactorChallenge;
use crate::error::{Error, Result};

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

pub(crate) struct SessionStore {
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

        Ok(Self {
            state_path: account_directory.join("session.json"),
            cookies_path: account_directory.join("cookies.json"),
            account_directory,
        })
    }

    pub fn load_state(&self) -> Result<SessionData> {
        if !self.state_path.exists() {
            return Ok(SessionData::default());
        }

        let reader = BufReader::new(File::open(&self.state_path)?);
        serde_json::from_reader(reader).map_err(Error::from)
    }

    pub fn load_cookies(&self) -> Result<Arc<CookieStoreMutex>> {
        let cookies = if self.cookies_path.exists() {
            let reader = BufReader::new(File::open(&self.cookies_path)?);
            cookie_store::serde::json::load(reader)
                .map_err(|error| Error::Session(format!("failed to load cookies: {error}")))?
        } else {
            CookieStore::new()
        };

        Ok(Arc::new(CookieStoreMutex::new(cookies)))
    }

    pub fn save(&self, state: &SessionData, cookies: &CookieStoreMutex) -> Result<()> {
        create_private_directory(&self.account_directory)?;

        let state_temporary = temporary_path(&self.state_path);
        {
            let mut writer = BufWriter::new(create_private_file(&state_temporary)?);
            serde_json::to_writer_pretty(&mut writer, state)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        fs::rename(state_temporary, &self.state_path)?;

        let cookies_temporary = temporary_path(&self.cookies_path);
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
        fs::rename(cookies_temporary, &self.cookies_path)?;

        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        if self.account_directory.exists() {
            fs::remove_dir_all(&self.account_directory)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn account_directory(&self) -> &Path {
        &self.account_directory
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
        fs::write(temporary_path(&store.state_path), b"{partial").unwrap();

        assert_eq!(
            store.load_state().unwrap().session_token.as_deref(),
            Some("session-token")
        );
        for path in [&store.state_path, &store.cookies_path] {
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
                fs::metadata(&store.state_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&store.cookies_path)
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
