use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use tokio::{
  io::AsyncWriteExt,
  sync::oneshot,
  time::{self, Instant},
};
use tracing::{
  debug, error, info, instrument, warn, Instrument, Span,
};

use crate::{
  client::{
    data_channel::RunDataChannelArgs,
    traffic::run_data_channel,
  },
  config::ClientServiceConfig,
  constants::run_control_channel_backoff,
  protocol::{
    self, read_ack, read_control_cmd, read_hello, Ack,
    Auth, ControlChannelCmd, Hello, CURRENT_PROTO_VERSION,
  },
  transport::{AddrMaybeCached, SocketOpts, Transport},
};

use super::ServiceDigest;

/// Control channel, using T as the transport layer.
pub(super) struct ControlChannel<T: Transport> {
  /// SHA256 of the service name
  digest: ServiceDigest,
  /// `[client.services.foo]` config block
  service: ClientServiceConfig,
  /// Receivers the shutdown signal
  shutdown_rx: oneshot::Receiver<u8>,
  /// `client.remote_addr`
  remote_addr: String,
  /// Wrapper around the transport layer
  transport: Arc<T>,
  /// Application layer heartbeat timeout in secs
  heartbeat_timeout: u64,
}

/// Handle of a control channel(also the service handler entity),
/// Dropping it will also drop the actual control channel(drop the service)
pub(super) struct ControlChannelHandle {
  shutdown_tx: oneshot::Sender<u8>,
}

impl<T: 'static + Transport> ControlChannel<T> {
  #[instrument(skip_all)]
  /// Startup the control channel(startup the service)
  pub(super) async fn run(&mut self) -> Result<()> {
    // wrapping address
    let mut remote_addr =
      AddrMaybeCached::new(&self.remote_addr);
    // refresh address
    remote_addr.resolve().await?;

    // build transport the connection
    let mut conn = self
      .transport
      .connect(&remote_addr)
      .await
      .with_context(|| {
        format!(
          "Failed to connect to {}",
          &self.remote_addr
        )
      })?;
    // control channel set nodelay, and apply the socket options
    T::hint(&conn, SocketOpts::for_control_channel());

    // Send hello
    debug!("Sending hello");
    let hello_send = Hello::ControlChannelHello(
      CURRENT_PROTO_VERSION,
      self.digest[..].try_into().unwrap(), /* service name(sha256) */
    );
    conn
      .write_all(&bincode::serialize(&hello_send).unwrap())
      .await?;
    conn.flush().await?;

    // Read hello
    debug!("Reading hello");
    // number which use once
    let nonce = match read_hello(&mut conn).await? {
      Hello::ControlChannelHello(_, d) => d,
      _ => {
        bail!("Unexpected type of hello");
      }
    };

    // Send Auth
    debug!("Sending auth");
    let mut concat = Vec::from(
      self.service.token.as_ref().unwrap().as_bytes(),
    );
    concat.extend_from_slice(&nonce);

    let session_key = protocol::digest(&concat); /* sha256 */
    let auth = Auth(session_key);
    conn
      .write_all(&bincode::serialize(&auth).unwrap())
      .await?;
    conn.flush().await?;

    // Read ack, normally will accept a Ok, otherwise it occurs some error during the traffic
    debug!("Reading ack");
    match read_ack(&mut conn).await? {
      Ack::Ok => {}
      v => {
        return Err(anyhow!("{}", v)).with_context(|| {
          format!(
            "Authentication failed: ControlChannelCmd{}",
            self.service.name
          )
        })
      }
    }

    // Channel ready
    info!("Control channel established");

    // Socket options for the data channel
    let socket_opts =
      SocketOpts::from_client_cfg(&self.service); /* use the config marked: nodelay */
    let data_ch_args = Arc::new(RunDataChannelArgs {
      session_key,
      remote_addr,
      connector: self.transport.clone(),
      socket_opts,
      service: self.service.clone(),
    });

    loop {
      tokio::select! {
        // control channel receive control command
        val = read_control_cmd(&mut conn) => {
          let val = val?;
          debug!("Received {:?}", val);
          match val {
            ControlChannelCmd::CreateDataChannel => {
              // here client is notified to create a data channel
              let args = data_ch_args.clone();
              tokio::spawn(async move {
                if let Err(e) = run_data_channel(args).await.with_context(|| "Failed to run the data channel") {
                  warn!("{:#}", e);
                }
              }.instrument(Span::current()));
            },
            // TODO: the heartbeat command should bring show message, or add packet traffic throughout?
            ControlChannelCmd::HeartBeat => (),
          }
        },
        // if the HeartBeat Control Channel Command dose not send to client from server in time(heartbeat_timeout),
        // the program control flow will execute here.
        _ = time::sleep(Duration::from_secs(self.heartbeat_timeout)),if self.heartbeat_timeout != 0 => {
          return Err(anyhow!("HeatBeat timed out"))
        }
        // notified to shutdown
        _ = &mut self.shutdown_rx => {
          break;
        }
      }
    }

    info!("Control channel shutdown");
    Ok(())
  }
}

impl ControlChannelHandle {
  #[instrument(name ="handle", skip_all, fields(service = %service.name))]
  /// Create a handle for control channel.
  pub(super) fn new<T: 'static + Transport>(
    service: ClientServiceConfig,
    remote_addr: String,
    transport: Arc<T>,
    heartbeat_timeout: u64,
  ) -> ControlChannelHandle {
    // service name -> sha256 to transport
    let digest = protocol::digest(service.name.as_bytes());

    info!("Starting {}", hex::encode(digest));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    // debug!(
    //   "ControlChannelHandle client service retry_interval: {:#?}",
    //   service.retry_interval
    // );
    let mut retry_backoff = run_control_channel_backoff(
      service.retry_interval.unwrap(),
    );

    let mut s = ControlChannel {
      digest,
      service,
      shutdown_rx,
      remote_addr,
      transport,
      heartbeat_timeout,
    };

    tokio::spawn(
      async move {
        // set now timestamp
        let mut start = Instant::now();

        while let Err(err) =
          s.run().await.with_context(|| {
            "Failed to run the control channel"
          })
        {
          // handle the control channel error situation

          // receive shutdown signal
          if s.shutdown_rx.try_recv()
            != Err(oneshot::error::TryRecvError::Empty)
          {
            break;
          }

          if start.elapsed() > Duration::from_secs(3) {
            // The client runs for at least 3 secs and then disconnects
            retry_backoff.reset();
          }

          if let Some(duration) =
            retry_backoff.next_backoff()
          {
            error!("{:#}. Retry in {:?}...", err, duration);
            // async sleep until timeout(duration)
            time::sleep(duration).await;
          } else {
            // Should never reach
            panic!("{:#}. Break", err);
          }

          start = Instant::now();
        }
      }
      .instrument(Span::current()),
    );

    ControlChannelHandle { shutdown_tx }
  }

  pub(super) fn shutdown(self) {
    // A send failure shows that the actor has already shutdown.
    let _ = self.shutdown_tx.send(0u8);
  }
}
