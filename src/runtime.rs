//! Runtime state, bounded retry, and redacted diagnostic helpers.
//!
//! The state file is an operational diagnostic sidecar. It contains process
//! identity and health facts only; authentication material remains in the
//! session sidecar owned by `state.rs`.

use crate::api::FailureKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Authenticating,
    Connecting,
    Ready,
    Degraded,
    Stopping,
    Stopped,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Authenticating => "authenticating",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeStore {
    path: PathBuf,
    generation: String,
}

impl RuntimeStore {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("CORPLINK_RUNTIME_STATE")?;
        let generation = std::env::var("CORPLINK_RUNTIME_GENERATION").ok()?;
        Some(Self::new(PathBuf::from(path), generation))
    }

    pub fn new(path: impl Into<PathBuf>, generation: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            generation: generation.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn mark_initial(&self) -> Result<()> {
        self.update(Phase::Connecting, false, "connection starting", None)
    }

    pub fn mark_authenticating(&self) -> Result<()> {
        self.update(
            Phase::Authenticating,
            false,
            "authentication required",
            None,
        )
    }

    pub fn mark_ready(&self, handshake_age: Duration) -> Result<()> {
        self.update(
            Phase::Ready,
            true,
            "current WireGuard handshake confirmed",
            Some(handshake_age.as_secs()),
        )
    }

    pub fn mark_health(&self, handshake_age: Duration) -> Result<()> {
        self.update(
            Phase::Ready,
            true,
            "current WireGuard handshake observed",
            Some(handshake_age.as_secs()),
        )
    }

    pub fn mark_degraded(&self, reason: &str) -> Result<()> {
        self.update(Phase::Degraded, false, reason, None)
    }

    pub fn mark_failed(&self, reason: &str) -> Result<()> {
        self.update(Phase::Failed, false, reason, None)
    }

    pub fn mark_stopping(&self) -> Result<()> {
        self.update(Phase::Stopping, false, "stop requested", None)
    }

    pub fn mark_stopped(&self) -> Result<()> {
        self.update(Phase::Stopped, false, "stopped", None)
    }

    fn update(
        &self,
        phase: Phase,
        ready: bool,
        reason: &str,
        handshake_age_secs: Option<u64>,
    ) -> Result<()> {
        let result = self.update_inner(phase, ready, reason, handshake_age_secs);
        if let Err(error) = &result {
            log::error!(
                "runtime state update failed for generation {}: {}",
                self.generation,
                redact(&error.to_string())
            );
        }
        result
    }

    fn update_inner(
        &self,
        phase: Phase,
        ready: bool,
        reason: &str,
        handshake_age_secs: Option<u64>,
    ) -> Result<()> {
        let _lock = StateLock::acquire(&self.path)?;
        let mut value = match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .with_context(|| format!("invalid runtime state {}", self.path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                serde_json::json!({})
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read runtime state {}", self.path.display())
                })
            }
        };

        if let Some(existing_generation) = value.get("generation").and_then(|v| v.as_str()) {
            if existing_generation != self.generation {
                // A process from an older generation must never revive a new
                // run's state after stop/restart.
                return Ok(());
            }
        }

        let existing_intent = value.get("intent").and_then(|v| v.as_str());
        let existing_phase = value.get("phase").and_then(|v| v.as_str());
        if ((existing_intent == Some("stopped") && existing_phase == Some("stopped"))
            || (existing_intent == Some("stopping") && existing_phase == Some("stopping"))
            || (existing_intent == Some("failed") && existing_phase == Some("failed")))
            && phase != Phase::Stopped
            && phase != Phase::Stopping
            && phase != Phase::Failed
        {
            // A delayed health callback from a process that has already been
            // fenced by stop must not revive this generation.
            return Ok(());
        }

        let object = value
            .as_object_mut()
            .context("runtime state must be a JSON object")?;
        object.insert("schema_version".to_string(), serde_json::Value::from(1_u32));
        object.insert(
            "generation".to_string(),
            serde_json::Value::from(self.generation.clone()),
        );
        let intent = match phase {
            Phase::Stopping => "stopping",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
            _ => "running",
        };
        object.insert("intent".to_string(), serde_json::Value::from(intent));
        object.insert("phase".to_string(), serde_json::Value::from(phase.as_str()));
        object.insert("ready".to_string(), serde_json::Value::from(ready));
        object.insert(
            "pid".to_string(),
            serde_json::Value::from(std::process::id()),
        );
        object.insert(
            "process_start".to_string(),
            process_start_token(std::process::id())
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null),
        );
        object.insert(
            "reason".to_string(),
            serde_json::Value::from(redact(reason)),
        );
        object.insert(
            "handshake_age_secs".to_string(),
            handshake_age_secs
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null),
        );
        object.insert(
            "updated_at".to_string(),
            serde_json::Value::from(now_timestamp()),
        );
        write_runtime_state(&self.path, &value)
    }
}

