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
//! Backend retrieval delegates to `BackendSession` (fnox-core). When the
//! daemon starts without a usable session (no `--fnox-config`, no
//! discoverable `fnox.toml`) it degrades: admin/ping/info/cache-hit traffic
//! continues to work; `get` cache misses surface as `backend_error` and
//! `rotate-bootstrap` returns `bootstrap_error`.

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

use crate::audit::{
    AuditEvent, AuditWriter, Decision, FetchEvent, Outcome, ShutdownEvent, WriterShutdown,
};
use crate::backend::BackendSession;
use crate::bootstrap::fetch_token;
use crate::config::{BootstrapSource, Config};
use crate::proto::MAX_MESSAGE_BYTES;
use crate::proto::admin::{
    AdminError, AdminErrorCode, AdminRequest, BootstrapMode, OkResponse, ProjectStatus,
    RegisterResponse, ReloadResponse, RotateBootstrapResponse, StatusResponse,
};
use crate::proto::project::{
    GetResponse, InfoResponse, PingResponse, ProjectError, ProjectErrorCode, ProjectRequest,
};
use crate::registry::{CacheDefaults, Project, ProjectRegistry, RegistryError};
use secrecy::{ExposeSecret, SecretString};

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
    /// Backend session (`fnox-core`). `None` means the broker started in
    /// degraded mode — `get` returns `backend_error`, `rotate-bootstrap`
    /// returns `bootstrap_error`. Constructed by `main.rs`; never installed
    /// or torn down by `Server` itself (only `rotate-bootstrap` swaps the
    /// inner `Fnox`).
    backend: Option<BackendSession>,
    /// Accept-loop `JoinHandle`s for currently-registered projects, keyed by
    /// project name. Populated on `register`, drained on `unregister` /
    /// shutdown. Held inside a `Mutex` (not `RwLock`) because the only
    /// readers also mutate.
    project_tasks: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Server {
    pub fn new(config: Config, audit: AuditWriter, backend: Option<BackendSession>) -> Self {
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
            backend,
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
                backend: arc.backend.clone(),
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
        AdminRequest::RotateBootstrap => dispatch_rotate_bootstrap(server).await,
    }
}

