use std::{
  future::Future, net::SocketAddr, time::Duration,
};

use anyhow::{anyhow, Result};
use async_http_proxy::{
  http_connect_tokio, http_connect_tokio_with_basic_auth,
};
use backoff::{backoff::Backoff, Notify};
use socket2::{SockRef, TcpKeepalive};
use tokio::net::{
  lookup_host, TcpStream, ToSocketAddrs, UdpSocket,
};
use tokio::sync::broadcast;
use tracing::trace;
use url::Url;

use crate::transport::AddrMaybeCached;

/// try to set tcp keepalive with a tcp stream,
/// set keepalive duration and keepalive interval in the socket
pub fn try_set_tcp_keepalive(
  conn: &TcpStream,
  keepalive_duration: Duration,
  keepalive_interval: Duration,
) -> Result<()> {
  let s = SockRef::from(conn);
  let keepalive = TcpKeepalive::new()
    .with_time(keepalive_duration)
    .with_interval(keepalive_interval);

  trace!(
    "Set TCP keepalive {:?} {:?}",
    keepalive_duration,
    keepalive_interval
  );

  Ok(s.set_tcp_keepalive(&keepalive)?)
}

/// lookup a the address
pub async fn to_socket_addr<A: ToSocketAddrs>(
  addr: A,
) -> Result<SocketAddr> {
  lookup_host(addr)
    .await?
    .next()
    .ok_or_else(|| anyhow!("Failed to lookup the host"))
}

pub fn host_port_pair(s: &str) -> Result<(&str, u16)> {
  let semi = s.rfind(':').expect("missing semicolon");
  Ok((&s[..semi], s[semi + 1..].parse()?))
}

/// Create a UDP socket
pub async fn udp_connect<A: ToSocketAddrs>(
  addr: A,
) -> Result<UdpSocket> {
  let addr = to_socket_addr(addr).await?;

  let bind_addr = match addr {
    SocketAddr::V4(_) => "0.0.0.0:0",
    SocketAddr::V6(_) => ":::0",
  };

  let s = UdpSocket::bind(bind_addr).await?;
  s.connect(addr).await?;
  Ok(s)
}

/// Create a TcpStream using a proxy
/// e.g.
/// - socks5://user:pass@127.0.0.1:1080
/// - http://127.0.0.1:8080
pub async fn tcp_connect_with_proxy(
  addr: &AddrMaybeCached,
  proxy: Option<&Url>,
) -> Result<TcpStream> {
  if let Some(url) = proxy {
    let addr = &addr.addr;
    let host = url
      .host_str()
      .expect("proxy url should have host field");
    let port =
      url.port().expect("proxy url should have port field");
    let mut s = TcpStream::connect((host, port)).await?;

    let auth = if !url.username().is_empty()
      || url.password().is_some()
    {
      Some(async_socks5::Auth {
        username: url.username().into(),
        password: url.password().unwrap_or("").into(),
      })
    } else {
      None
    };

    // handle with url scheme
    match url.scheme() {
      "socks5" => {
        async_socks5::connect(
          &mut s,
          host_port_pair(addr)?,
          auth,
        )
        .await?;
      }
      "http" => {
        let (host, port) = host_port_pair(addr)?;
        match auth {
          Some(auth) => {
            http_connect_tokio_with_basic_auth(
              &mut s,
              host,
              port,
              &auth.username,
              &auth.password,
            )
            .await?
          }
          None => {
            http_connect_tokio(&mut s, host, port).await?
          }
        }
      }
      _ => panic!("unknown proxy scheme"),
    }
    Ok(s)
  } else {
    Ok(match addr.socket_addr {
      // connect directly
      Some(s) => TcpStream::connect(s).await?,
      None => TcpStream::connect(&addr.addr).await?,
    })
  }
}

/// Wrapper of retry_notify
pub async fn retry_notify_with_deadline<
  I,
  E,
  Fn,
  Fut,
  B,
  N,
>(
  backoff: B,
  operation: Fn,
  notify: N,
  deadline: &mut broadcast::Receiver<bool>,
) -> Result<I>
where
  E: std::error::Error + Send + Sync + 'static,
  B: Backoff,
  Fn: FnMut() -> Fut,
  Fut: Future<
    Output = std::result::Result<I, backoff::Error<E>>,
  >,
  N: Notify<E>,
{
  tokio::select! {
    v = backoff::future::retry_notify(backoff, operation, notify) => {
      v.map_err(anyhow::Error::new)
    }
    _ = deadline.recv() => {
      Err(anyhow!("shutdown"))
    }
  }
}
