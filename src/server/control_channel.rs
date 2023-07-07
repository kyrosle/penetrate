use std::{
  collections::HashMap, sync::Arc, time::Duration,
};

use anyhow::{bail, Context, Result};
use rand::RngCore;
use tokio::{
  io::AsyncWriteExt,
  sync::{broadcast, mpsc, RwLock},
  time,
};
use tracing::{
  debug, error, info, instrument, warn, Instrument, Span,
};

use crate::{
  config::{
    ServerConfig, ServerServiceConfig, ServiceType,
  },
  protocol::{
    self, read_auth, Ack, ControlChannelCmd, Hello,
    HASH_WIDTH_IN_BYTES,
  },
  transport::{SocketOpts, Transport},
};

use super::{
  pool::{
    run_tcp_connection_pool, run_udp_connection_pool,
  },
  ControlChannelMap, ServiceDigest, CHAN_SIZE,
  TCP_POOL_SIZE, UDP_POOL_SIZE,
};

/// Control channel, using T as the transport layer.
struct ControlChannel<T: Transport> {
  /// The connection of control channel
  conn: T::Stream,
  /// Receives the shutdown signal
  shutdown_rx: broadcast::Receiver<bool>,
  /// Receives visitor connections
  data_ch_req_rx: mpsc::UnboundedReceiver<bool>,
  /// Application-layer heartbeat interval in seconds
  heartbeat_interval: u64,
}

impl<T: Transport> ControlChannel<T> {
  async fn write_and_flush(
    &mut self,
    data: &[u8],
  ) -> Result<()> {
    self
      .conn
      .write_all(data)
      .await
      .with_context(|| "Failed to write control command")?;
    self
      .conn
      .flush()
      .await
      .with_context(|| "Failed to flush control command")?;
    Ok(())
  }

  #[instrument(skip_all)]
  /// Run a control channel
  async fn run(mut self) -> Result<()> {
    let create_ch_cmd = bincode::serialize(
      &ControlChannelCmd::CreateDataChannel,
    )
    .unwrap();

    let heartbeat =
      bincode::serialize(&ControlChannelCmd::HeartBeat)
        .unwrap();

    // Wait for data channel requests and the shutdown signal
    loop {
      tokio::select! {
        val = self.data_ch_req_rx.recv() => {
          match val {
            Some(_) => {
              if let Err(e) = self.write_and_flush(&create_ch_cmd).await {
                error!("{:#}", e);
                break;
              }
            }
            None => {
              break;
            }
          }
        }
        _ = time::sleep(Duration::from_secs(self.heartbeat_interval)),
                                  if self.heartbeat_interval != 0 => {
          if let Err(e) = self.write_and_flush(&heartbeat).await {
            error!("{:#}", e);
            break;
          }
        }
        _ = self.shutdown_rx.recv() => {
          break;
        }
      }
    }

    info!("Control channel shutdown");

    Ok(())
  }
}

pub struct ControlChannelHandle<T: Transport> {
  /// Shutdown the control channel by dropping it
  _shutdown_tx: broadcast::Sender<bool>,
  pub(super) data_ch_tx: mpsc::Sender<T::Stream>,
  pub(super) service: ServerServiceConfig,
}

