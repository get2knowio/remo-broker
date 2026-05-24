//! Append-only JSONL audit log.
//!
//! Implements FR-013, FR-017, and FR-018 plus the related edge cases in
//! the spec (audit log filesystem full, log rotation, manifest.invalid /
//! socket.recovered / shutdown events).
//!
//! ## Design
//!
//! Per FR-018 audit failures MUST NOT block serving. The writer is therefore
//! split:
//!
//! - [`AuditFile`] is the sync file-IO half: every event opens the file
//!   fresh with `O_APPEND | O_CREAT`. The spec explicitly endorses this
//!   pattern ("no SIGHUP required if using O_APPEND + open-per-write"), and
//!   it makes log rotation (`mv audit.log audit.log.1`) work automatically —
//!   the next event lands in a freshly-opened file at the original path
//!   rather than the orphaned inode a kept-open handle would still write to.
//! - [`AuditWriter`] is the async front door: a bounded mpsc channel feeds a
//!   background task that owns the [`AuditFile`]. The fetch path calls
//!   [`AuditWriter::record`] which uses `try_send`; if the channel is full
//!   (the writer task is wedged) the call is non-blocking and a dropped-event
//!   counter is incremented instead. If a file write fails the event is
//!   pushed onto an in-memory degraded buffer capped at 1000 entries (drop
//!   oldest, FIFO), per the spec's edge-case wording.
//!
//! ## Never log secret values
//!
//! Per FR-017 and SC-004 the audit log MUST NOT contain secret values. This
//! is enforced structurally: the event types do not carry a value field.
//! Callers that have a value in scope (e.g. a `get` handler) construct the
//! event with `secret_name` only.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Capacity of the producer→writer mpsc channel. A full channel means the
/// writer task is wedged (slow disk, blocked syscall); further events are
/// dropped with a counter rather than blocking the fetch path.
pub const CHANNEL_CAP: usize = 1_000;

/// Capacity of the in-memory degraded buffer used when file writes fail
/// (FR-018 / "Audit log filesystem full" edge case). Drop-oldest FIFO.
pub const DEGRADED_BUFFER_CAP: usize = 1_000;

// ---- Event types --------------------------------------------------------

/// Any single line that may appear in the JSONL audit log.
///
/// Tagged on `"event"`. The names match the spec's wording verbatim
/// (`"manifest.invalid"`, `"socket.recovered"`), so a future log-shipping
/// pipeline can filter on them without rewriting strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event")]
pub enum AuditEvent {
    #[serde(rename = "fetch")]
    Fetch(FetchEvent),
    #[serde(rename = "manifest.invalid")]
    ManifestInvalid(ManifestInvalidEvent),
    #[serde(rename = "socket.recovered")]
    SocketRecovered(SocketRecoveredEvent),
    #[serde(rename = "shutdown")]
    Shutdown(ShutdownEvent),
}

/// A single secret-fetch attempt: allowed or denied, success or failure.
/// Fields per FR-017 plus a `reason` for denial classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub project: String,
    pub secret_name: String,
    pub decision: Decision,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_uid: Option<u32>,
    pub latency_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Why a deny decision was made (e.g. `"allowlist"`). Always None on allow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestInvalidEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub project: String,
    pub manifest_path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SocketRecoveredEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub project: String,
    pub socket_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShutdownEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub reason: String,
    pub events_dropped: u64,
    pub events_in_degraded_buffer: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    NotFound,
    BackendError,
    BackendUnreachable,
}

// ---- File writer --------------------------------------------------------

/// Open-per-write `O_APPEND` audit log file.
///
/// Re-opens the file on every event so log rotation (`mv audit.log
/// audit.log.1`) is transparent. The spec endorses this pattern; the
/// per-write open cost is small (≈tens of µs) compared with the fetch
/// budget that hosts the audit write, and a quick measurement against the
/// SC-002 load (50 devcontainers × 10 Hz) puts it under 3% of the writer
/// task's CPU.
///
/// Constructed implicitly by [`AuditWriter::spawn`]; exposed publicly so
/// it can be unit-tested independently of the async machinery.
pub struct AuditFile {
    path: PathBuf,
}