fn now_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    seconds.to_string()
}

fn process_start_token(pid: u32) -> Option<String> {
    #[cfg(unix)]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()
            .ok()?;
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return (!token.is_empty()).then_some(token);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

pub fn bounded_backoff(attempt: u32) -> Duration {
    let seconds = 1_u64 << attempt.min(5);
    Duration::from_secs(seconds.min(30))
}

pub enum OperationOutcome<T> {
    Completed(T),
    Cancelled,
    TimedOut,
}

pub async fn run_bounded<F, T>(
    operation: F,
    deadline: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> OperationOutcome<T>
where
    F: Future<Output = T>,
{
    let mut operation = Box::pin(operation);
    loop {
        tokio::select! {
            result = &mut operation => return OperationOutcome::Completed(result),
            _ = tokio::time::sleep(deadline) => return OperationOutcome::TimedOut,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return OperationOutcome::Cancelled;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryDecision {
    Retry { attempt: u32, delay: Duration },
    Reauthenticate,
    Fail,
}

#[derive(Clone, Debug)]
pub struct RecoveryPolicy {
    transient_attempts: u32,
    auth_attempts: u32,
}

impl RecoveryPolicy {
    pub fn new() -> Self {
        Self {
            transient_attempts: 0,
            auth_attempts: 0,
        }
    }

    pub fn on_failure(&mut self, kind: FailureKind) -> RetryDecision {
        if kind.requires_login() && self.auth_attempts == 0 {
            self.auth_attempts = 1;
            return RetryDecision::Reauthenticate;
        }
        if kind.is_retryable() {
            return self.on_transient_failure();
        }
        RetryDecision::Fail
    }

    pub fn on_transient_failure(&mut self) -> RetryDecision {
        self.transient_attempts = self.transient_attempts.saturating_add(1);
        RetryDecision::Retry {
            attempt: self.transient_attempts,
            delay: bounded_backoff(self.transient_attempts),
        }
    }

    pub fn reset_after_ready(&mut self) {
        self.transient_attempts = 0;
        self.auth_attempts = 0;
    }
}

pub fn redact(input: &str) -> String {
    let mut output = input.to_string();
    for key in ["password", "token", "secret", "cookie", "private_key"] {
        redact_key_values(&mut output, key);
    }
    output
}

fn redact_key_values(output: &mut String, key: &str) {
    let mut search_from = 0;
    loop {
        let lower = output.to_ascii_lowercase();
        let Some(relative_start) = lower[search_from..].find(key) else {
            return;
        };
        let key_start = search_from + relative_start;
        if key_start > 0 {
            let previous = lower.as_bytes()[key_start - 1];
            if previous.is_ascii_alphanumeric() || previous == b'_' {
                search_from = key_start + key.len();
                continue;
            }
        }

        let after_key = key_start + key.len();
        let mut delimiter = None;
        for (offset, character) in lower[after_key..].char_indices() {
            if matches!(character, ':' | '=') {
                delimiter = Some(after_key + offset);
                break;
            }
            if character.is_whitespace() || character == ',' {
                break;
            }
        }
        let Some(delimiter) = delimiter else {
            search_from = after_key;
            continue;
        };

        let mut value_start = delimiter + 1;
        while value_start < output.len() && output.as_bytes()[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        if value_start >= output.len() {
            return;
        }

        let first = output.as_bytes()[value_start];
        let (value_end, replacement) = if first == b'"' || first == b'\'' {
            let mut end = value_start + 1;
            while end < output.len() {
                if output.as_bytes()[end] == first && output.as_bytes()[end - 1] != b'\\' {
                    break;
                }
                end += 1;
            }
            if end >= output.len() {
                return;
            }
            (
                end + 1,
                format!("{}<redacted>{}", first as char, first as char),
            )
        } else {
            let end = output[value_start..]
                .char_indices()
                .find(|(_, character)| {
                    character.is_whitespace() || matches!(character, ',' | ';' | '}' | ']')
                })
                .map(|(offset, _)| value_start + offset)
                .unwrap_or(output.len());
            (end, "<redacted>".to_string())
        };

        output.replace_range(value_start..value_end, &replacement);
        search_from = value_start + replacement.len();
    }
}

fn write_runtime_state(path: &Path, value: &serde_json::Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize runtime state")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime state directory {}",
            parent.display()
        )
    })?;
    let nonce = format!("{}.{}", std::process::id(), now_timestamp());
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("runtime")
    ));
    let temp = temp.with_extension(nonce);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(&temp)
        .with_context(|| format!("failed to create {}", temp.display()))?;
    file.write_all(&bytes)
        .context("failed to write runtime state")?;
    file.write_all(b"\n")
        .context("failed to finish runtime state")?;
    file.sync_all().context("failed to sync runtime state")?;
    drop(file);
    fs::rename(&temp, path).with_context(|| format!("failed to publish {}", path.display()))?;
    fs::set_permissions(path, permissions_for_runtime_state()).with_context(|| {
        format!(
            "failed to set readable permissions on runtime state {}",
            path.display()
        )
    })?;
    Ok(())
}

fn permissions_for_runtime_state() -> fs::Permissions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return fs::Permissions::from_mode(0o644);
    }
    #[cfg(not(unix))]
    {
        fs::metadata(".")
            .expect("current directory metadata")
            .permissions()
    }
}

