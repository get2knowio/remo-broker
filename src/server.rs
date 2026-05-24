//! Daemon harness, admin socket loop, and per-project socket loops.
//!
//! Implements:
//! - FR-006: create socket_dir + admin socket at startup (mode 0600).
//! - FR-007: bind per-project sockets on `register` (mode 0660).
//! - FR-008/FR-009: remove sockets on `unregister` / shutdown; tolerate stale
//!   socket files.
//! - FR-012: allowlist check happens before any backend round-trip.
//! - FR-019/FR-020: speak the admin and project wire protocols; advertise
//!   `broker_version` + `protocol_version` in `status` and `ping`.
//! - FR-021: send `READY=1` via `sd_notify` after sockets are bound.
//! - FR-022: handle SIGTERM by stopping accept, draining in-flight up to 5s,
//!   then exiting cleanly.
//! - FR-024: per-connection spawn — no global serialization across projects.
//!
//! `rotate-bootstrap` and the actual backend fetch in `get` remain stubbed
//! (returning `internal_error` / `backend_error`) until fnox-core integration
//! lands. Allowlist denial in `get` is fully wired (FR-012).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::{JoinHandle, JoinSet};

use crate::audit::{AuditEvent, AuditWriter, ShutdownEvent, WriterShutdown};
use crate::config::{BootstrapSource, Config};
use crate::proto::MAX_MESSAGE_BYTES;
use crate::proto::admin::{
    AdminError, AdminErrorCode, AdminRequest, BootstrapMode, OkResponse, ProjectStatus,
    RegisterResponse, ReloadResponse, StatusResponse,
};
use crate::proto::project::{
    GetResponse, InfoResponse, PingResponse, ProjectError, ProjectErrorCode, ProjectRequest,
};
use crate::registry::{CacheDefaults, Project, ProjectRegistry, RegistryError};
use secrecy::ExposeSecret;

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
    registry: Arc<ProjectRegistry>,
    /// Accept-loop `JoinHandle`s for currently-registered projects, keyed by
    /// project name. Populated on `register`, drained on `unregister` /
    /// shutdown. Held inside a `Mutex` (not `RwLock`) because the only
    /// readers also mutate.
    project_tasks: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Server {
    pub fn new(config: Config, audit: AuditWriter) -> Self {
        let cache_defaults = CacheDefaults {
            max_entries: config.cache_default_max_entries,
            ttl: config.cache_default_ttl,
        };
        let registry = Arc::new(ProjectRegistry::new(
            config.socket_dir.clone(),
            cache_defaults,
        ));
        Self {
            config,
            audit,
            started_at: Instant::now(),
            registry,
            project_tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Borrowed handle to the project registry. Callers (currently tests +
    /// future fetch-path wiring) can use this to look up projects without
    /// going through the admin socket. Cloning the returned `Arc` is cheap.
    pub fn registry(&self) -> Arc<ProjectRegistry> {
        Arc::clone(&self.registry)
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

        // FR-022: drain in-flight up to 5s, then bail. The 5s budget is
        // shared across all project loops and the admin loop so total
        // shutdown stays bounded regardless of how many projects are
        // registered.
        sd_notify_stopping();
        let shutdown_deadline = Instant::now() + SHUTDOWN_DRAIN;

        // Drain project accept loops first: signal each project's `stop`,
        // then await its accept-loop task (with abort on deadline so
        // hung connections don't leak `Arc<Server>` and stall audit drain).
        drain_project_loops(&shared, shutdown_deadline).await;

        // Remove project sockets after their loops have exited (FR-008).
        for project in shared.registry.snapshot().await {
            if let Err(e) = std::fs::remove_file(&project.socket_path) {
                tracing::warn!(
                    error = %e,
                    path = %project.socket_path.display(),
                    project = %project.name,
                    "failed to remove project socket on shutdown"
                );
            }
        }

        // Remaining drain budget for admin connections (FR-022).
        let remaining = shutdown_deadline.saturating_duration_since(Instant::now());
        drain_join_set(&mut connections, remaining).await;

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
                registry: arc.registry.clone(),
                project_tasks: Mutex::new(HashMap::new()),
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
            Ok(req) => dispatch_admin(&server, req).await,
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

async fn dispatch_admin(server: &Arc<Server>, req: AdminRequest) -> String {
    match req {
        AdminRequest::Status => dispatch_status(server).await,
        AdminRequest::Register { name, project_path } => {
            dispatch_register(server, &name, &project_path).await
        }
        AdminRequest::Unregister { name } => dispatch_unregister(server, &name).await,
        AdminRequest::Reload { name } => dispatch_reload(server, &name).await,
        AdminRequest::RotateBootstrap => serde_json::to_string(&AdminError::new(
            AdminErrorCode::InternalError,
            "rotate-bootstrap not yet implemented in this build",
        ))
        .expect("AdminError always serializes"),
    }
}

async fn dispatch_status(server: &Arc<Server>) -> String {
    let projects = server.registry.snapshot().await;
    let project_statuses: Vec<ProjectStatus> = projects
        .iter()
        .map(|p| {
            let m = p.manifest();
            ProjectStatus {
                name: p.name.clone(),
                socket_path: p.socket_path.clone(),
                allowlist_size: m.allowlist.secrets.len() as u32,
                cache_entries: p.cache.len() as u32,
            }
        })
        .collect();
    let resp = StatusResponse::new(
        env!("CARGO_PKG_VERSION"),
        server.started_at.elapsed().as_secs(),
        bootstrap_mode(&server.config.bootstrap),
        project_statuses,
    );
    serde_json::to_string(&resp).expect("StatusResponse always serializes")
}

async fn dispatch_register(server: &Arc<Server>, name: &str, project_path: &Path) -> String {
    match server.registry.register(name, project_path).await {
        Ok((project, listener)) => {
            let socket_path = project.socket_path.clone();
            let server_for_task = Arc::clone(server);
            let project_for_task = Arc::clone(&project);
            let handle = tokio::spawn(async move {
                run_project_socket(server_for_task, project_for_task, listener).await;
            });
            server
                .project_tasks
                .lock()
                .await
                .insert(name.to_string(), handle);
            tracing::info!(project = %name, socket = %socket_path.display(), "project registered");
            let resp = RegisterResponse::new(socket_path);
            serde_json::to_string(&resp).expect("RegisterResponse always serializes")
        }
        Err(e) => admin_error_for(&e),
    }
}

async fn dispatch_unregister(server: &Arc<Server>, name: &str) -> String {
    match server.registry.unregister(name).await {
        Ok(project) => {
            project.stop.notify_waiters();
            let handle = server.project_tasks.lock().await.remove(name);
            if let Some(mut h) = handle {
                // Cap per-project unregister at SHUTDOWN_DRAIN so a wedged
                // connection on one project doesn't pin the admin loop.
                match tokio::time::timeout(SHUTDOWN_DRAIN, &mut h).await {
                    Ok(_) => {}
                    Err(_) => {
                        tracing::warn!(project = %name, "unregister drain timeout; aborting");
                        h.abort();
                        let _ = h.await;
                    }
                }
            }
            if let Err(e) = std::fs::remove_file(&project.socket_path) {
                tracing::warn!(
                    error = %e,
                    project = %name,
                    path = %project.socket_path.display(),
                    "failed to remove project socket on unregister"
                );
            }
            tracing::info!(project = %name, "project unregistered");
            serde_json::to_string(&OkResponse::new()).expect("OkResponse always serializes")
        }
        Err(e) => admin_error_for(&e),
    }
}

async fn dispatch_reload(server: &Arc<Server>, name: &str) -> String {
    match server.registry.reload(name).await {
        Ok(manifest) => {
            tracing::info!(project = %name, "manifest reloaded");
            let resp = ReloadResponse::new(manifest.allowlist.secrets.clone());
            serde_json::to_string(&resp).expect("ReloadResponse always serializes")
        }
        Err(e) => admin_error_for(&e),
    }
}

fn admin_error_for(e: &RegistryError) -> String {
    let code = match e {
        RegistryError::ProjectExists(_) => AdminErrorCode::ProjectExists,
        RegistryError::ProjectUnknown(_) => AdminErrorCode::ProjectUnknown,
        RegistryError::ManifestNotFound(_) => AdminErrorCode::ManifestNotFound,
        RegistryError::ManifestInvalid(_) => AdminErrorCode::ManifestInvalid,
        RegistryError::BindSocket { .. } | RegistryError::SocketPerms { .. } => {
            AdminErrorCode::InternalError
        }
    };
    serde_json::to_string(&AdminError::new(code, e.to_string()))
        .expect("AdminError always serializes")
}

fn bootstrap_mode(source: &BootstrapSource) -> BootstrapMode {
    match source {
        BootstrapSource::File { .. } => BootstrapMode::File,
        BootstrapSource::Imds => BootstrapMode::Imds,
        BootstrapSource::Env => BootstrapMode::Env,
    }
}

// ---- project socket loop ------------------------------------------------

/// Per-project accept loop. Lives in a tokio task spawned by `dispatch_register`.
/// Exits when `project.stop` is notified (unregister or daemon shutdown).
async fn run_project_socket(server: Arc<Server>, project: Arc<Project>, listener: UnixListener) {
    let mut connections = JoinSet::new();
    loop {
        // Pin a fresh `notified()` future per iteration and `enable()` it
        // synchronously so a `notify_waiters()` racing with the start of
        // the iteration is captured rather than lost.
        let stop = project.stop.notified();
        tokio::pin!(stop);
        stop.as_mut().enable();

        tokio::select! {
            biased;
            () = stop => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let server = Arc::clone(&server);
                        let project = Arc::clone(&project);
                        connections.spawn(async move {
                            if let Err(e) = handle_project_connection(server, project, stream).await {
                                tracing::warn!(error = %e, "project connection error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, project = %project.name, "project accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
    drain_join_set(&mut connections, SHUTDOWN_DRAIN).await;
}

async fn handle_project_connection(
    server: Arc<Server>,
    project: Arc<Project>,
    stream: UnixStream,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::with_capacity(256);

    loop {
        line.clear();
        let n = read_line_capped(&mut reader, &mut line, MAX_MESSAGE_BYTES).await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        let response_json = match serde_json::from_slice::<ProjectRequest>(&line) {
            Ok(req) => dispatch_project(&server, &project, req),
            Err(e) => serde_json::to_string(&ProjectError::new(
                ProjectErrorCode::ProtocolError,
                format!("malformed request: {e}"),
            ))
            .expect("ProjectError always serializes"),
        };
        write_half.write_all(response_json.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
    }
}

fn dispatch_project(_server: &Server, project: &Project, req: ProjectRequest) -> String {
    match req {
        ProjectRequest::Ping => {
            let resp = PingResponse::new(env!("CARGO_PKG_VERSION"), project.name.clone());
            serde_json::to_string(&resp).expect("PingResponse always serializes")
        }
        ProjectRequest::Info => {
            let manifest = project.manifest();
            let resp = InfoResponse::new(
                project.name.clone(),
                manifest.allowlist.secrets.clone(),
                manifest.schema_version,
            );
            serde_json::to_string(&resp).expect("InfoResponse always serializes")
        }
        ProjectRequest::Get { name } => {
            let manifest = project.manifest();
            // FR-012: allowlist denial does not incur a backend round-trip
            // and does not consult the cache.
            if !manifest.allowlist.secrets.iter().any(|n| n == &name) {
                return serde_json::to_string(&ProjectError::new(
                    ProjectErrorCode::Denied,
                    format!("Secret {name:?} is not in this project's allowlist."),
                ))
                .expect("ProjectError always serializes");
            }
            // Cache hit short-circuits the backend (FR-014). The plaintext
            // boundary is here — `expose_secret()` is the one place per
            // request where the value materialises outside `SecretString`,
            // and it's immediately handed to `serde_json` to write to the
            // socket.
            if let Some(hit) = project.cache.get(&name) {
                let resp = GetResponse::utf8(hit.value.expose_secret(), hit.ttl_seconds);
                return serde_json::to_string(&resp).expect("GetResponse always serializes");
            }
            // Cache miss with no backend wired (FR-004/FR-005 pending) →
            // `backend_error` placeholder. Once fnox-core lands, a
            // successful fetch will `project.cache.insert(name, value, None)`
            // before constructing the response.
            serde_json::to_string(&ProjectError::new(
                ProjectErrorCode::BackendError,
                "backend fetch not yet implemented in this build",
            ))
            .expect("ProjectError always serializes")
        }
    }
}

/// Stop and drain every running project accept loop under a single deadline.
/// On per-task timeout we abort the task so it releases its `Arc<Server>`
/// clone — otherwise the audit writer in main would never observe the last
/// sender drop and would hang the daemon on shutdown.
async fn drain_project_loops(server: &Arc<Server>, deadline: Instant) {
    let projects = server.registry.snapshot().await;
    for project in &projects {
        project.stop.notify_waiters();
    }
    let tasks: HashMap<String, JoinHandle<()>> =
        std::mem::take(&mut *server.project_tasks.lock().await);
    for (name, mut handle) in tasks {
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), &mut handle).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(project = %name, "project drain timeout; aborting");
                handle.abort();
                let _ = handle.await;
            }
        }
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

    /// Build `<root>/<name>/.remo/broker.toml` with the given allowlist and
    /// return the project root path the admin caller would pass.
    fn write_project(root: &Path, name: &str, allowlist: &[&str]) -> PathBuf {
        let project_dir = root.join(name);
        let remo_dir = project_dir.join(".remo");
        std::fs::create_dir_all(&remo_dir).unwrap();
        let secrets = allowlist
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!(
            "schema_version = 1\n\n\
             [project]\n\
             name = \"{name}\"\n\n\
             [allowlist]\n\
             secrets = [{secrets}]\n"
        );
        std::fs::write(remo_dir.join("broker.toml"), manifest).unwrap();
        project_dir
    }

    async fn send_project(socket: &Path, request: &str) -> String {
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

    async fn wait_for_path(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("path never appeared: {}", path.display());
    }

    async fn wait_for_path_gone(path: &Path) {
        for _ in 0..100 {
            if !path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("path lingered: {}", path.display());
    }

    #[tokio::test]
    async fn rotate_bootstrap_returns_internal_error() {
        // Only remaining stub on the admin plane; fnox-core integration
        // replaces this with a real backend re-auth.
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;

        let response = send_admin(&admin_socket, r#"{"op":"rotate-bootstrap"}"#).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "internal_error");

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn register_lifecycle_end_to_end() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);

        // register → socket appears
        let req = format!(
            r#"{{"op":"register","name":"alpha","project_path":"{}"}}"#,
            project_dir.display()
        );
        let response = send_admin(&admin_socket, &req).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], true, "register failed: {response}");
        let project_socket = socket_dir.join("alpha.sock");
        assert_eq!(v["socket_path"], project_socket.to_str().unwrap());
        wait_for_path(&project_socket).await;

        // ping over the project socket works
        let ping = send_project(&project_socket, r#"{"op":"ping"}"#).await;
        let v: Value = serde_json::from_str(&ping).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["project"], "alpha");
        assert_eq!(v["broker_version"], env!("CARGO_PKG_VERSION"));

        // info reports the allowlist atomically loaded
        let info = send_project(&project_socket, r#"{"op":"info"}"#).await;
        let v: Value = serde_json::from_str(&info).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["allowlist"], serde_json::json!(["FOO"]));
        assert_eq!(v["schema_version"], 1);

        // FR-012: allowlist denial does not invoke the backend
        let denied = send_project(&project_socket, r#"{"op":"get","name":"BAR"}"#).await;
        let v: Value = serde_json::from_str(&denied).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "denied");

        // get for an allowlisted name reaches the (stubbed) backend path
        let allowed = send_project(&project_socket, r#"{"op":"get","name":"FOO"}"#).await;
        let v: Value = serde_json::from_str(&allowed).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "backend_error");

        // status reflects the registration
        let status = send_admin(&admin_socket, r#"{"op":"status"}"#).await;
        let v: Value = serde_json::from_str(&status).unwrap();
        let projects = v["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["name"], "alpha");
        assert_eq!(projects[0]["allowlist_size"], 1);

        // unregister tears the socket down
        let response = send_admin(&admin_socket, r#"{"op":"unregister","name":"alpha"}"#).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], true);
        wait_for_path_gone(&project_socket).await;

        // status now empty again
        let status = send_admin(&admin_socket, r#"{"op":"status"}"#).await;
        let v: Value = serde_json::from_str(&status).unwrap();
        assert_eq!(v["projects"], serde_json::json!([]));

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn duplicate_register_returns_project_exists() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let req = format!(
            r#"{{"op":"register","name":"alpha","project_path":"{}"}}"#,
            project_dir.display()
        );
        let _ = send_admin(&admin_socket, &req).await;
        let response = send_admin(&admin_socket, &req).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "project_exists");

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn register_with_missing_manifest_returns_manifest_not_found() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;
        let project_dir = dir.path().join("alpha");
        std::fs::create_dir_all(&project_dir).unwrap();

        let req = format!(
            r#"{{"op":"register","name":"alpha","project_path":"{}"}}"#,
            project_dir.display()
        );
        let response = send_admin(&admin_socket, &req).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "manifest_not_found");

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn reload_propagates_new_allowlist() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let req = format!(
            r#"{{"op":"register","name":"alpha","project_path":"{}"}}"#,
            project_dir.display()
        );
        let _ = send_admin(&admin_socket, &req).await;
        let project_socket = socket_dir.join("alpha.sock");

        // Rewrite the manifest then reload.
        let _ = write_project(dir.path(), "alpha", &["FOO", "BAR"]);
        let response = send_admin(&admin_socket, r#"{"op":"reload","name":"alpha"}"#).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["allowlist"], serde_json::json!(["FOO", "BAR"]));

        // Existing project socket sees the new allowlist (atomic swap, no
        // socket teardown — FR-011 + User Story 3 scenario 2).
        let info = send_project(&project_socket, r#"{"op":"info"}"#).await;
        let v: Value = serde_json::from_str(&info).unwrap();
        assert_eq!(v["allowlist"], serde_json::json!(["FOO", "BAR"]));

        let denied = send_project(&project_socket, r#"{"op":"get","name":"BAR"}"#).await;
        let v: Value = serde_json::from_str(&denied).unwrap();
        // BAR is now in the allowlist post-reload, so we go to the stub
        // backend path rather than denial.
        assert_eq!(v["code"], "backend_error");

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn cache_hit_returns_get_response_and_status_counts_entry() {
        use secrecy::SecretString;

        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit);
        let registry = server.registry();
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let req = format!(
            r#"{{"op":"register","name":"alpha","project_path":"{}"}}"#,
            project_dir.display()
        );
        let _ = send_admin(&admin_socket, &req).await;

        // Pre-populate the cache as the fetch path will once fnox-core lands.
        let project = registry
            .snapshot()
            .await
            .into_iter()
            .find(|p| p.name == "alpha")
            .expect("project registered");
        project
            .cache
            .insert("FOO".into(), SecretString::from("hello".to_string()), None);

        // get FOO must now return the cached value, not the backend stub.
        let project_socket = socket_dir.join("alpha.sock");
        let resp = send_project(&project_socket, r#"{"op":"get","name":"FOO"}"#).await;
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true, "expected cache hit, got: {resp}");
        assert_eq!(v["value"], "hello");
        assert!(v["ttl_seconds"].as_u64().unwrap() > 0);

        // status now reports the cache entry.
        let status = send_admin(&admin_socket, r#"{"op":"status"}"#).await;
        let v: Value = serde_json::from_str(&status).unwrap();
        let p = v["projects"][0].clone();
        assert_eq!(p["name"], "alpha");
        assert_eq!(p["cache_entries"], 1);

        // unregister drops the project Arc, which drops the cache, which
        // zeroizes every entry — the test doesn't inspect post-drop memory
        // directly but verifies the cache_entries count returns to zero.
        let _ = send_admin(&admin_socket, r#"{"op":"unregister","name":"alpha"}"#).await;
        let status = send_admin(&admin_socket, r#"{"op":"status"}"#).await;
        let v: Value = serde_json::from_str(&status).unwrap();
        assert_eq!(v["projects"], serde_json::json!([]));

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn allowlist_denial_does_not_consult_cache() {
        use secrecy::SecretString;

        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit);
        let registry = server.registry();
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let req = format!(
            r#"{{"op":"register","name":"alpha","project_path":"{}"}}"#,
            project_dir.display()
        );
        let _ = send_admin(&admin_socket, &req).await;

        // Insert a cache entry for a name that is NOT in the allowlist.
        // The denial path must still fire — FR-012 says the allowlist
        // check happens before any other work.
        let project = registry.snapshot().await.into_iter().next().unwrap();
        project
            .cache
            .insert("BAR".into(), SecretString::from("oops".to_string()), None);
        let project_socket = socket_dir.join("alpha.sock");
        let resp = send_project(&project_socket, r#"{"op":"get","name":"BAR"}"#).await;
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "denied");
        assert!(
            v.get("value").is_none(),
            "denied response must not leak a value"
        );

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
