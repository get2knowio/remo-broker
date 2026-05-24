//! Daemon harness and admin socket loop.
//!
//! Implements:
//! - FR-006: create socket_dir + admin socket at startup (mode 0600).
//! - FR-008/FR-009: remove sockets on shutdown; tolerate stale socket files.
//! - FR-019/FR-020: speak the admin wire protocol; advertise broker_version
//!   and protocol_version in status.
//! - FR-021: send READY=1 via sd_notify after sockets are bound.
//! - FR-022: handle SIGTERM by stopping accept, draining in-flight up to 5s,
//!   then exiting cleanly.
//!
//! Per-project sockets, register/unregister/reload, and rotate-bootstrap are
//! intentionally stubbed (returning `internal_error`) until the project
//! registry and fnox-core integration land.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::audit::{AuditEvent, AuditWriter, ShutdownEvent, WriterShutdown};
use crate::config::{BootstrapSource, Config};
use crate::proto::MAX_MESSAGE_BYTES;
use crate::proto::admin::{
    AdminError, AdminErrorCode, AdminRequest, BootstrapMode, OkResponse, RegisterResponse,
    ReloadResponse, RotateBootstrapResponse, StatusResponse,
};

/// Max time the daemon waits for in-flight admin connections to finish
/// after SIGTERM. Beyond this we abort the JoinSet and exit (FR-022).
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

const ADMIN_SOCKET_NAME: &str = "admin.sock";
const ADMIN_SOCKET_MODE: u32 = 0o600;
const SOCKET_DIR_MODE: u32 = 0o755;

/// Errors surfaced by [`Server::run`]. Connection-level errors are logged and
/// swallowed by the handler; only setup/teardown failures escape this enum.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to create socket dir {path}: {source}")]
    CreateSocketDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to bind admin socket {path}: {source}")]
    BindAdminSocket {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to set admin socket permissions on {path}: {source}")]
    AdminSocketPerms {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to install SIGTERM handler: {0}")]
    SignalHandler(#[source] std::io::Error),
}

/// Long-lived daemon state shared across admin handlers.
pub struct Server {
    config: Config,
    audit: AuditWriter,
    started_at: Instant,
}

impl Server {
    pub fn new(config: Config, audit: AuditWriter) -> Self {
        Self {
            config,
            audit,
            started_at: Instant::now(),
        }
    }

    /// Run until SIGTERM. Returns when the daemon has cleanly shut down.
    pub async fn run(self) -> Result<(), ServerError> {
        let admin_path = ensure_socket_dir(&self.config.socket_dir)?.join(ADMIN_SOCKET_NAME);

        let listener = bind_admin_socket(&admin_path)?;
        tracing::info!(path = %admin_path.display(), "admin socket bound");

        // FR-021: signal readiness once all sockets are bound.
        sd_notify_ready();

        let shared = Arc::new(self);
        let shutdown = Arc::new(Notify::new());
        install_sigterm(shutdown.clone())?;

        let mut connections = JoinSet::new();

        // Accept loop.
        loop {
            tokio::select! {
                biased;
                () = shutdown.notified() => {
                    tracing::info!("SIGTERM received; draining in-flight connections");
                    break;
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let server = shared.clone();
                            connections.spawn(async move {
                                if let Err(e) = handle_admin_connection(server, stream).await {
                                    tracing::warn!(error = %e, "admin connection error");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "admin accept failed");
                            // Brief backoff so we don't spin on a persistent failure.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        }

        // FR-022: drain in-flight up to 5s, then bail.
        sd_notify_stopping();
        drain_join_set(&mut connections, SHUTDOWN_DRAIN).await;

        // Remove the admin socket so the next daemon start binds cleanly
        // (FR-008). Don't fail shutdown on cleanup error — log and move on.
        if let Err(e) = std::fs::remove_file(&admin_path) {
            tracing::warn!(error = %e, path = %admin_path.display(), "failed to remove admin socket on shutdown");
        }

        // Flush audit: emit final Shutdown event then drop our writer clone
        // so the writer task can drain and exit.
        let server = Arc::try_unwrap(shared).unwrap_or_else(|arc| {
            // Should be unreachable: connections JoinSet is empty by this
            // point and held the only other strong ref. If something has
            // leaked an Arc we still want a graceful shutdown.
            tracing::warn!("server Arc had outstanding clones at shutdown");
            Server {
                config: arc.config.clone(),
                audit: arc.audit.clone(),
                started_at: arc.started_at,
            }
        });
        let WriterShutdown {
            events_written,
            degraded_buffer_remaining,
        } = server.shutdown_audit().await;
        tracing::info!(
            events_written,
            degraded_buffer_remaining,
            "audit writer drained"
        );

        Ok(())
    }

    async fn shutdown_audit(self) -> WriterShutdown {
        self.audit.record(AuditEvent::Shutdown(ShutdownEvent {
            timestamp: OffsetDateTime::now_utc(),
            reason: "sigterm".into(),
            events_dropped: self.audit.dropped(),
            events_in_degraded_buffer: 0,
        }));
        drop(self.audit);
        // The writer's JoinHandle is owned by whatever spawned it; this
        // method just signals drain by dropping our handle. The caller
        // (typically main) awaits the writer task separately.
        WriterShutdown::default()
    }
}

// ---- setup ---------------------------------------------------------------

fn ensure_socket_dir(path: &Path) -> Result<PathBuf, ServerError> {
    std::fs::create_dir_all(path).map_err(|source| ServerError::CreateSocketDir {
        path: path.to_path_buf(),
        source,
    })?;
    // Best-effort: tighten perms to 0755 (FR-006). Ignore failures on
    // filesystems that don't support chmod (rare on Linux but possible in
    // containers / tmpfs configurations).
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_DIR_MODE));
    Ok(path.to_path_buf())
}

fn bind_admin_socket(path: &Path) -> Result<UnixListener, ServerError> {
    // FR-009: tolerate stale socket files. `remove_file` is a no-op if the
    // file doesn't exist (NotFound is ignored); any other error is fatal.
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(ServerError::BindAdminSocket {
            path: path.to_path_buf(),
            source: e,
        });
    }
    let listener = UnixListener::bind(path).map_err(|source| ServerError::BindAdminSocket {
        path: path.to_path_buf(),
        source,
    })?;
    // FR-006: admin socket mode 0600 (root-only).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(ADMIN_SOCKET_MODE)).map_err(
        |source| ServerError::AdminSocketPerms {
            path: path.to_path_buf(),
            source,
        },
    )?;
    Ok(listener)
}

fn install_sigterm(shutdown: Arc<Notify>) -> Result<(), ServerError> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).map_err(ServerError::SignalHandler)?;
    let mut intr = signal(SignalKind::interrupt()).map_err(ServerError::SignalHandler)?;
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = intr.recv() => {}
        }
        shutdown.notify_waiters();
    });
    Ok(())
}

