use std::path::PathBuf;

use clap::Parser;
use remo_broker::audit::{AuditWriter, WriterShutdown};
use remo_broker::backend::BackendSession;
use remo_broker::bootstrap::fetch_token;
use remo_broker::config::{BootstrapSource, BootstrapSourceKind, Config, Overrides};
use remo_broker::server::Server;

#[derive(Debug, Parser)]
#[command(
    name = "remo-broker",
    version,
    about = "On-instance credential broker daemon for Remo",
    long_about = None,
)]
struct Cli {
    /// Path to the daemon's TOML config file. If omitted, falls back to
    /// /etc/remo-broker/config.toml (and silently to built-in defaults if
    /// that path is missing).
    #[arg(long, value_name = "PATH", env = "REMO_BROKER_CONFIG")]
    config: Option<PathBuf>,

    /// Override the bootstrap source.
    #[arg(long, value_enum, value_name = "SOURCE")]
    bootstrap_source: Option<BootstrapSourceKind>,

    /// Override the file path read when bootstrap_source = "file".
    #[arg(long, value_name = "PATH")]
    bootstrap_token_path: Option<PathBuf>,

    /// Override the directory under which broker sockets are created.
    #[arg(long, value_name = "PATH")]
    socket_dir: Option<PathBuf>,

    /// Override the audit-log file path.
    #[arg(long, value_name = "PATH")]
    audit_log_path: Option<PathBuf>,

    /// Override the default cache TTL (seconds, 1..=86400).
    #[arg(long, value_name = "SECONDS")]
    cache_default_ttl_seconds: Option<u32>,

    /// Override the default cache max entries (1..=1024).
    #[arg(long, value_name = "N")]
    cache_default_max_entries: Option<u32>,

    /// Override the backend fetch timeout in milliseconds.
    #[arg(long, value_name = "MS")]
    backend_fetch_timeout_ms: Option<u32>,

    /// Path to a `fnox.toml` for the backend session. If omitted, the broker
    /// uses `Fnox::discover()` (walks upward from cwd, merges config chain).
    #[arg(long, value_name = "PATH")]
    fnox_config_path: Option<PathBuf>,
}

impl Cli {
    fn into_overrides(self) -> (Option<PathBuf>, Overrides) {
        let Cli {
            config,
            bootstrap_source,
            bootstrap_token_path,
            socket_dir,
            audit_log_path,
            cache_default_ttl_seconds,
            cache_default_max_entries,
            backend_fetch_timeout_ms,
            fnox_config_path,
        } = self;
        (
            config,
            Overrides {
                bootstrap_source,
                bootstrap_token_path,
                socket_dir,
                audit_log_path,
                cache_default_ttl_seconds,
                cache_default_max_entries,
                backend_fetch_timeout_ms,
                fnox_config_path,
            },
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let (config_path, overrides) = Cli::parse().into_overrides();
    let config = Config::load(config_path.as_deref(), &overrides)?;

    if matches!(config.bootstrap, BootstrapSource::Env) {
        // FR-002c: env bootstrap is development/testing only.
        tracing::warn!(
            env_var = remo_broker::config::BOOTSTRAP_ENV_VAR,
            "bootstrap_source = env is intended for development/testing only \
             — production deployments should use file or imds"
        );
    }

    // FR-003: refuse to start if no bootstrap source yields a usable token.
    // We deliberately discard the token after this validation — the daemon
    // proper will re-fetch when it constructs the fnox-core session.
    let _token = fetch_token(&config.bootstrap).await.map_err(|e| {
        eprintln!("error: bootstrap token unavailable: {e}");
        anyhow::Error::new(e).context("bootstrap source did not yield a usable token")
    })?;

    let (audit, audit_handle) = AuditWriter::spawn(config.audit_log_path.clone());

    // FR-004/FR-005: construct the fnox-core session once. We tolerate
    // failure here so the daemon stays useful for admin / ping / info /
    // cache-hit traffic even if fnox config is broken — but loudly warn
    // so the operator knows `get` will return `backend_error` until they
    // fix it. An explicit `--fnox-config /typo` is the exception: if the
    // operator named a file, missing or unreadable is a hard error.
    let backend = match &config.fnox_config_path {
        Some(path) => Some(BackendSession::open(path).map_err(|e| {
            eprintln!("error: backend session: {e}");
            anyhow::Error::new(e).context("fnox-core session could not be opened")
        })?),
        None => match BackendSession::discover() {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "fnox-core session could not be discovered; `get` will return backend_error until --fnox-config is provided or fnox.toml is reachable"
                );
                None
            }
        },
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        socket_dir = %config.socket_dir.display(),
        audit_log_path = %config.audit_log_path.display(),
        cache_default_ttl_secs = config.cache_default_ttl.as_secs(),
        bootstrap_ok = true,
        backend_ready = backend.is_some(),
        "remo-broker starting"
    );

    Server::new(config, audit, backend).run().await?;

    // Wait for the audit writer to drain after Server::run dropped its handle.
    let WriterShutdown {
        events_written,
        degraded_buffer_remaining,
    } = audit_handle.await.unwrap_or_default();
    tracing::info!(
        events_written,
        degraded_buffer_remaining,
        "audit writer exited"
    );

    Ok(())
}