/// User Story 5: re-read the bootstrap token, re-construct the fnox-core
/// session, and atomically swap it in. On any failure the old session is
/// kept (User Story 5 acceptance scenario 3) and we return `bootstrap_error`
/// — the daemon keeps serving from cache.
///
/// Cache entries from before the rotation survive because the cache lives
/// on `Project`, not on the backend session.
async fn dispatch_rotate_bootstrap(server: &Arc<Server>) -> String {
    let Some(backend) = server.backend.as_ref() else {
        return serde_json::to_string(&AdminError::new(
            AdminErrorCode::BootstrapError,
            "no fnox-core session is configured; restart the daemon with --fnox-config",
        ))
        .expect("AdminError always serializes");
    };

    // FR-003-style validation: confirm a usable bootstrap token still exists
    // before touching the backend session. If the operator wrote a new
    // token file, this re-reads it; if the file went away, we surface the
    // error without disturbing the in-flight session.
    if let Err(e) = fetch_token(&server.config.bootstrap).await {
        return serde_json::to_string(&AdminError::new(
            AdminErrorCode::BootstrapError,
            format!("bootstrap token unavailable: {e}"),
        ))
        .expect("AdminError always serializes");
    }

    // Construct a fresh Fnox using the same config path policy as startup
    // (open if explicitly named, else discover).
    let new = match &server.config.fnox_config_path {
        Some(path) => BackendSession::open(path),
        None => BackendSession::discover(),
    };
    let new = match new {
        Ok(b) => b,
        Err(e) => {
            return serde_json::to_string(&AdminError::new(
                AdminErrorCode::BootstrapError,
                format!("failed to rebuild fnox-core session: {e}"),
            ))
            .expect("AdminError always serializes");
        }
    };

    // Adopt the new session by atomic swap. Existing in-flight `get` calls
    // that already loaded the old Fnox arc complete against it; subsequent
    // calls see the new one.
    backend.adopt(&new);
    tracing::info!("rotate-bootstrap: fnox-core session swapped");
    serde_json::to_string(&RotateBootstrapResponse::ok())
        .expect("RotateBootstrapResponse always serializes")
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
    // FR-017: peer_pid + peer_uid in audit events. `peer_cred()` is the
    // SO_PEERCRED lookup; it almost never fails on a connected stream, but
    // if it does we fall back to None on both fields.
    let peer = stream.peer_cred().ok();
    let peer_pid = peer.as_ref().and_then(|c| c.pid());
    let peer_uid = peer.as_ref().map(|c| c.uid());

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::with_capacity(256);

    loop {
        line.clear();
        let n = read_line_capped(&mut reader, &mut line, MAX_MESSAGE_BYTES).await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        // Measure broker-internal handling time from request bytes received
        // to response bytes about to be written. Audit `latency_ms` is the
        // time inside the broker, not end-to-end including socket flush.
        let start = Instant::now();
        let response_json = match serde_json::from_slice::<ProjectRequest>(&line) {
            Ok(req) => dispatch_project(&server, &project, req, peer_pid, peer_uid, start).await,
            // Protocol errors are not "fetch attempts" — there's no parseable
            // secret name to record — so no audit event is emitted here.
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

async fn dispatch_project(
    server: &Server,
    project: &Project,
    req: ProjectRequest,
    peer_pid: Option<i32>,
    peer_uid: Option<u32>,
    start: Instant,
) -> String {
    match req {
        ProjectRequest::Ping => {
            // ping/info are not fetch attempts → no audit event (FR-013
            // applies to `get` only).
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
            dispatch_get(server, project, name, peer_pid, peer_uid, start).await
        }
    }
}

async fn dispatch_get(
    server: &Server,
    project: &Project,
    name: String,
    peer_pid: Option<i32>,
    peer_uid: Option<u32>,
    start: Instant,
) -> String {
    let manifest = project.manifest();
    // FR-012: allowlist denial does not incur a backend round-trip and
    // does not consult the cache.
    if !manifest.allowlist.secrets.iter().any(|n| n == &name) {
        emit_fetch(
            &server.audit,
            &project.name,
            &name,
            Decision::Deny,
            Outcome::Ok,
            peer_pid,
            peer_uid,
            start,
            None,
            Some("allowlist"),
        );
        return serde_json::to_string(&ProjectError::new(
            ProjectErrorCode::Denied,
            format!("Secret {name:?} is not in this project's allowlist."),
        ))
        .expect("ProjectError always serializes");
    }

    // Cache hit short-circuits the backend (FR-014). The plaintext boundary
    // is here — `expose_secret()` is the one place per request where the
    // value materialises outside `SecretString`, and it's immediately
    // handed to `serde_json` to write to the socket.
    if let Some(hit) = project.cache.get(&name) {
        emit_fetch(
            &server.audit,
            &project.name,
            &name,
            Decision::Allow,
            Outcome::Ok,
            peer_pid,
            peer_uid,
            start,
            Some("cache"),
            None,
        );
        let resp = GetResponse::utf8(hit.value.expose_secret(), hit.ttl_seconds);
        return serde_json::to_string(&resp).expect("GetResponse always serializes");
    }

    // Cache miss — go to the backend. If no backend was constructed at
    // startup (degraded mode), return backend_error directly. Otherwise
    // call fnox-core.
    let Some(backend) = server.backend.as_ref() else {
        emit_fetch(
            &server.audit,
            &project.name,
            &name,
            Decision::Allow,
            Outcome::BackendError,
            peer_pid,
            peer_uid,
            start,
            None,
            None,
        );
        return serde_json::to_string(&ProjectError::new(
            ProjectErrorCode::BackendError,
            "no fnox-core session is configured; rerun the daemon with --fnox-config",
        ))
        .expect("ProjectError always serializes");
    };

    match backend.get(&name).await {
        Ok(Some(plaintext)) => {
            // Cache before responding so concurrent in-flight gets converge
            // on the same TTL window. The cache stores a SecretString
            // clone; the plaintext String stays around until this scope
            // ends (after the response is serialized).
            project
                .cache
                .insert(name.clone(), SecretString::from(plaintext.clone()), None);
            let ttl_seconds =
                u32::try_from(project.cache.default_ttl().as_secs()).unwrap_or(u32::MAX);
            emit_fetch(
                &server.audit,
                &project.name,
                &name,
                Decision::Allow,
                Outcome::Ok,
                peer_pid,
                peer_uid,
                start,
                Some("fnox"),
                None,
            );
            let resp = GetResponse::utf8(plaintext, ttl_seconds);
            serde_json::to_string(&resp).expect("GetResponse always serializes")
        }
        Ok(None) => {
            // FR-017 Outcome::NotFound: the backend resolved the name but
            // the value was absent (fnox `if_missing = "ignore"` / "warn").
            emit_fetch(
                &server.audit,
                &project.name,
                &name,
                Decision::Allow,
                Outcome::NotFound,
                peer_pid,
                peer_uid,
                start,
                Some("fnox"),
                None,
            );
            serde_json::to_string(&ProjectError::new(
                ProjectErrorCode::NotFound,
                format!("Secret {name:?} not found by fnox-core resolver."),
            ))
            .expect("ProjectError always serializes")
        }
        Err(msg) => {
            // fnox-core's error type doesn't distinguish "unreachable" from
            // "auth failed" from "provider misconfigured", so everything
            // surfaces as `backend_error` with the message attached for
            // operator triage.
            emit_fetch(
                &server.audit,
                &project.name,
                &name,
                Decision::Allow,
                Outcome::BackendError,
                peer_pid,
                peer_uid,
                start,
                Some("fnox"),
                None,
            );
            serde_json::to_string(&ProjectError::new(
                ProjectErrorCode::BackendError,
                format!("fnox-core: {msg}"),
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

/// Build and dispatch a `FetchEvent` audit record (FR-013, FR-017). The
/// emission is fire-and-forget: `AuditWriter::record` queues onto a bounded
/// channel and degrades to an in-memory ring buffer if the writer task can't
/// keep up — it never blocks the calling fetch.
#[allow(clippy::too_many_arguments)]
fn emit_fetch(
    audit: &AuditWriter,
    project: &str,
    secret_name: &str,
    decision: Decision,
    outcome: Outcome,
    peer_pid: Option<i32>,
    peer_uid: Option<u32>,
    start: Instant,
    backend: Option<&str>,
    reason: Option<&str>,
) {
    let latency_ms = u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
    audit.record(AuditEvent::Fetch(FetchEvent {
        timestamp: OffsetDateTime::now_utc(),
        project: project.to_string(),
        secret_name: secret_name.to_string(),
        decision,
        outcome,
        peer_pid,
        peer_uid,
        latency_ms,
        backend: backend.map(str::to_string),
        reason: reason.map(str::to_string),
    }));
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
        let server = Server::new(config, audit, None);

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
        let server = Server::new(config, audit, None);
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
    async fn rotate_bootstrap_with_no_backend_returns_bootstrap_error() {
        // backend=None (degraded mode): rotate-bootstrap can't construct a
        // new session since none was wired at startup. Returns
        // bootstrap_error with a hint pointing the operator at
        // --fnox-config.
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;

        let response = send_admin(&admin_socket, r#"{"op":"rotate-bootstrap"}"#).await;
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "bootstrap_error");
        assert!(
            v["message"].as_str().unwrap().contains("--fnox-config"),
            "expected hint about --fnox-config, got: {}",
            v["message"]
        );

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn get_with_no_backend_returns_backend_error_with_hint() {
        // Mirror of the rotate test for the data plane: when backend=None
        // a cache miss must surface as backend_error mentioning
        // --fnox-config so the operator knows how to fix it.
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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
        let resp = send_project(&project_socket, r#"{"op":"get","name":"FOO"}"#).await;
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "backend_error");
        assert!(
            v["message"].as_str().unwrap().contains("--fnox-config"),
            "expected hint about --fnox-config, got: {}",
            v["message"]
        );

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn register_lifecycle_end_to_end() {
        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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

    /// Read every JSONL line in `path` (if it exists) and return them as
    /// parsed `serde_json::Value`s. The audit writer is on its own task so
    /// callers should poll via `wait_for_audit_events`.
    fn read_audit_events(path: &Path) -> Vec<Value> {
        match std::fs::read_to_string(path) {
            Ok(s) => s
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).expect("audit line must be valid JSON"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Poll until at least `min` events appear in the audit log. Returns the
    /// final parsed list. Times out the test rather than hanging.
    async fn wait_for_audit_events(path: &Path, min: usize) -> Vec<Value> {
        for _ in 0..100 {
            let events = read_audit_events(path);
            if events.len() >= min {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "audit log never reached {min} events; have {:?}",
            read_audit_events(path)
        );
    }

    #[tokio::test]
    async fn get_emits_fetch_event_per_request() {
        use secrecy::SecretString;

        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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

        let project_socket = socket_dir.join("alpha.sock");

        // Pre-warm cache so we can exercise the cache-hit audit branch.
        let project = registry
            .snapshot()
            .await
            .into_iter()
            .find(|p| p.name == "alpha")
            .unwrap();
        project
            .cache
            .insert("FOO".into(), SecretString::from("v1".to_string()), None);

        // Three fetches: cache hit, denied, allowed-but-no-cache.
        let _ = send_project(&project_socket, r#"{"op":"get","name":"FOO"}"#).await;
        let _ = send_project(&project_socket, r#"{"op":"get","name":"BAR"}"#).await;
        project.cache.clear();
        let _ = send_project(&project_socket, r#"{"op":"get","name":"FOO"}"#).await;

        // ping + info must NOT produce audit events.
        let _ = send_project(&project_socket, r#"{"op":"ping"}"#).await;
        let _ = send_project(&project_socket, r#"{"op":"info"}"#).await;

        let events = wait_for_audit_events(&audit_log, 3).await;
        // All three are Fetch events; ping/info contribute nothing.
        assert_eq!(events.len(), 3, "expected 3 fetch events: {events:#?}");
        for e in &events {
            assert_eq!(e["event"], "fetch");
            assert_eq!(e["project"], "alpha");
            // peer_pid is the test process itself (server + client share
            // a process in this test).
            assert_eq!(
                e["peer_pid"].as_i64(),
                Some(std::process::id() as i64),
                "peer_pid wrong: {e}"
            );
            // peer_uid varies across CI runners; just confirm it's recorded.
            assert!(e["peer_uid"].is_u64(), "peer_uid missing: {e}");
            assert!(e.get("latency_ms").is_some());
            // SC-004: never a `value` field in audit events.
            assert!(e.get("value").is_none(), "audit leaked value: {e}");
        }

        // Cache hit.
        assert_eq!(events[0]["secret_name"], "FOO");
        assert_eq!(events[0]["decision"], "allow");
        assert_eq!(events[0]["outcome"], "ok");
        assert_eq!(events[0]["backend"], "cache");
        assert!(events[0].get("reason").is_none());

        // Denied.
        assert_eq!(events[1]["secret_name"], "BAR");
        assert_eq!(events[1]["decision"], "deny");
        assert_eq!(events[1]["outcome"], "ok");
        assert_eq!(events[1]["reason"], "allowlist");
        assert!(events[1].get("backend").is_none());

        // Allowed but cache miss → backend stub.
        assert_eq!(events[2]["secret_name"], "FOO");
        assert_eq!(events[2]["decision"], "allow");
        assert_eq!(events[2]["outcome"], "backend_error");
        assert!(events[2].get("backend").is_none());

        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn audit_never_contains_secret_value() {
        // Tighter SC-004 check: pre-warm a cache entry with a distinctive
        // secret string, exercise the cache-hit path, and grep the audit
        // log for the substring.
        use secrecy::SecretString;

        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, None);
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

        let secret_value = "tripwire-DO-NOT-LEAK-7f3a";
        let project = registry.snapshot().await.into_iter().next().unwrap();
        project.cache.insert(
            "FOO".into(),
            SecretString::from(secret_value.to_string()),
            None,
        );

        let project_socket = socket_dir.join("alpha.sock");
        let resp = send_project(&project_socket, r#"{"op":"get","name":"FOO"}"#).await;
        assert!(resp.contains(secret_value), "value must be in the response");

        let _events = wait_for_audit_events(&audit_log, 1).await;
        let log_contents = std::fs::read_to_string(&audit_log).unwrap();
        assert!(
            !log_contents.contains(secret_value),
            "audit log contained the secret value: {log_contents}"
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
        let server = Server::new(config, audit, None);
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

    /// End-to-end fnox-core integration test. Mirrors the
    /// `Quick start` / scenarios 0-6 of the CONTRIBUTING playbook
    /// in-process: writes a hermetic fnox.toml using fnox's `plain`
    /// provider, constructs a real `BackendSession`, registers a
    /// project, and proves `get` returns the actual secret value
    /// from fnox-core (not the degraded-mode stub).
    ///
    /// This is the "code-complete, now also CI-proven" check for
    /// FR-004/FR-005/FR-014/FR-017 end-to-end. Until this test
    /// landed, the cache-miss → backend → real value path had only
    /// been verified by hand via the playbook.
    #[tokio::test]
    async fn end_to_end_fnox_backend_resolves_a_secret() {
        use crate::backend::BackendSession;

        let dir = tempdir();
        let socket_dir = dir.path().join("run");
        let audit_log = dir.path().join("audit.log");
        let fnox_config = dir.path().join("fnox.toml");

        // fnox's `plain` provider is the only one hermetic enough
        // for CI: no network, no keychain, no external state.
        std::fs::write(
            &fnox_config,
            r#"[providers]
plain = { type = "plain" }

[secrets]
HELLO = { provider = "plain", value = "world" }
"#,
        )
        .unwrap();

        let backend =
            BackendSession::open(&fnox_config).expect("fnox-core opens the hermetic config");

        let (audit, _audit_handle) = AuditWriter::spawn(audit_log.clone());
        let server = Server::new(test_config(&socket_dir, &audit_log), audit, Some(backend));
        let server_handle = tokio::spawn(server.run());

        let admin_socket = socket_dir.join(ADMIN_SOCKET_NAME);
        wait_for_path(&admin_socket).await;

        let project_dir = write_project(dir.path(), "hello", &["HELLO"]);
        let register_req = format!(
            r#"{{"op":"register","name":"hello","project_path":"{}"}}"#,
            project_dir.display()
        );
        let resp = send_admin(&admin_socket, &register_req).await;
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true, "register failed: {resp}");

        // First get → cache miss → backend (fnox plain provider).
        let project_socket = socket_dir.join("hello.sock");
        let resp = send_project(&project_socket, r#"{"op":"get","name":"HELLO"}"#).await;
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true, "cold get failed: {resp}");
        assert_eq!(v["value"], "world", "wrong value: {resp}");

        // Second get → cache hit. Value identical; TTL non-increasing.
        let resp2 = send_project(&project_socket, r#"{"op":"get","name":"HELLO"}"#).await;
        let v2: Value = serde_json::from_str(&resp2).unwrap();
        assert_eq!(v2["ok"], true);
        assert_eq!(v2["value"], "world");
        assert!(
            v2["ttl_seconds"].as_u64().unwrap() <= v["ttl_seconds"].as_u64().unwrap(),
            "ttl should be non-increasing across a cache hit",
        );

        // Audit captured both: one backend=fnox + one backend=cache.
        let events = wait_for_audit_events(&audit_log, 2).await;
        assert_eq!(events.len(), 2, "expected 2 fetch events: {events:#?}");
        assert_eq!(events[0]["backend"], "fnox");
        assert_eq!(events[1]["backend"], "cache");
        for e in &events {
            assert_eq!(e["outcome"], "ok");
            assert_eq!(e["decision"], "allow");
            assert!(e.get("value").is_none(), "audit leaked value: {e}");
        }
        // The plaintext never reaches the audit log.
        let log = std::fs::read_to_string(&audit_log).unwrap();
        assert!(!log.contains("world"), "audit log leaked the value");

        server_handle.abort();
        let _ = server_handle.await;
    }
}