async fn drain_join_set(set: &mut JoinSet<()>, limit: Duration) {
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            res = set.join_next() => {
                if res.is_none() { break; }
            }
            () = &mut deadline => {
                tracing::warn!(remaining = set.len(), "shutdown drain timeout; aborting in-flight connections");
                set.abort_all();
                while set.join_next().await.is_some() {}
                break;
            }
        }
    }
}

// ---- sd_notify (no-op outside systemd) ----------------------------------

fn sd_notify_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY=1 failed (probably not running under systemd)");
    }
}

fn sd_notify_stopping() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
        tracing::debug!(error = %e, "sd_notify STOPPING=1 failed");
    }
}

// ---- connection handler -------------------------------------------------

async fn handle_admin_connection(server: Arc<Server>, stream: UnixStream) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::with_capacity(256);

    loop {
        line.clear();
        let n = read_line_capped(&mut reader, &mut line, MAX_MESSAGE_BYTES).await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        let response_json = match serde_json::from_slice::<AdminRequest>(&line) {
            Ok(req) => dispatch_admin(&server, req),
            Err(e) => serde_json::to_string(&AdminError::new(
                AdminErrorCode::ProtocolError,
                format!("malformed request: {e}"),
            ))
            .expect("AdminError always serializes"),
        };
        write_half.write_all(response_json.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
    }
}

/// Read up to one `\n`-terminated line, capped at `max_bytes`. Returns the
/// number of bytes read (0 = EOF). On cap exceeded, drains the line and
/// returns an error so the caller closes the connection.
async fn read_line_capped<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize>
where
    R: AsyncBufReadExt + Unpin,
{
    let n = reader.take(max_bytes as u64).read_until(b'\n', buf).await?;
    if n == max_bytes && !buf.ends_with(b"\n") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("admin request exceeds max message size of {max_bytes} bytes"),
        ));
    }
    Ok(n)
}

