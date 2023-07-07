use std::{
  fmt::{Debug, Display},
  net::SocketAddr,
  time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::{
  io::{AsyncRead, AsyncWrite},
  net::{TcpStream, ToSocketAddrs},
};
use tracing::{error, trace};

use crate::{
  config::{
    ClientServiceConfig, ServerServiceConfig, TcpConfig,
    TransportConfig,
  },
  helper::{to_socket_addr, try_set_tcp_keepalive},
};

mod noise;
pub use noise::NoiseTransport;
mod tcp;
pub use tcp::TcpTransport;
mod tls;
pub use tls::TlsTransport;

pub const DEFAULT_NODELAY: bool = true;
pub const DEFAULT_KEEPALIVE_SECS: u64 = 20;
pub const DEFAULT_KEEPALIVE_INTERVAL: u64 = 8;

#[derive(Clone)]
pub struct AddrMaybeCached {
  pub addr: String,
  pub socket_addr: Option<SocketAddr>,
}

impl AddrMaybeCached {
  pub fn new(addr: &str) -> AddrMaybeCached {
    AddrMaybeCached {
      addr: addr.to_string(),
      socket_addr: None,
    }
  }

  // re-lookup the address and refresh the socket addr
  pub async fn resolve(&mut self) -> Result<()> {
    match to_socket_addr(&self.addr).await {
      Ok(s) => {
        self.socket_addr = Some(s);
        Ok(())
      }
      Err(e) => Err(e),
    }
  }
}

impl Display for AddrMaybeCached {
  fn fmt(
    &self,
    f: &mut std::fmt::Formatter<'_>,
  ) -> std::fmt::Result {
    match self.socket_addr {
      Some(s) => f.write_fmt(format_args!("{}", s)),
      None => f.write_str(&self.addr),
    }
  }
}

#[async_trait]
pub trait Transport: Debug + Send + Sync {
  type Acceptor: Send + Sync;
  type RawStream: Send + Sync;
  type Stream: 'static
    + AsyncRead
    + AsyncWrite
    + Unpin
    + Send
    + Sync
    + Debug;

  fn new(config: &TransportConfig) -> Result<Self>
  where
    Self: Sized;

  /// Provide the transport with socket options, which can
  /// be handled at the need of the transport.
  fn hint(conn: &Self::Stream, opt: SocketOpts);

  async fn bind<T: ToSocketAddrs + Send + Sync>(
    &self,
    addr: T,
  ) -> Result<Self::Acceptor>;

  /// accept must be cancel safe
  async fn accept(
    &self,
    a: &Self::Acceptor,
  ) -> Result<(Self::RawStream, SocketAddr)>;

  async fn handshake(
    &self,
    conn: Self::RawStream,
  ) -> Result<Self::Stream>;

  async fn connect(
    &self,
    addr: &AddrMaybeCached,
  ) -> Result<Self::Stream>;
}

#[derive(Debug, Clone, Copy)]
struct KeepAlive {
  // tcp_keepalive_time if the underlying protocol is TCP
  pub keepalive_secs: u64,
  // tcp_keepalive_intvl if the underlying protocol is TCP
  pub keepalive_interval: u64,
}

#[derive(Debug, Clone, Copy)]
/// Socket options
pub struct SocketOpts {
  // None means do not change
  nodelay: Option<bool>,
  // keepalive must be Some or None at the same time, or the
  // behavior will be platform-dependent
  keepalive: Option<KeepAlive>,
}

impl SocketOpts {
  fn none() -> SocketOpts {
    SocketOpts {
      nodelay: None,
      keepalive: None,
    }
  }

  // Socket options for the control channel
  pub fn for_control_channel() -> SocketOpts {
    SocketOpts {
      nodelay: Some(true), // always set nodelay for the control channel
      ..SocketOpts::none()  // None means do not change,
                           // Keepalive is set by TcpTransport
    }
  }
}

impl SocketOpts {
  pub fn from_cfg(cfg: &TcpConfig) -> SocketOpts {
    SocketOpts {
      nodelay: Some(cfg.nodelay),
      keepalive: Some(KeepAlive {
        keepalive_secs: cfg.keepalive_secs,
        keepalive_interval: cfg.keepalive_interval,
      }),
    }
  }

  pub fn from_client_cfg(
    cfg: &ClientServiceConfig,
  ) -> SocketOpts {
    SocketOpts {
      nodelay: cfg.nodelay,
      ..SocketOpts::none()
    }
  }

  pub fn from_server_cfg(
    cfg: &ServerServiceConfig,
  ) -> SocketOpts {
    SocketOpts {
      nodelay: cfg.nodelay,
      ..SocketOpts::none()
    }
  }

  /// apply socket options to tcp stream
  pub fn apply(&self, conn: &TcpStream) {
    if let Some(v) = self.keepalive {
      let keepalive_duration =
        Duration::from_secs(v.keepalive_secs);
      let keepalive_interval =
        Duration::from_secs(v.keepalive_interval);

      if let Err(e) = try_set_tcp_keepalive(
        conn,
        keepalive_duration,
        keepalive_interval,
      )
      .with_context(|| "Failed to set keepalive")
      {
        error!("{:#?}", e);
      }
    }

    if let Some(nodelay) = self.nodelay {
      trace!("Set nodelay {}", nodelay);
      if let Err(e) = conn
        .set_nodelay(nodelay)
        .with_context(|| "Failed to set nodelay")
      {
        error!("{:#?}", e)
      }
    }
  }
}
