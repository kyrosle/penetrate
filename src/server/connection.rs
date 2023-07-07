use std::{
  collections::HashMap, sync::Arc, time::Duration,
};

use anyhow::{Context, Result};
use backoff::{backoff::Backoff, ExponentialBackoff};
use tokio::{
  net::{TcpListener, TcpStream},
  sync::{broadcast, mpsc, RwLock},
  time,
};
use tracing::{debug, error, info, Instrument, Span};

use crate::{
  config::{ServerConfig, ServerServiceConfig},
  constants::listen_backoff,
  helper::retry_notify_with_deadline,
  protocol::{read_hello, Hello},
  transport::Transport,
};

use super::{
  control_channel::do_control_channel_handshake,
  data_channel::do_data_channel_handshake,
  ControlChannelMap, ServiceDigest, CHAN_SIZE,
};

/// Handle the server accepted connection
pub(super) async fn handle_connection<
  T: 'static + Transport,
>(
  mut conn: T::Stream,
  services: Arc<
    RwLock<HashMap<ServiceDigest, ServerServiceConfig>>,
  >,
  control_channels: Arc<RwLock<ControlChannelMap<T>>>,
  server_config: Arc<ServerConfig>,
) -> Result<()> {
  // Read Hello from client, to judge whether is control channel or
  // data channel handshake
  let hello = read_hello(&mut conn).await?;

  match hello {
    Hello::ControlChannelHello(_, service_digest) => {
      do_control_channel_handshake(
        conn,
        services,
        control_channels,
        service_digest,
        server_config,
      )
      .await?;
    }
    Hello::DataChannelHello(_, nonce) => {
      do_data_channel_handshake(
        conn,
        control_channels,
        nonce,
      )
      .await?;
    }
  }
  Ok(())
}

/// Startup a tcp listener, return a receiver of channel to fetch the
/// accepted tcp stream
pub(super) fn tcp_listen_and_send(
  addr: String,
  data_ch_req_tx: mpsc::UnboundedSender<bool>,
  mut shutdown_rx: broadcast::Receiver<bool>,
) -> mpsc::Receiver<TcpStream> {
  let (tx, rx) = mpsc::channel(CHAN_SIZE);

  tokio::spawn(
    async move {
      // tcp bind address
      let l = retry_notify_with_deadline(
        listen_backoff(),
        || async { Ok(TcpListener::bind(&addr).await?) }, /* tcp listener bind the address */
        |e, duration| {
          error!("{:#}. Retry in {:?}", e, duration);
        },
        &mut shutdown_rx,
      )
      .await
      .with_context(|| "Failed to listen for the service");

      let l = match l {
        Ok(v) => v,
        Err(e) => {
          error!("{:#}", e);
          return;
        }
      };

      info!("Listening at {}", &addr);

      // Retry at least every 1s
      let mut backoff = ExponentialBackoff {
        max_elapsed_time: None,
        max_interval: Duration::from_secs(1),
        ..Default::default()
      };

      // Wait for visitors and the shutdown signal
      loop {
        tokio::select! {
          // tcp listener accept connection
          val = l.accept() => {
            match val {
              Err(e) => {
                // `l` is a TCP listener so this must be
                // a IO error possibly a EMFILE. So sleep for a while
                error!("{}. Sleep for a while", e);
                if let Some(d) = backoff.next_backoff() {
                  time::sleep(d).await;
                } else {
                  // This branch will never be reached for current
                  // backoff policy
                  error!("Too many retires. Aborting...");
                  break;
                }
              }
              Ok((incoming, addr)) => {
                // For every visitor, request to create a data channel
                if data_ch_req_tx.send(true)
                    .with_context(|| "Failed to send data chan create request")
                    .is_err() {
                  // An error indicates the control channel is broken
                  // so break the loop
                  break;
                }

                backoff.reset();

                debug!("New visitor from {}", addr);

                // Send the visitor to the connection pool
                let _ = tx.send(incoming).await;
              }
            }
          }
          _ = shutdown_rx.recv() => {
            break;
          }
        }
      }

      info!("TCPListener shutdown");
    }
    .instrument(Span::current()),
  );

  rx
}