impl<T> ControlChannelHandle<T>
where
  T: 'static + Transport,
{
  #[instrument(name = "handle", skip_all, fields(service = %service.name))]
  /// Create a control channel handle, where the control channel handling
  /// task and the connection pool task are created.
  fn new(
    conn: T::Stream,
    service: ServerServiceConfig,
    heartbeat_interval: u64,
  ) -> ControlChannelHandle<T> {
    // Create a shutdown channel
    let (
      shutdown_tx, /* offer to ControlChannelHandle, controlling shutdown the channel  */
      shutdown_rx,
    ) = broadcast::channel::<bool>(1);

    // Store data channels
    let (data_ch_tx, data_ch_rx) =
      mpsc::channel(CHAN_SIZE * 2);

    // Store data channel creation requests
    let (data_ch_req_tx, data_ch_req_rx) =
      mpsc::unbounded_channel();

    let pool_size = match service.service_type {
      ServiceType::Tcp => TCP_POOL_SIZE,
      ServiceType::Udp => UDP_POOL_SIZE,
    };

    for _ in 0..pool_size {
      if let Err(e) = data_ch_req_tx.send(true) {
        error!("Failed to request data channel {}", e);
      }
    }

    let shutdown_rx_clone = shutdown_tx.subscribe();
    let bind_addr = service.bind_addr.clone();

    // spawn connection pool
    match service.service_type {
      ServiceType::Tcp => tokio::spawn(
        async move {
          if let Err(e) = run_tcp_connection_pool::<T>(
            bind_addr,
            data_ch_rx,
            data_ch_req_tx,
            shutdown_rx_clone,
          )
          .await
          .with_context(|| {
            "Failed to run TCP connection pool"
          }) {
            error!("{:#}", e);
          }
        }
        .instrument(Span::current()),
      ),
      ServiceType::Udp => tokio::spawn(
        async move {
          if let Err(e) = run_udp_connection_pool::<T>(
            bind_addr,
            data_ch_rx,
            data_ch_req_tx,
            shutdown_rx_clone,
          )
          .await
          .with_context(|| {
            "Failed to run UDP connection pool"
          }) {
            error!("{:#}", e);
          }
        }
        .instrument(Span::current()),
      ),
    };

    // spawn control channel
    // handle the request produced by data channel
    let ch = ControlChannel::<T> {
      conn,
      shutdown_rx,
      data_ch_req_rx,
      heartbeat_interval,
    };
    tokio::spawn(
      async move {
        if let Err(err) = ch.run().await {
          error!("{:#}", err);
        }
      }
      .instrument(Span::current()),
    );

    ControlChannelHandle {
      _shutdown_tx: shutdown_tx,
      data_ch_tx,
      service,
    }
  }
}

pub(super) async fn do_control_channel_handshake<
  T: 'static + Transport,
>(
  mut conn: T::Stream,
  services: Arc<
    RwLock<HashMap<ServiceDigest, ServerServiceConfig>>,
  >,
  control_channels: Arc<RwLock<ControlChannelMap<T>>>,
  service_digest: ServiceDigest,
  server_config: Arc<ServerConfig>,
) -> Result<()> {
  info!("Try to handshake a control channel");

  T::hint(&conn, SocketOpts::for_control_channel());

  // Generate a nonce
  let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
  rand::thread_rng().fill_bytes(&mut nonce);

  // Send hello to server with a nonce(number use once)
  let hello_send = Hello::ControlChannelHello(
    protocol::CURRENT_PROTO_VERSION,
    nonce.clone().try_into().unwrap(),
  );
  conn
    .write_all(&bincode::serialize(&hello_send).unwrap())
    .await?;
  conn.flush().await?;

  // Lookup the service
  let service_config =
    match services.read().await.get(&service_digest) {
      Some(v) => v,
      None => {
        conn
          .write_all(
            &bincode::serialize(&Ack::ServiceNotExist)
              .unwrap(),
          )
          .await?;
        bail!(
          "No such a service {}",
          hex::encode(service_digest)
        );
      }
    }
    .to_owned();

  let service_name = &service_config.name;

  // Calculate the checksum
  let mut concat = Vec::from(
    service_config.token.as_ref().unwrap().as_bytes(),
  );
  concat.append(&mut nonce);

  // Read Auth
  let protocol::Auth(d) = read_auth(&mut conn).await?;

  // Validate
  let session_key = protocol::digest(&concat);
  if session_key != d {
    conn
      .write_all(
        &bincode::serialize(&Ack::AuthFailed).unwrap(),
      )
      .await?;
    debug!(
      "Expect {}, but got {}",
      hex::encode(session_key),
      hex::encode(d)
    );
    bail!(
      "Service {} failed the authentication",
      service_name
    );
  } else {
    // the auth check is ok
    let mut wg = control_channels.write().await;

    if wg.remove1(&service_digest).is_some() {
      warn!(
        "Dropping previous control channel for service {}",
        service_name
      );
    }

    // Send ack
    conn
      .write_all(&bincode::serialize(&Ack::Ok).unwrap())
      .await?;
    conn.flush().await?;

    info!(service = %service_config.name, "Control channel established");
    let handle = ControlChannelHandle::new(
      conn,
      service_config,
      server_config.heartbeat_interval,
    );

    // Insert the new handle
    let _ = wg.insert(service_digest, session_key, handle);
  }

  Ok(())
}
