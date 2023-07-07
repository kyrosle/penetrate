use anyhow::{anyhow, Context, Result};
use tokio::{
  io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt},
  net::UdpSocket,
  sync::{broadcast, mpsc},
};
use tracing::{debug, info, instrument, warn};

use crate::{
  constants::{listen_backoff, UDP_BUFFER_SIZE},
  helper::retry_notify_with_deadline,
  protocol::{DataChannelCmd, UdpTraffic},
  transport::Transport,
};

use super::connection::tcp_listen_and_send;

#[instrument(skip_all)]
pub(super) async fn run_tcp_connection_pool<
  T: Transport,
>(
  bind_addr: String,
  mut data_ch_rx: mpsc::Receiver<T::Stream>, /* chanel to receive data */
  data_ch_req_tx: mpsc::UnboundedSender<bool>, /* channel to send request */
  shutdown_rx: broadcast::Receiver<bool>, /* shutdown signal */
) -> Result<()> {
  // visitor (receiver of channel) - focus on handling tcp stream
  let mut visitor_rx = tcp_listen_and_send(
    bind_addr,
    data_ch_req_tx.clone(),
    shutdown_rx,
  );
  let cmd =
    bincode::serialize(&DataChannelCmd::StartForwardTcp)
      .unwrap();

  'pool: while let Some(mut visitor) =
    visitor_rx.recv().await
  {
    // new accepted connection, fetch a tcp stream
    loop {
      if let Some(mut ch) = data_ch_rx.recv().await {
        if ch.write_all(&cmd).await.is_ok() {
          tokio::spawn(async move {
            // new tcp stream <==> created data channel stream
            let _ =
              copy_bidirectional(&mut ch, &mut visitor)
                .await;
          });
          break;
        } else {
          // Current data channel is broken. Request for a new one
          if data_ch_req_tx.send(true).is_err() {
            break 'pool;
          }
        }
      } else {
        break 'pool;
      }
    }
  }

  info!("Shutdown");
  Ok(())
}

#[instrument(skip_all)]
pub(super) async fn run_udp_connection_pool<
  T: Transport,
>(
  bind_addr: String,
  mut data_ch_rx: mpsc::Receiver<T::Stream>,
  _data_ch_req_tx: mpsc::UnboundedSender<bool>,
  mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
  // TODO: Load balance

  let l = retry_notify_with_deadline(
    listen_backoff(),
    || async { Ok(UdpSocket::bind(&bind_addr).await?) },
    |e, duration| {
      warn!("{:#}. Retry in {:?}", e, duration);
    },
    &mut shutdown_rx,
  )
  .await
  .with_context(|| "Failed to listen for the service")?;

  info!("Listening at {}", &bind_addr);

  let cmd =
    bincode::serialize(&DataChannelCmd::StartForwardUdp)
      .unwrap();

  // Receive one data channel
  let mut conn = data_ch_rx
    .recv()
    .await
    .ok_or_else(|| anyhow!("No available data channels"))?;
  conn.write_all(&cmd).await?;

  let mut buf = [0u8; UDP_BUFFER_SIZE];
  loop {
    tokio::select! {
      // Forward inbound traffic to the client
      val = l.recv_from(&mut buf) => {
        let (n, from) = val?;
        UdpTraffic::write_slice(&mut conn, from, &buf[..n]).await?;
      }
      // Forward outbound traffic from the client to the visitor
      hdr_len = conn.read_u8() => {
        let t = UdpTraffic::read(&mut conn, hdr_len?).await?;
        l.send_to(&t.data, t.from).await?;
      }
      _ = shutdown_rx.recv() => {
        break;
      }
    }
  }

  debug!("UDP pool dropped");

  Ok(())
}
