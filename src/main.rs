use anyhow::Result;
use clap::Parser;
use penetrate::cli::Cli;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> Result<()> {
  let args = Cli::parse();
  println!("{:#?}", args);
  let (_tx, rx) = broadcast::channel(1);
  penetrate::run(args, rx).await?;
  Ok(())
}
