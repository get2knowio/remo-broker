use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "remo-broker",
    version,
    about = "On-instance credential broker daemon for Remo",
    long_about = None,
)]
struct Cli {
    #[arg(long, value_name = "PATH", env = "REMO_BROKER_CONFIG")]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "remo-broker starting (skeleton — no functionality wired yet)"
    );

    Ok(())
}
