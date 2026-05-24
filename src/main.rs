use std::path::PathBuf;

use clap::Parser;
use remo_broker::config::{BootstrapSource, BootstrapSourceKind, Config, Overrides};

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

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        socket_dir = %config.socket_dir.display(),
        audit_log_path = %config.audit_log_path.display(),
        cache_default_ttl_secs = config.cache_default_ttl.as_secs(),
        "remo-broker starting (skeleton — sockets not yet wired)"
    );

    Ok(())
}
