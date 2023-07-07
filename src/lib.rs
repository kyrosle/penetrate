use anyhow::Result;
use client::run_client;
use config::Config;
use config_watcher::{ConfigChange, ConfigWatcherHandle};
use server::run_server;
use tokio::sync::{broadcast, mpsc};

pub use cli::{Cli, KeyPairType};
use tracing::info;

pub mod config;
pub mod config_watcher;
pub mod protocol;
pub mod transport;

pub mod client;
pub mod server;

pub mod constants;
pub mod helper;
pub mod multi_map;

pub mod cli;

const DEFAULT_CURVE: KeyPairType = KeyPairType::X22519;

fn get_str_from_keypair_type(
  curve: KeyPairType,
) -> &'static str {
  match curve {
    KeyPairType::X22519 => "25519",
    KeyPairType::X448 => "448",
  }
}

fn genkey(curve: Option<KeyPairType>) -> Result<()> {
  let curve = curve.unwrap_or(DEFAULT_CURVE);
  let builder = snowstorm::Builder::new(
    format!(
      "Noise_KK_{}_ChaChaPoly_BLAKE2s",
      get_str_from_keypair_type(curve)
    )
    .parse()?,
  );

  let keypair = builder.generate_keypair()?;

  println!(
    "KeyPair type: x{}\n",
    get_str_from_keypair_type(curve)
  );

  use base64::{engine::general_purpose, Engine as _};
  println!(
    "Private Key: \n{}\n",
    general_purpose::STANDARD
      .encode(keypair.private)
  );
  println!(
    "Public Key: \n{}\n",
    general_purpose::STANDARD.encode(keypair.public)
  );
  Ok(())
}

pub async fn run(
  args: Cli,
  shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
  if args.genkey.is_some() {
    return genkey(args.genkey.unwrap());
  }

  // Spawn a config watcher. The watcher will send a initial signal
  // to start the instance with a config
  let config_path = args.config_path.as_ref().unwrap();
  let mut cfg_watcher =
    ConfigWatcherHandle::new(config_path, shutdown_rx)
      .await?;

  // shutdown_tx owns the instance
  let (shutdown_tx, _) = broadcast::channel(1);

  // (The join handle of the last instance,
  // the service update channel sender)
  let mut last_instance: Option<(
    tokio::task::JoinHandle<()>,
    mpsc::Sender<ConfigChange>,
  )> = None;

  while let Some(e) = cfg_watcher.event_rx.recv().await {
    match e {
      ConfigChange::General(config) => {
        if let Some((i, _)) = last_instance {
          info!("General configuration change detected. Restarting...");
          shutdown_tx.send(true)?;
          i.await?;
        }

        // debug!("{:?}", config);

        let (service_update_tx, service_update_rx) =
          mpsc::channel(1024);

        last_instance = Some((
          tokio::spawn(run_instance(
            *config,
            args.clone(),
            shutdown_tx.subscribe(),
            service_update_rx,
          )),
          service_update_tx,
        ));
      }
      ev => {
        info!("Service change detected. {:?}", ev);
        if let Some((_, service_update_tx)) = &last_instance
        {
          let _ = service_update_tx.send(ev).await;
        }
      }
    }
  }

  Ok(())
}

async fn run_instance(
  config: Config,
  args: Cli,
  shutdown_rx: broadcast::Receiver<bool>,
  service_update: mpsc::Receiver<ConfigChange>,
) {
  // info!("run instance config: {:#?}", config);
  let ret: Result<()> =
    match determine_run_mode(&config, &args) {
      RunMode::Server => {
        info!("run server");
        run_server(config, shutdown_rx, service_update)
          .await
      }
      RunMode::Client => {
        info!("run client");
        run_client(config, shutdown_rx, service_update)
          .await
      }
      RunMode::Undetermined => panic!(
        "Cannot determine running as server or a client"
      ),
    };
  ret.unwrap();
}

#[derive(PartialEq, Eq, Debug)]
enum RunMode {
  Server,
  Client,
  Undetermined,
}

fn determine_run_mode(
  config: &Config,
  args: &Cli,
) -> RunMode {
  if args.client && args.server {
    RunMode::Undetermined
  } else if args.client {
    RunMode::Client
  } else if args.server {
    RunMode::Server
  } else if config.client.is_some()
    && config.server.is_none()
  {
    RunMode::Client
  } else if config.client.is_none()
    && config.server.is_some()
  {
    RunMode::Server
  } else {
    RunMode::Undetermined
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use pretty_assertions::assert_eq;

  #[test]
  fn test_determine_run_mode() {
    use config::*;

    struct T {
      cfg_s: bool,
      cfg_c: bool,
      arg_s: bool,
      arg_c: bool,
      run_mode: RunMode,
    }

    let tests = [
      T {
        cfg_s: false,
        cfg_c: false,
        arg_s: false,
        arg_c: false,
        run_mode: RunMode::Undetermined,
      },
      T {
        cfg_s: true,
        cfg_c: false,
        arg_s: false,
        arg_c: false,
        run_mode: RunMode::Server,
      },
      T {
        cfg_s: false,
        cfg_c: true,
        arg_s: false,
        arg_c: false,
        run_mode: RunMode::Client,
      },
      T {
        cfg_s: true,
        cfg_c: true,
        arg_s: false,
        arg_c: false,
        run_mode: RunMode::Undetermined,
      },
      T {
        cfg_s: true,
        cfg_c: true,
        arg_s: true,
        arg_c: false,
        run_mode: RunMode::Server,
      },
      T {
        cfg_s: true,
        cfg_c: true,
        arg_s: false,
        arg_c: true,
        run_mode: RunMode::Client,
      },
      T {
        cfg_s: true,
        cfg_c: true,
        arg_s: true,
        arg_c: true,
        run_mode: RunMode::Undetermined,
      },
    ];

    for t in tests {
      let config = Config {
        server: match t.cfg_s {
          true => Some(ServerConfig::default()),
          false => None,
        },
        client: match t.cfg_c {
          true => Some(ClientConfig::default()),
          false => None,
        },
      };

      let args = Cli {
        config_path: Some(std::path::PathBuf::new()),
        server: t.arg_s,
        client: t.arg_c,
        ..Default::default()
      };

      assert_eq!(
        determine_run_mode(&config, &args),
        t.run_mode
      );
    }
  }
}
