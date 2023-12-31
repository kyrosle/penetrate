use std::path::PathBuf;

use anyhow::Result;
use tokio::{
  io::{self, AsyncReadExt, AsyncWriteExt},
  net::{TcpListener, TcpStream, ToSocketAddrs},
  sync::broadcast,
};

pub const PING: &str = "ping";
pub const PONG: &str = "pong";

pub async fn run_penetrate_server(
  config_path: &str,
  shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
  let cli = penetrate::Cli {
    config_path: Some(PathBuf::from(config_path)),
    server: true,
    client: false,
    ..Default::default()
  };
  penetrate::run(cli, shutdown_rx).await
}

pub async fn run_penetrate_client(
  config_path: &str,
  shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
  let cli = penetrate::Cli {
    config_path: Some(PathBuf::from(config_path)),
    server: false,
    client: true,
    ..Default::default()
  };
  penetrate::run(cli, shutdown_rx).await
}

pub mod tcp {
  use super::*;

  /// Echo server in tcp connection
  pub async fn echo_server<A: ToSocketAddrs>(
    addr: A,
  ) -> Result<()> {
    let l = TcpListener::bind(addr).await?;

    loop {
      let (conn, _addr) = l.accept().await?;
      tokio::spawn(async move {
        let _ = echo(conn).await;
      });
    }
  }

  pub async fn pingpong_server<A: ToSocketAddrs>(
    addr: A,
  ) -> Result<()> {
    let l = TcpListener::bind(addr).await?;

    loop {
      let (conn, _addr) = l.accept().await?;
      tokio::spawn(async move {
        let _ = pingpong(conn).await;
      });
    }
  }

  /// Continuously read data from reader and then write it into writer
  /// in a streaming fashion until reader returns EOF or fails.
  async fn echo(conn: TcpStream) -> Result<()> {
    let (mut rd, mut wr) = conn.into_split();
    io::copy(&mut rd, &mut wr).await?;

    Ok(())
  }

  /// This read a `Ping` and then write a `Pong` into it.
  async fn pingpong(mut conn: TcpStream) -> Result<()> {
    let mut buf = [0u8; PING.len()];

    while conn.read_exact(&mut buf).await? != 0 {
      assert_eq!(buf, PING.as_bytes());
      conn.write_all(PONG.as_bytes()).await?;
    }

    Ok(())
  }
}

pub mod udp {
  use penetrate::constants::UDP_BUFFER_SIZE;
  use tokio::net::UdpSocket;
  use tracing::debug;

  use super::*;

  /// Echo server in udp connection
  pub async fn echo_server<A: ToSocketAddrs>(
    addr: A,
  ) -> Result<()> {
    let l = UdpSocket::bind(addr).await?;
    debug!("UDP echo server listening");

    let mut buf = [0u8; UDP_BUFFER_SIZE];
    loop {
      let (n, addr) = l.recv_from(&mut buf).await?;
      debug!("Get {:?} from {}", &buf[..n], addr);
      l.send_to(&buf[..n], addr).await?;
    }
  }
/// Receive `ping` and send `pong`
  pub async fn pingpong_server<A: ToSocketAddrs>(
    addr: A,
  ) -> Result<()> {
    let l = UdpSocket::bind(addr).await?;

    let mut buf = [0u8; UDP_BUFFER_SIZE];
    loop {
      let (n, addr) = l.recv_from(&mut buf).await?;
      assert_eq!(&buf[..n], PING.as_bytes());
      l.send_to(PONG.as_bytes(), addr).await?;
    }
  }
}