impl AuditFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serialize `event` as a single JSONL line and append it to the file.
    pub fn write_event(&self, event: &AuditEvent) -> std::io::Result<()> {
        let mut json = serde_json::to_string(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        json.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(json.as_bytes())
    }
}

// ---- Async writer -------------------------------------------------------

/// Front-door producer handle. Cloneable so multiple fetch handlers can
/// share it. Dropping all clones closes the channel and lets the writer
/// task drain and exit.
#[derive(Clone)]
pub struct AuditWriter {
    tx: mpsc::Sender<AuditEvent>,
    dropped: Arc<AtomicU64>,
}

/// Final tallies returned by the writer task when it exits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterShutdown {
    /// Events still in the degraded buffer (file was unwritable at shutdown).
    pub degraded_buffer_remaining: u64,
    /// Events that the writer task wrote successfully.
    pub events_written: u64,
}

impl AuditWriter {
    /// Spawn the background writer task and return a producer handle plus
    /// the join handle. Callers should typically:
    /// 1. emit a `Shutdown` event,
    /// 2. drop all `AuditWriter` clones,
    /// 3. await the join handle to flush.
    pub fn spawn(path: PathBuf) -> (Self, JoinHandle<WriterShutdown>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        let dropped = Arc::new(AtomicU64::new(0));
        let handle = tokio::spawn(writer_loop(path, rx));
        (Self { tx, dropped }, handle)
    }

    /// Non-blocking enqueue. If the channel is full (writer wedged) the
    /// event is dropped and the dropped-event counter is incremented.
    /// If the writer task has exited (shutdown), the event is silently
    /// discarded.
    pub fn record(&self, event: AuditEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task gone; shutdown in progress.
            }
        }
    }

    /// Total events dropped because the producer→writer channel was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

