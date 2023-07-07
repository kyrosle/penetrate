use std::{
  collections::HashMap, net::SocketAddr, sync::Arc,
  time::Duration,
};

use anyhow::{Context, Result};
use backoff::{future::retry_notify, ExponentialBackoff};
use bytes::{Bytes, BytesMut};
use tokio::{
  io::{
    self, copy_bidirectional, AsyncReadExt, AsyncWriteExt,
  },
  net::{TcpStream, UdpSocket},
  sync::{mpsc, RwLock},
  time,
};
use tracing::{debug, error, instrument, trace, warn};

use crate::{
  config::ClientServiceConfig,
  constants::{
    UDP_BUFFER_SIZE, UDP_SEND_Q_SIZE, UDP_TIMEOUT,
  },
  helper::udp_connect,
  protocol::{
    Hello, UdpTraffic, CURRENT_PROTO_VERSION,
    HASH_WIDTH_IN_BYTES,
  },
  transport::{AddrMaybeCached, SocketOpts, Transport},
};

use super::Nonce;

/// Context for data channel
pub(super) struct RunDataChannelArgs<T: Transport> {
  pub(super) session_key: Nonce,
  pub(super) remote_addr: AddrMaybeCached,
  pub(super) connector: Arc<T>,
  pub(super) socket_opts: SocketOpts,
  pub(super) service: ClientServiceConfig,
}

/// Do data channel handshake when at the beginning of calling `run_data_channel`
pub(super) async fn do_data_channel_handshake<
  T: Transport,
>(
  args: Arc<RunDataChannelArgs<T>>,
) -> Result<T::Stream> {
  // Retry at least every 100ms, at most for 10 seconds
  let backoff = ExponentialBackoff {
    max_interval: Duration::from_millis(100),
    max_elapsed_time: Some(Duration::from_secs(10)),
    ..Default::default()
  };

  // Connect to remote_addr, asynchronously retry backoff
  let mut conn: T::Stream = retry_notify(
    backoff,
    || async {
      /* connect execution */
      args
        .connector
        .connect(&args.remote_addr)
        .await
        .with_context(|| {
          format!(
            "Failed to connect to {}",
            &args.remote_addr
          )
        })
        .map_err(backoff::Error::transient)
    },
    |e, duration| {
      warn!("{:#}, Retry in {:?}", e, duration);
    },
  )
  .await?;

  // apply the socket options for connection
  T::hint(&conn, args.socket_opts);

  // Send nonce to data channel hello
  let v: &[u8; HASH_WIDTH_IN_BYTES] =
    args.session_key[..].try_into().unwrap();
  let hello = Hello::DataChannelHello(
    CURRENT_PROTO_VERSION,
    v.to_owned(),
  );
  conn
    .write_all(&bincode::serialize(&hello).unwrap())
    .await?;
  conn.flush().await?;

  Ok(conn)
}

#[instrument(skip(conn))]
/// run data channel for tcp when receiving `DataChannelCmd::StartForwardTcp` in `read_data_cmd`
pub(super) async fn run_data_channel_for_tcp<
  T: Transport,
>(
  mut conn: T::Stream,
  local_addr: &str,
) -> Result<()> {
  debug!("New data channel starts forwarding");

  let mut local =
    TcpStream::connect(local_addr).await.with_context(
      || format!("Failed to connect to {}", local_addr),
    )?;

  // Copies data in both directions between `conn` and `local`
  let _ = copy_bidirectional(&mut conn, &mut local).await;
  Ok(())
}

type UdpPortMap =
  Arc<RwLock<HashMap<SocketAddr, mpsc::Sender<Bytes>>>>;

#[instrument(skip(conn))]
pub(super) async fn run_data_channel_for_udp<
  T: Transport,
>(
  conn: T::Stream,
  local_addr: &str,
) -> Result<()> {
  debug!("New data channel starts forwarding");

  let port_map: UdpPortMap =
    Arc::new(RwLock::new(HashMap::new()));

  // The channel stores UdpTraffic that needs to be sent to the server
  let (outbound_tx, mut outbound_rx) =
    mpsc::channel::<UdpTraffic>(UDP_BUFFER_SIZE);

  // FIXME: https://github.com/tokio-rs/tls/issues/40
  // Maybe this is our concern
  let (mut rd, mut wr) = io::split(conn);

  // Keep sending items from the outbound channel to the server
  tokio::spawn(async move {
    while let Some(t) = outbound_rx.recv().await {
      trace!("outbound {:?}", t);
      if let Err(e) =
        t.write(&mut wr).await.with_context(|| {
          "Failed to forward UDP traffic to the server"
        })
      {
        debug!("{:?}", e);
        break;
      }
    }
  });

  loop {
    // Read a packet from the server
    let hdr_len = rd.read_u8().await?;
    let packet = UdpTraffic::read(&mut rd, hdr_len)
      .await
      .with_context(|| {
      "Failed to read UDPTraffic from the server"
    })?;

    let m = port_map.read().await;

    if m.get(&packet.from).is_none() {
      // This packet is from a address we don't see for a while,
      // which is not in the UdpPortMap.
      // So set up a mapping (and a forwarding) for it

      // Drop the reader lock
      drop(m);

      // Grab the writer lock
      // This is the only thread that will try to grab the writer lock
      // So no need to worry about some other thread has already set up
      // the mapping between the gap of dropping the reader lock and
      // grabbing the writer lock
      let mut m = port_map.write().await;

      match udp_connect(local_addr).await {
        Ok(s) => {
          let (inbound_tx, inbound_rx) =
            mpsc::channel(UDP_SEND_Q_SIZE);

          m.insert(
            packet.from,
            inbound_tx, /* UDP_SEND_Q */
          );

          tokio::spawn(run_udp_forwarder(
            s,
            inbound_rx,          /* UDP_SEND_Q */
            outbound_tx.clone(), /* UDP_BUFFER_SIZE */
            packet.from,
            port_map.clone(),
          ));
        }
        Err(e) => {
          error!("{:#?}", e);
        }
      }
    }

    // Now there should be a udp forwarder that can receive the packet
    let m = port_map.read().await;
    if let Some(tx) = m.get(&packet.from) {
      let _ = tx.send(packet.data).await;
    }
  }
}

#[instrument(skip_all, fields(from))]
async fn run_udp_forwarder(
  s: UdpSocket,
  mut inbound_rx: mpsc::Receiver<Bytes>,
  outbound_tx: mpsc::Sender<UdpTraffic>,
  from: SocketAddr,
  port_map: UdpPortMap,
) -> Result<()> {
  debug!("Forwarder created");
  let mut buf = BytesMut::new();
  buf.resize(UDP_BUFFER_SIZE, 0);

  loop {
    tokio::select! {
      // Receive from the server
      data = inbound_rx.recv() => {
        if let Some(data) = data {
          s.send(&data).await?;
        } else {
          break;
        }
      }
      // Receive from the service
      val = s.recv(&mut buf) => {
        let len = match val {
          Ok(v) => v,
          Err(_) => break,
        };

        let t = UdpTraffic{
          from,
          data: Bytes::copy_from_slice(&buf[..len])
        };

        outbound_tx.send(t).await?;
      }
      // No traffic for duration of UDP_TIMEOUT, clean up the state
      _ = time::sleep(Duration::from_secs(UDP_TIMEOUT)) => {
        break;
      }
    }
  }
  let mut port_map = port_map.write().await;
  port_map.remove(&from);

  debug!("Forwarder dropped");
  Ok(())
}
