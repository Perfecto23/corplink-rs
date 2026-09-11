use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Default)]
pub enum State {
    #[default]
    Init,
    Login,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            State::Init => write!(f, "Init"),
            State::Login => write!(f, "Login"),
        }
    }
}

/// Runtime authentication material persisted independently from user intent.
///
/// The legacy fields remain on [`crate::config::Config`] so existing config
/// files continue to load. New writes use this sidecar instead of rewriting
/// the user's config file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionState {
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub legacy_cookie_migration: bool,
    pub state: State,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
    pub code: Option<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            identity: None,
            legacy_cookie_migration: false,
            state: State::Init,
            device_name: None,
            device_id: None,
            public_key: None,
            private_key: None,
            code: None,
        }
    }
}

/// The sidecar sits beside the configured file and follows the same interface
/// naming as the existing cookie store (for example `corplink_session.json`).
pub fn session_file_path(config_file: &str, interface_name: &str) -> PathBuf {
    let dir = Path::new(config_file)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    dir.join(format!("{interface_name}_session.json"))
}

pub fn load_session(path: &Path) -> io::Result<Option<SessionState>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    // A damaged sidecar is treated as an expired session. Keep the bytes in
    // place so a failed recovery can never destroy the last known state.
    Ok(serde_json::from_slice(&bytes).ok())
}

pub fn save_session(path: &Path, session: &SessionState) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(session)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    preserve_replaced_session(path, session)?;
    atomic_write_private(path, &bytes)
}

fn preserve_replaced_session(path: &Path, next: &SessionState) -> io::Result<()> {
    let previous = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let should_backup = match serde_json::from_slice::<SessionState>(&previous) {
        Ok(previous) => previous.identity != next.identity,
        Err(_) => true,
    };
    if !should_backup {
        return Ok(());
    }
    backup_existing_private(path, "replaced")?;
    Ok(())
}

pub(crate) fn backup_existing_private(path: &Path, label: &str) -> io::Result<Option<PathBuf>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let backup_path = backup_path(path, label);
    write_private_new(&backup_path, &bytes)?;
    Ok(Some(backup_path))
}

fn backup_path(path: &Path, label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("corplink-session");
    path.with_file_name(format!(".{name}.{label}.{nonce}"))
}

fn write_private_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// Atomically replace a private sidecar, preserving the previous file when
/// serialization, writing, syncing, or renaming fails.
pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("corplink-state");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));

    atomic_write_private_with_temp(path, bytes, &temp_path)
}

fn atomic_write_private_with_temp(path: &Path, bytes: &[u8], temp_path: &Path) -> io::Result<()> {
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        set_private_permissions(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn set_private_permissions(file: &std::fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("corplink-state-{name}-{}", std::process::id()))
    }

    #[test]
    fn session_round_trip_preserves_values_and_private_permissions() {
        let dir = test_dir("round-trip");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corplink_session.json");
        let session = SessionState {
            identity: Some("test-identity".to_string()),
            legacy_cookie_migration: false,
            state: State::Login,
            device_name: Some("test-device".to_string()),
            device_id: Some("device-id".to_string()),
            public_key: Some("public".to_string()),
            private_key: Some("private".to_string()),
            code: Some("otp-secret".to_string()),
        };

        save_session(&path, &session).unwrap();
        assert_eq!(load_session(&path).unwrap(), Some(session));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_session_is_reported_without_becoming_a_runtime_error() {
        let dir = test_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corplink_session.json");
        fs::write(&path, b"not-json").unwrap();

        assert_eq!(load_session(&path).unwrap(), None);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_atomic_replace_keeps_previous_state_file() {
        let dir = test_dir("atomic-failure");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corplink_session.json");
        let temp_path = dir.join("fixed-temp");
        let old = b"previous-session";
        fs::write(&path, old).unwrap();
        fs::write(&temp_path, b"occupied").unwrap();

        assert!(atomic_write_private_with_temp(&path, b"new-session", &temp_path).is_err());
        assert_eq!(fs::read(&path).unwrap(), old);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replaced_session_backup_is_private() {
        let dir = test_dir("backup-mode");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corplink_session.json");
        save_session(
            &path,
            &SessionState {
                identity: Some("first".to_string()),
                state: State::Login,
                ..SessionState::default()
            },
        )
        .unwrap();
        save_session(
            &path,
            &SessionState {
                identity: Some("second".to_string()),
                state: State::Init,
                ..SessionState::default()
            },
        )
        .unwrap();

        let backup = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("corplink_session.json.replaced.")
            })
            .expect("replacement backup should exist");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(backup.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