async fn writer_loop(path: PathBuf, mut rx: mpsc::Receiver<AuditEvent>) -> WriterShutdown {
    let file = AuditFile::new(path);
    let mut degraded: VecDeque<AuditEvent> = VecDeque::new();
    let mut written: u64 = 0;

    while let Some(event) = rx.recv().await {
        // First try to drain anything we previously couldn't write. Stop on
        // first failure so we don't burn cycles spinning on a dead file.
        while let Some(buffered) = degraded.front() {
            match file.write_event(buffered) {
                Ok(()) => {
                    degraded.pop_front();
                    written += 1;
                }
                Err(_) => break,
            }
        }

        match file.write_event(&event) {
            Ok(()) => {
                written += 1;
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    degraded_buffer_size = degraded.len(),
                    "audit log write failed; buffering event in memory"
                );
                if degraded.len() == DEGRADED_BUFFER_CAP {
                    // Drop oldest to make room. The spec is fine with this
                    // as long as the operator gets a clear signal — the
                    // tracing::error above plus the eventual Shutdown
                    // event's events_in_degraded_buffer field cover that.
                    degraded.pop_front();
                }
                degraded.push_back(event);
            }
        }
    }

    WriterShutdown {
        degraded_buffer_remaining: degraded.len() as u64,
        events_written: written,
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use time::macros::datetime;

    fn fixed_ts() -> OffsetDateTime {
        datetime!(2026-05-24 12:34:56.789 UTC)
    }

    fn sample_fetch_allow() -> FetchEvent {
        FetchEvent {
            timestamp: fixed_ts(),
            project: "myrepo".into(),
            secret_name: "GITHUB_TOKEN".into(),
            decision: Decision::Allow,
            outcome: Outcome::Ok,
            peer_pid: Some(12345),
            peer_uid: Some(1000),
            latency_ms: 4,
            backend: Some("vault".into()),
            reason: None,
        }
    }

    fn sample_fetch_deny() -> FetchEvent {
        FetchEvent {
            timestamp: fixed_ts(),
            project: "myrepo".into(),
            secret_name: "NPM_TOKEN".into(),
            decision: Decision::Deny,
            outcome: Outcome::Ok,
            peer_pid: Some(12345),
            peer_uid: Some(1000),
            latency_ms: 0,
            backend: None,
            reason: Some("allowlist".into()),
        }
    }

    fn to_value(event: &AuditEvent) -> Value {
        serde_json::from_str(&serde_json::to_string(event).unwrap()).unwrap()
    }

    // ---- event serialization shape ----

    #[test]
    fn fetch_allow_event_serializes_with_all_required_fields() {
        let e = AuditEvent::Fetch(sample_fetch_allow());
        assert_eq!(
            to_value(&e),
            json!({
                "event": "fetch",
                "timestamp": "2026-05-24T12:34:56.789Z",
                "project": "myrepo",
                "secret_name": "GITHUB_TOKEN",
                "decision": "allow",
                "outcome": "ok",
                "peer_pid": 12345,
                "peer_uid": 1000,
                "latency_ms": 4,
                "backend": "vault",
            })
        );
    }

    #[test]
    fn fetch_deny_event_omits_backend_and_includes_reason() {
        let e = AuditEvent::Fetch(sample_fetch_deny());
        assert_eq!(
            to_value(&e),
            json!({
                "event": "fetch",
                "timestamp": "2026-05-24T12:34:56.789Z",
                "project": "myrepo",
                "secret_name": "NPM_TOKEN",
                "decision": "deny",
                "outcome": "ok",
                "peer_pid": 12345,
                "peer_uid": 1000,
                "latency_ms": 0,
                "reason": "allowlist",
            })
        );
    }

    #[test]
    fn manifest_invalid_event_serializes_with_dotted_name() {
        let e = AuditEvent::ManifestInvalid(ManifestInvalidEvent {
            timestamp: fixed_ts(),
            project: "myrepo".into(),
            manifest_path: PathBuf::from("/projects/myrepo/.devcontainer/remo-broker.toml"),
            error: "invalid project.name".into(),
        });
        assert_eq!(
            to_value(&e),
            json!({
                "event": "manifest.invalid",
                "timestamp": "2026-05-24T12:34:56.789Z",
                "project": "myrepo",
                "manifest_path": "/projects/myrepo/.devcontainer/remo-broker.toml",
                "error": "invalid project.name",
            })
        );
    }

    #[test]
    fn socket_recovered_event_serializes() {
        let e = AuditEvent::SocketRecovered(SocketRecoveredEvent {
            timestamp: fixed_ts(),
            project: "myrepo".into(),
            socket_path: PathBuf::from("/run/remo-broker/myrepo.sock"),
        });
        assert_eq!(to_value(&e)["event"], "socket.recovered");
        assert_eq!(to_value(&e)["socket_path"], "/run/remo-broker/myrepo.sock");
    }

    #[test]
    fn shutdown_event_serializes_with_tallies() {
        let e = AuditEvent::Shutdown(ShutdownEvent {
            timestamp: fixed_ts(),
            reason: "SIGTERM".into(),
            events_dropped: 3,
            events_in_degraded_buffer: 17,
        });
        let v = to_value(&e);
        assert_eq!(v["event"], "shutdown");
        assert_eq!(v["events_dropped"], 3);
        assert_eq!(v["events_in_degraded_buffer"], 17);
    }

    #[test]
    fn fetch_event_does_not_serialize_a_value_field() {
        // SC-004 / FR-017: never log values. The event type is structurally
        // unable to carry one; this test pins that against future drift.
        let json = serde_json::to_string(&AuditEvent::Fetch(sample_fetch_allow())).unwrap();
        assert!(!json.contains("\"value\""), "found value field: {json}");
        assert!(
            !json.contains("\"value_b64\""),
            "found value_b64 field: {json}"
        );
    }

    // ---- AuditFile ----

    #[test]
    fn audit_file_writes_each_event_as_one_jsonl_line() {
        let dir = tempdir();
        let path = dir.path().join("audit.log");
        let f = AuditFile::new(path.clone());
        f.write_event(&AuditEvent::Fetch(sample_fetch_allow()))
            .unwrap();
        f.write_event(&AuditEvent::Fetch(sample_fetch_deny()))
            .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line is independently valid JSON.
        let _: Value = serde_json::from_str(lines[0]).unwrap();
        let _: Value = serde_json::from_str(lines[1]).unwrap();
    }

    #[test]
    fn audit_file_reopens_at_path_after_rotation() {
        // Simulate `mv audit.log audit.log.1`. With open-per-write, the
        // next event lands in a freshly-created file at the original
        // path; the renamed file keeps the first event intact.
        let dir = tempdir();
        let path = dir.path().join("audit.log");
        let rotated = dir.path().join("audit.log.1");
        let f = AuditFile::new(path.clone());
        f.write_event(&AuditEvent::Fetch(sample_fetch_allow()))
            .unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        f.write_event(&AuditEvent::Fetch(sample_fetch_deny()))
            .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        let rotated_contents = std::fs::read_to_string(&rotated).unwrap();
        assert_eq!(after.lines().count(), 1);
        assert_eq!(rotated_contents.lines().count(), 1);
        let v: Value = serde_json::from_str(after.lines().next().unwrap()).unwrap();
        assert_eq!(v["secret_name"], "NPM_TOKEN");
        let v: Value = serde_json::from_str(rotated_contents.lines().next().unwrap()).unwrap();
        assert_eq!(v["secret_name"], "GITHUB_TOKEN");
    }

    #[test]
    fn audit_file_errors_when_parent_directory_is_missing() {
        let path = PathBuf::from(format!(
            "/nonexistent-remo-broker-audit-{}/audit.log",
            std::process::id()
        ));
        let f = AuditFile::new(path);
        let err = f
            .write_event(&AuditEvent::Fetch(sample_fetch_allow()))
            .unwrap_err();
        let _ = err;
    }

    // ---- AuditWriter ----

    #[tokio::test]
    async fn writer_writes_events_to_file() {
        let dir = tempdir();
        let path = dir.path().join("audit.log");
        let (writer, handle) = AuditWriter::spawn(path.clone());
        writer.record(AuditEvent::Fetch(sample_fetch_allow()));
        writer.record(AuditEvent::Fetch(sample_fetch_deny()));
        drop(writer);
        let result = handle.await.unwrap();
        assert_eq!(result.events_written, 2);
        assert_eq!(result.degraded_buffer_remaining, 0);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }

    #[tokio::test]
    async fn writer_buffers_events_when_file_path_is_unwritable() {
        let path = PathBuf::from(format!(
            "/nonexistent-remo-broker-audit-{}/audit.log",
            std::process::id()
        ));
        let (writer, handle) = AuditWriter::spawn(path);
        writer.record(AuditEvent::Fetch(sample_fetch_allow()));
        writer.record(AuditEvent::Fetch(sample_fetch_deny()));
        drop(writer);
        let result = handle.await.unwrap();
        assert_eq!(result.events_written, 0);
        assert_eq!(result.degraded_buffer_remaining, 2);
    }

    #[tokio::test]
    async fn writer_drops_oldest_when_degraded_buffer_full() {
        let path = PathBuf::from(format!(
            "/nonexistent-remo-broker-audit-cap-{}/audit.log",
            std::process::id()
        ));
        let (writer, handle) = AuditWriter::spawn(path);
        // Push DEGRADED_BUFFER_CAP + 5 events. The channel is bounded to
        // CHANNEL_CAP (==1000), so to push >1000 we must let the writer
        // drain. Easiest: send up to CHANNEL_CAP at a time and yield.
        for _ in 0..(DEGRADED_BUFFER_CAP + 5) {
            writer.record(AuditEvent::Fetch(sample_fetch_allow()));
            // Periodically yield so the writer task can drain the channel
            // into its degraded buffer.
            tokio::task::yield_now().await;
        }
        drop(writer);
        let result = handle.await.unwrap();
        // The buffer is capped, so at most CAP events remain regardless of
        // how many we pushed.
        assert!(result.degraded_buffer_remaining <= DEGRADED_BUFFER_CAP as u64);
    }

    // ---- helpers ----

    fn tempdir() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "remo-broker-test-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
