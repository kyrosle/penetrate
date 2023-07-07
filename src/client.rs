//! Client implementation
//! calling `run_client()` accepting a config, receiver of shutdown channel,
//! receiver of update channel.
use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, Context, Result};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, warn};

use crate::{
  config::{ClientConfig, Config, TransportType},
  config_watcher::{ClientServiceChange, ConfigChange},
  protocol,
  transport::{
    NoiseTransport, TcpTransport, TlsTransport, Transport,
  },
};

use self::control_channel::ControlChannelHandle;

mod control_channel;
mod data_channel;
mod traffic;

/// The entrypoint of running a client
pub async fn run_client(
  config: Config,
  shutdown_rx: broadcast::Receiver<bool>,
  update_rx: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
  // extract the client configuration
  let config = config.client.ok_or_else(|| {
    anyhow!(
      "Try to run as a client, but the configuration is missing.
      Please add the `[client]` block"
    )
  })?;

  // handle the client transport type
  match config.transport.transport_type {
    TransportType::Tcp => {
      let mut client =
        Client::<TcpTransport>::from(config).await?;
      client.run(shutdown_rx, update_rx).await
    }
    TransportType::Tls => {
      let mut client =
        Client::<TlsTransport>::from(config).await?;
      client.run(shutdown_rx, update_rx).await
    }
    TransportType::Noise => {
      let mut client =
        Client::<NoiseTransport>::from(config).await?;
      client.run(shutdown_rx, update_rx).await
    }
  }
}

/// service name digest
type ServiceDigest = protocol::Digest;
/// temporary number for negotiate
type Nonce = protocol::Digest;

/// # Client
/// corresponding transport configuration will set in `config` field.
struct Client<T: Transport> {
  /// client configuration
  config: ClientConfig,
  /// service collection
  service_handles: HashMap<String, ControlChannelHandle>,
  /// transport entity, specify `T` from tcp, tls, noise,
  transport: Arc<T>,
}

impl<T: 'static + Transport> Client<T> {
  async fn from(config: ClientConfig) -> Result<Client<T>> {
    let transport =
      Arc::new(T::new(&config.transport).with_context(
        || "Failed to create the transport",
      )?);
    Ok(Client {
      config,
      service_handles: HashMap::new(),
      transport,
    })
  }

  async fn run(
    &mut self,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut update_rx: mpsc::Receiver<ConfigChange>,
  ) -> Result<()> {
    for (name, config) in &self.config.services {
      // debug!(
      //   "create control channel handle with config: {:#?}",
      //   config
      // );
      let handle = ControlChannelHandle::new(
        (*config).clone(),
        self.config.remote_addr.clone(),
        self.transport.clone(),
        self.config.heartbeat_timeout,
      );
      self.service_handles.insert(name.clone(), handle);
    }

    // Wait for the shutdown signal
    loop {
      tokio::select! {
        // shutdown signal
        val = shutdown_rx.recv() => {
          match val {
            Ok(_) => {}
            Err(err) => {
              error!("Unable to listen for shutdown signal: {}", err);
            }
          }
          break;
        }
        // configuration update event received
        e = update_rx.recv() => {
          if let Some(e) = e {
            self.handle_hot_reload(e).await;
          }
        }
      }
    }

    // Shutdown all services, gracefully shutdown.
    for (_, handle) in self.service_handles.drain() {
      handle.shutdown();
    }

    Ok(())
  }

  /// handle `ConfigChange` from update channel
  async fn handle_hot_reload(&mut self, e: ConfigChange) {
    match e {
      ConfigChange::ClientChange(client_change) => {
        // only handle client configuration changes
        match client_change {
          ClientServiceChange::Add(cfg) => {
            let name = cfg.name.clone();
            debug!(
              "control channel handle hot reload with config: {:#?}",
              cfg
            );
            let handle = ControlChannelHandle::new(
              cfg,
              self.config.remote_addr.clone(),
              self.transport.clone(),
              self.config.heartbeat_timeout,
            );
            let _ =
              self.service_handles.insert(name, handle);
          }
          ClientServiceChange::Delete(s) => {
            let _ = self.service_handles.remove(&s);
          }
        }
      }
      ignored => warn!(
        "Ignored {:?} since running as a client",
        ignored
      ),
    }
  }
}