fn dispatch_admin(server: &Server, req: AdminRequest) -> String {
    match req {
        AdminRequest::Status => {
            let resp = StatusResponse::new(
                env!("CARGO_PKG_VERSION"),
                server.started_at.elapsed().as_secs(),
                bootstrap_mode(&server.config.bootstrap),
                Vec::new(),
            );
            serde_json::to_string(&resp).expect("StatusResponse always serializes")
        }
        AdminRequest::Register { .. }
        | AdminRequest::Unregister { .. }
        | AdminRequest::Reload { .. }
        | AdminRequest::RotateBootstrap => {
            // Stubs: real implementations land with the project registry +
            // fnox-core integration. Returned shape exists so clients can
            // depend on the call-and-response contract today.
            let _ = std::any::type_name_of_val(&RegisterResponse::new("/"));
            let _ = OkResponse::new();
            let _ = ReloadResponse::new(Vec::<String>::new());
            let _ = RotateBootstrapResponse::ok();
            serde_json::to_string(&AdminError::new(
                AdminErrorCode::InternalError,
                "operation not yet implemented in this build",
            ))
            .expect("AdminError always serializes")
        }
    }
}

fn bootstrap_mode(source: &BootstrapSource) -> BootstrapMode {
    match source {
        BootstrapSource::File { .. } => BootstrapMode::File,
        BootstrapSource::Imds => BootstrapMode::Imds,
        BootstrapSource::Env => BootstrapMode::Env,
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Overrides;
    use crate::proto::PROTOCOL_VERSION;
    use serde_json::Value;
    use tokio::io::AsyncWriteExt;

    fn test_config(socket_dir: &Path, audit_log: &Path) -> Config {
        // The server reports the bootstrap mode in status; it doesn't fetch
        // the token itself, so any source kind works here.
        let overrides = Overrides {
            socket_dir: Some(socket_dir.to_path_buf()),
            audit_log_path: Some(audit_log.to_path_buf()),
            bootstrap_source: Some(crate::config::BootstrapSourceKind::File),
            bootstrap_token_path: Some(PathBuf::from("/tmp/unused-in-this-test")),
            ..Default::default()
        };
        Config::from_toml_str("", &overrides).unwrap()
    }

    fn tempdir() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "remo-broker-test-server-{}-{}",
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

    async fn send_admin(socket: &Path, request: &str) -> String {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(request.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();
        write.shutdown().await.unwrap();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    }

    #[tokio::test]
    async fn status_round_trip() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let config = test_config(&socket_dir, &audit_log);
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(config, audit);

        let server_handle = tokio::spawn(server.run());

        // Wait for the socket to appear (bind happens early in run()).
        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        for _ in 0..50 {
            if admin_socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(admin_socket.exists(), "admin socket never appeared");

        let response = send_admin(&admin_socket, r#"{"op":"status"}"#).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(v["broker_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["bootstrap_mode"], "file");
        assert_eq!(v["projects"], Value::Array(vec![]));

        // Trigger shutdown by sending SIGTERM to ourselves. (The test process
        // is the same one running the server; SIGTERM here would kill the
        // test runner, so instead we abort the server handle.)
        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn unknown_op_returns_protocol_error() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let config = test_config(&socket_dir, &audit_log);
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(config, audit);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        for _ in 0..50 {
            if admin_socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let response = send_admin(&admin_socket, r#"{"op":"shutdown"}"#).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "protocol_error");

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn stub_ops_return_internal_error() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let config = test_config(&socket_dir, &audit_log);
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(config, audit);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        for _ in 0..50 {
            if admin_socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        for op in [
            r#"{"op":"register","name":"a","project_path":"/p"}"#,
            r#"{"op":"unregister","name":"a"}"#,
            r#"{"op":"reload","name":"a"}"#,
            r#"{"op":"rotate-bootstrap"}"#,
        ] {
            let response = send_admin(&admin_socket, op).await;
            let v: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(v["ok"], false, "op={op} response={response}");
            assert_eq!(v["code"], "internal_error", "op={op}");
        }

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn stale_admin_socket_is_replaced_on_bind() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        // Create a stale file at the bind path.
        std::fs::write(&admin_socket, b"stale").unwrap();

        let audit_log = dir.path().join("audit.log");
        let config = test_config(&socket_dir, &audit_log);
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(config, audit);
        let server_handle = tokio::spawn(server.run());

        // The bind should succeed despite the stale file (FR-009).
        for _ in 0..50 {
            if let Ok(_s) = UnixStream::connect(&admin_socket).await {
                server_handle.abort();
                let _ = server_handle.await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("admin socket never became connectable after stale-file cleanup");
    }
}
