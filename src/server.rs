use std::{
  collections::HashMap, sync::Arc, time::Duration,
};

use anyhow::{anyhow, Context, Result};
use backoff::{backoff::Backoff, ExponentialBackoff};
use tokio::{
  io,
  sync::{broadcast, mpsc, RwLock},
  time,
};
use tracing::{error, info, info_span, warn, Instrument};

use crate::{
  config::{
    Config, ServerConfig, ServerServiceConfig,
    TransportType,
  },
  config_watcher::{ConfigChange, ServerServiceChange},
  multi_map::MultiMap,
  protocol,
  server::connection::handle_connection,
  transport::{
    NoiseTransport, TcpTransport, TlsTransport, Transport,
  },
};

use self::control_channel::ControlChannelHandle;

mod connection;
mod control_channel;
mod data_channel;
mod pool;

type ServiceDigest = protocol::Digest; // SHA256 of a service name
type Nonce = protocol::Digest; // Also called `session_key`

const TCP_POOL_SIZE: usize = 8; // The number of cached connections for TCP services
const UDP_POOL_SIZE: usize = 2; // The number of cached connections for UDP services
const CHAN_SIZE: usize = 2048; // The capacity of various channels
const HANDSHAKE_TIMEOUT: u64 = 5; // Timeout for transport handshake

/// The entrypoint to running a server
pub async fn run_server(
  config: Config,
  shutdown_rx: broadcast::Receiver<bool>,
  update_rx: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
  let config = match config.server {
    Some(config) => config,
    None => {
      return Err(anyhow!("Try to run as a server, but the configuration is missing.Please add the `[server]` block"))
    }
  };

  match config.transport.transport_type {
    TransportType::Tcp => {
      let mut server =
        Server::<TcpTransport>::from(config)?;
      server.run(shutdown_rx, update_rx).await?;
    }
    TransportType::Tls => {
      let mut server =
        Server::<TlsTransport>::from(config)?;
      server.run(shutdown_rx, update_rx).await?
    }
    TransportType::Noise => {
      let mut server =
        Server::<NoiseTransport>::from(config)?;
      server.run(shutdown_rx, update_rx).await?
    }
  }

  Ok(())
}

/// A hash map of ControlChannelHandles, indexed by ServiceDigest or Nonce.
type ControlChannelMap<T> =
  MultiMap<ServiceDigest, Nonce, ControlChannelHandle<T>>;

/// Server holds all states of running a server
struct Server<T: Transport> {
  /// `[server]` config
  config: Arc<ServerConfig>,
  /// `[server.service]` config, indexed by ServiceDigest
  services: Arc<
    RwLock<HashMap<ServiceDigest, ServerServiceConfig>>,
  >,
  /// Collection of control channels
  control_channels: Arc<RwLock<ControlChannelMap<T>>>,
  /// Wrapper around the transport layer
  transport: Arc<T>,
}

impl<T: 'static + Transport> Server<T> {
  /// Create a serer form `[server]`
  pub fn from(config: ServerConfig) -> Result<Self> {
    let config = Arc::new(config);
    let services = Arc::new(RwLock::new(
      generate_service_hashmap(&config),
    ));
    let control_channels =
      Arc::new(RwLock::new(ControlChannelMap::new()));
    let transport = Arc::new(T::new(&config.transport)?);
    Ok(Server {
      config,
      services,
      control_channels,
      transport,
    })
  }

  /// The entry point of Server
  pub async fn run(
    &mut self,
    mut shutdown_rx: broadcast::Receiver<bool>,
    mut update_rx: mpsc::Receiver<ConfigChange>,
  ) -> Result<()> {
    // server transport bind the `bind-address`
    let l = self
      .transport
      .bind(&self.config.bind_addr)
      .await
      .with_context(|| {
        "Failed to listen at `server.bind_addr`"
      })?;

    info!("Listening at {}", self.config.bind_addr);

    // Retry at least every 100ms
    let mut backoff = ExponentialBackoff {
      max_elapsed_time: None,
      max_interval: Duration::from_millis(100),
      ..Default::default()
    };

    // Wait for connections and shutdown signals
    loop {
      tokio::select! {
        // transport accept new connection
        ret = self.transport.accept(&l) => {
          match ret {
            Err(err) => {
              if let Some(err) = err.downcast_ref::<io::Error> (){
                // If it is an tokio::io error, then it's possibly an
                // EMFILE. So sleep for a while and retry.
                // Only sleep for
                // - EMFILE: Too many open files in process
                // - ENFILE: Too many open files in system
                // - ENOMEM: Out of memory
                // - ENOBUFS: No buffer space available
                if let Some(d) = backoff.next_backoff() {
                  error!("Failed to accept: {:#}. Retry in {:?}...", err, d);
                  time::sleep(d).await;
                } else {
                  // This branch will never be executed according
                  // to the current retry policy
                  error!("Too many retries. Aborting...");
                  break;
                }
              }
              // If it's not an IO error, then it comes from
              // the transport layer, so just ignore it
            }
            Ok((conn, addr)) => {
              backoff.reset();
              // Do transport handshake with a timeout
              match time::timeout(
                Duration::from_secs(HANDSHAKE_TIMEOUT),
                self.transport.handshake(conn)
              ).await {
                Ok(conn) => {
                  match conn.with_context(|| "Failed to do transport handshake ") {
                    Ok(conn) => {
                      let services = self.services.clone();
                      let control_channels = self.control_channels.clone();
                      let server_config = self.config.clone();

                      // spawn a new thread to handle the connection
                      tokio::spawn(async move {
                        if let Err(err) =
                          handle_connection(
                            conn,
                            services,
                            control_channels,
                            server_config).await {
                          error!("{:#}", err);
                        }
                      }.instrument(info_span!("connection", %addr)));
                    },
                    Err(e) => {
                      error!("{:#}", e);
                    },
                  }
                },
                Err(e) => {
                  error!("Transport handshake timeout: {:#}", e);
                },
              }
            }
          }
        }
        _ = shutdown_rx.recv() => {
          info!("Shuting down graceful...");
          break;
        }
        e = update_rx.recv() => {
          if let Some(e) = e {
            self.handle_hot_reload(e).await;
          }
        }
      }
    }
    info!("Shutdown");

    Ok(())
  }

  async fn handle_hot_reload(&mut self, e: ConfigChange) {
    match e {
      ConfigChange::ServerChange(server_change) => {
        match server_change {
          ServerServiceChange::Add(cfg) => {
            let hash =
              protocol::digest(cfg.name.as_bytes());
            let mut wg = self.services.write().await;
            let _ = wg.insert(hash, cfg);
            let mut wg =
              self.control_channels.write().await;
            let _ = wg.remove1(&hash);
          }
          ServerServiceChange::Delete(s) => {
            let hash = protocol::digest(s.as_bytes());
            let _ =
              self.services.write().await.remove(&hash);

            let mut wg =
              self.control_channels.write().await;
            let _ = wg.remove1(&hash);
          }
        }
      }
      ignored => warn!(
        "Ignored {:?} since running as a server",
        ignored
      ),
    }
  }
}

/// Generate a hash map of services which is indexed by ServiceDigest
fn generate_service_hashmap(
  server_config: &ServerConfig,
) -> HashMap<ServiceDigest, ServerServiceConfig> {
  let mut ret = HashMap::new();
  for u in &server_config.services {
    ret.insert(
      protocol::digest(u.0.as_bytes()),
      (*u.1).clone(),
    );
  }
  ret
}
