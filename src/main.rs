pub mod bot;
pub mod config;
pub mod database;
pub mod error;
pub mod music_api;
pub mod utils;

use anyhow::Result;
use clap::Parser;
use config::Config;
use tracing::info;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Configuration file path
    #[arg(short, long, default_value = "config.ini")]
    config: String,

    /// Disable update checks
    #[arg(long)]
    no_update: bool,

    /// Disable MD5 verification
    #[arg(long)]
    no_md5_check: bool,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup logging
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&args.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = FmtSubscriber::builder()
        .with_env_filter(filter)
        .with_target(false)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    info!("Music163bot-Rust starting...");

    // Load configuration
    let config = Config::load(&args.config)?;
    info!("Configuration loaded from {}", args.config);

    // Start the bot
    bot::run(config).await?;

    Ok(())
}