struct StateLock {
    path: PathBuf,
}

impl StateLock {
    fn acquire(state_path: &Path) -> Result<Self> {
        let path = PathBuf::from(format!("{}.lock", state_path.display()));
        for _ in 0..200 {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let _ = fs::write(path.join("pid"), std::process::id().to_string());
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Ok(owner) = fs::read_to_string(path.join("pid")) {
                        if let Ok(pid) = owner.trim().parse::<u32>() {
                            if !process_exists(pid) {
                                let _ = fs::remove_dir_all(&path);
                                continue;
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("failed to acquire runtime state lock"),
            }
        }
        Err(anyhow::anyhow!("timed out acquiring runtime state lock"))
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("pid"));
        let _ = fs::remove_dir(&self.path);
    }
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("corplink-runtime-{name}-{}", std::process::id()))
    }

    #[test]
    fn runtime_state_is_ready_only_after_current_handshake() {
        let state_path = path("ready.json");
        let _ = fs::remove_file(&state_path);
        let store = RuntimeStore::new(&state_path, "generation-a");
        fs::write(
            &state_path,
            r#"{"schema_version":1,"generation":"generation-a","intent":"running","phase":"connecting","ready":false,"pid":1,"process_start":null,"reason":"connecting","handshake_age_secs":null,"restart_count":0,"last_exit":null,"updated_at":"0"}"#,
        )
        .unwrap();

        store.mark_ready(Duration::from_secs(4)).unwrap();
        let snapshot: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(snapshot["phase"], "ready");
        assert!(snapshot["ready"].as_bool().unwrap());
        assert_eq!(snapshot["handshake_age_secs"], 4);
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn stale_generation_cannot_revive_runtime_state() {
        let state_path = path("stale.json");
        let _ = fs::remove_file(&state_path);
        fs::write(
            &state_path,
            r#"{"schema_version":1,"generation":"generation-b","intent":"stopped","phase":"stopped","ready":false,"pid":1,"process_start":null,"reason":"stopped","handshake_age_secs":null,"restart_count":0,"last_exit":null,"updated_at":"0"}"#,
        )
        .unwrap();
        RuntimeStore::new(&state_path, "generation-a")
            .mark_ready(Duration::from_secs(1))
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(value["phase"], "stopped");
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn diagnostics_redact_authentication_material() {
        let line = redact("got token=ABC123 password=secret-value");
        assert!(!line.contains("ABC123"));
        assert!(!line.contains("secret-value"));
        assert!(line.contains("<redacted>"));
    }

    #[tokio::test]
    async fn bounded_operation_reports_timeout_without_running_forever() {
        let (_sender, mut shutdown) = tokio::sync::watch::channel(false);
        let result = run_bounded(
            std::future::pending::<()>(),
            Duration::from_millis(1),
            &mut shutdown,
        )
        .await;
        assert!(matches!(result, OperationOutcome::TimedOut));
    }

    #[tokio::test]
    async fn bounded_operation_reports_shutdown_cancellation() {
        let (sender, mut shutdown) = tokio::sync::watch::channel(false);
        sender.send(true).unwrap();
        let result = run_bounded(
            std::future::pending::<()>(),
            Duration::from_secs(30),
            &mut shutdown,
        )
        .await;
        assert!(matches!(result, OperationOutcome::Cancelled));
    }

    #[test]
    fn diagnostics_redact_all_repeated_and_quoted_secret_values() {
        let line = redact(
            r#"operation=login password="first secret" token: 'second-token' password=third-secret"#,
        );
        assert!(!line.contains("first secret"));
        assert!(!line.contains("second-token"));
        assert!(!line.contains("third-secret"));
        assert!(line.contains("operation=login"));
        assert_eq!(line.matches("<redacted>").count(), 3);
    }

    #[test]
    fn transient_network_failures_keep_retrying_with_capped_backoff() {
        let mut policy = RecoveryPolicy::new();
        for attempt in 1..=100 {
            match policy.on_transient_failure() {
                RetryDecision::Retry {
                    attempt: actual,
                    delay,
                } => {
                    assert_eq!(actual, attempt);
                    assert!(delay <= Duration::from_secs(30));
                }
                other => panic!("temporary network failure stopped recovery: {other:?}"),
            }
        }
    }

    #[test]
    fn recovery_policy_does_not_retry_interaction_or_configuration_failures() {
        let mut policy = RecoveryPolicy::new();
        assert_eq!(
            policy.on_failure(FailureKind::InteractionRequired),
            RetryDecision::Fail
        );
        assert_eq!(
            policy.on_failure(FailureKind::Configuration),
            RetryDecision::Fail
        );
    }

    #[test]
    fn recovery_policy_allows_one_reauthentication_then_stops_looping() {
        let mut policy = RecoveryPolicy::new();
        assert_eq!(
            policy.on_failure(FailureKind::AuthenticationExpired),
            RetryDecision::Reauthenticate
        );
        assert_eq!(
            policy.on_failure(FailureKind::AuthenticationExpired),
            RetryDecision::Fail
        );
    }

    #[test]
    fn terminal_stop_state_cannot_be_revived_by_delayed_health_update() {
        let state_path = path("terminal-stop.json");
        let _ = fs::remove_file(&state_path);
        let store = RuntimeStore::new(&state_path, "generation-terminal");
        fs::write(
            &state_path,
            r#"{"schema_version":1,"generation":"generation-terminal","intent":"running","phase":"connecting","ready":false,"pid":1,"process_start":null,"reason":"connecting","handshake_age_secs":null,"restart_count":0,"last_exit":null,"updated_at":"0"}"#,
        )
        .unwrap();

        store.mark_stopping().unwrap();
        store.mark_stopped().unwrap();
        store.mark_ready(Duration::from_secs(1)).unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(value["intent"], "stopped");
        assert_eq!(value["phase"], "stopped");
        assert!(!value["ready"].as_bool().unwrap());
        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn stop_cleanup_failure_is_terminal_and_health_cannot_revive_it() {
        let state_path = path("cleanup-failure.json");
        let _ = fs::remove_file(&state_path);
        let store = RuntimeStore::new(&state_path, "generation-cleanup-failure");
        fs::write(
            &state_path,
            r#"{"schema_version":1,"generation":"generation-cleanup-failure","intent":"running","phase":"ready","ready":true,"pid":1,"process_start":null,"reason":"ready","handshake_age_secs":1,"restart_count":0,"last_exit":null,"updated_at":"0"}"#,
        )
        .unwrap();

        store.mark_stopping().unwrap();
        store.mark_failed("local cleanup incomplete").unwrap();
        store.mark_health(Duration::from_secs(1)).unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(value["intent"], "failed");
        assert_eq!(value["phase"], "failed");
        assert!(!value["ready"].as_bool().unwrap());
        let _ = fs::remove_file(state_path);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_diagnostics_are_readable_by_the_starting_user() {
        use std::os::unix::fs::PermissionsExt;

        let state_path = path("permissions.json");
        let _ = fs::remove_file(&state_path);
        RuntimeStore::new(&state_path, "generation-permissions")
            .mark_ready(Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let _ = fs::remove_file(state_path);
    }
}
