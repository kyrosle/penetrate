use anyhow::Result;
use clap::Parser;
use penetrate::cli::Cli;
use tokio::{signal, sync::broadcast};
use tracing::debug;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
  let args = Cli::parse();
  debug!("{:#?}", args);

  let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

  tokio::spawn(async move {
    if let Err(e) = signal::ctrl_c().await {
      panic!(
        "Failed to listen for the ctrl-c signal: {:?}",
        e
      );
    }

    if let Err(e) = shutdown_tx.send(true) {
      // shutdown signal must caught and handle properly
      // `rx` must not be dropped
      panic!("Failed to send shutdown signal: {:?}", e);
    }
  });

  let is_atty = atty::is(atty::Stream::Stdout);
  let level = "info";
  tracing_subscriber::fmt()
    .with_env_filter(
      EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::from(level)),
    )
    .with_ansi(is_atty)
    .init();
  penetrate::run(args, shutdown_rx).await
}
