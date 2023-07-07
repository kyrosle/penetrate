//! Protocol definition
//! - `Hello`: send and read for control channel and data channel to fetch the nonce(number used once).
//! - `Auth`: the hash of authentication.
//! - `Ack`: check the service handshake from server
//! - `ControlChannelCmd`: CreateDataChannel or HeartBeat
//! - `DataChannelCmd`: StartForwardTcp or StartForwardUdp
use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::io::{
  AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tracing::trace;

pub const HASH_WIDTH_IN_BYTES: usize = 32;
type ProtocolVersion = u8;
const PROTO_V1: u8 = 1u8;
pub const CURRENT_PROTO_VERSION: ProtocolVersion = PROTO_V1;

pub type Digest = [u8; HASH_WIDTH_IN_BYTES];

#[derive(Deserialize, Serialize, Debug)]
/// Hello protocol definition,
/// Time of use:
/// - client:
///   - control channel send/read hello
///   - data channel send nonce
pub enum Hello {
  ControlChannelHello(ProtocolVersion, Digest), // sha256sum(service name) or a nonce
  DataChannelHello(ProtocolVersion, Digest), // token provided by CreateDataChannel
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Auth(pub Digest);

#[derive(Deserialize, Serialize, Debug)]
/// Ack protocol definition,
/// - client:
///   - control channel read ack and judge the service status
pub enum Ack {
  Ok,
  ServiceNotExist,
  AuthFailed,
}

impl std::fmt::Display for Ack {
  fn fmt(
    &self,
    f: &mut std::fmt::Formatter<'_>,
  ) -> std::fmt::Result {
    write!(
      f,
      "{}",
      match self {
        Ack::Ok => "Ok",
        Ack::ServiceNotExist => "Service not exist",
        Ack::AuthFailed => "Incorrect token",
      }
    )
  }
}

#[derive(Deserialize, Serialize, Debug)]
/// Command of Control Channel
pub enum ControlChannelCmd {
  CreateDataChannel,
  HeartBeat,
}

#[derive(Deserialize, Serialize, Debug)]
/// Command of Data Channel
pub enum DataChannelCmd {
  StartForwardTcp,
  StartForwardUdp,
}

// `u16` should be enough for any practical UDP traffic on the Internet
type UdpPacketLen = u16;

#[derive(Deserialize, Serialize, Debug)]
/// Udp packet header
struct UdpHeader {
  /// Sender socket address
  from: SocketAddr,
  /// length of udp packet
  len: UdpPacketLen,
}

#[derive(Deserialize, Serialize, Debug)]
/// Udp packet
pub struct UdpTraffic {
  /// Sender socket address
  pub from: SocketAddr,
  /// bytes data()
  pub data: Bytes,
}

impl UdpTraffic {
  /// write udp packet into stream
  pub async fn write<T: AsyncWrite + Unpin>(
    &self,
    writer: &mut T,
  ) -> Result<()> {
    // set udp header
    let hdr = UdpHeader {
      from: self.from,
      len: self.data.len() as UdpPacketLen,
    };

    let v =
      bincode::serialize(&hdr).with_context(|| {
        format!(
          "Failed to bincode::serialize the UdpHeader: {:?}",
          hdr
        )
      })?;

    trace!("Write {:?} of length {}", hdr, v.len());

    // write udp header len
    writer.write_u8(v.len() as u8).await?;
    // write udp header
    writer.write_all(&v).await?;
    // write udp packet data
    writer.write_all(&self.data).await?;

    Ok(())
  }

  pub async fn write_slice<T: AsyncWrite + Unpin>(
    writer: &mut T,
    from: SocketAddr,
    data: &[u8],
  ) -> Result<()> {
    let hdr = UdpHeader {
      from,
      len: data.len() as UdpPacketLen,
    };

    let v = bincode::serialize(&hdr).unwrap();

    trace!("Write {:?} of length {}", hdr, v.len());

    writer.write_u8(v.len() as u8).await?;
    writer.write_all(&v).await?;
    writer.write_all(data).await?;

    Ok(())
  }

  // read udp packet from stream with the predict udp header
  pub async fn read<T: AsyncRead + Unpin>(
    reader: &mut T,
    hdr_len: u8,
  ) -> Result<UdpTraffic> {
    // reader buffer
    let mut buf = Vec::new();
    buf.resize(hdr_len as usize, 0);

    reader
      .read_exact(&mut buf)
      .await
      .with_context(|| "Failed to read udp header")?;

    // parsing udp packet header part
    let hdr: UdpHeader = bincode::deserialize(&buf)
      .with_context(|| {
        "Failed to bincode::deserialize UdpHeader"
      })?;

    trace!("hdr {:?}", hdr);

    // read the udp packet data part
    let mut data = BytesMut::new();
    data.resize(hdr.len as usize, 0);
    reader.read_exact(&mut data).await?;

    Ok(UdpTraffic {
      from: hdr.from,
      data: data.freeze(), /* convert BytesMut into Bytes */
    })
  }
}

/// Compressed bytes data into abstract in sha256.
pub fn digest(data: &[u8]) -> Digest {
  use sha2::{Digest, Sha256};
  let mut hasher = Sha256::new();
  hasher.update(data);
  let result = hasher.finalize();
  result.into()
}

/// Recording the Packet Length metadata after bincode::serialized (global static structure)
struct PacketLength {
  hello: usize,
  ack: usize,
  auth: usize,
  c_cmd: usize,
  d_cmd: usize,
}

impl PacketLength {
  pub fn new() -> PacketLength {
    let username = "default";

    let d = digest(username.as_bytes());
    let hello = bincode::serialized_size(
      &Hello::ControlChannelHello(CURRENT_PROTO_VERSION, d),
    )
    .unwrap() as usize;

    let c_cmd = bincode::serialized_size(
      &ControlChannelCmd::CreateDataChannel,
    )
    .unwrap() as usize;

    let d_cmd = bincode::serialized_size(
      &DataChannelCmd::StartForwardTcp,
    )
    .unwrap() as usize;

    let ack = Ack::Ok;
    let ack =
      bincode::serialized_size(&ack).unwrap() as usize;

    let auth =
      bincode::serialized_size(&Auth(d)).unwrap() as usize;

    PacketLength {
      hello,
      ack,
      auth,
      c_cmd,
      d_cmd,
    }
  }
}

static PACKET_LEN: Lazy<PacketLength> =
  Lazy::new(PacketLength::new);

// read Hello message form stream
pub async fn read_hello<
  T: AsyncRead + AsyncWrite + Unpin,
>(
  conn: &mut T,
) -> Result<Hello> {
  let mut buf = vec![0u8; PACKET_LEN.hello];
  conn
    .read_exact(&mut buf)
    .await
    .with_context(|| "Failed to read hello")?;

  let hello = bincode::deserialize(&buf)
    .with_context(|| "Failed to deserialize hello")?;

  // check the hello protocol version wether equal to current protocol version
  match hello {
    Hello::ControlChannelHello(v, _) => {
      if v != CURRENT_PROTO_VERSION {
        bail!("Protocol version mismatched. Expected {}, got {}. Please update", CURRENT_PROTO_VERSION, v)
      }
    }
    Hello::DataChannelHello(v, _) => {
      if v != CURRENT_PROTO_VERSION {
        bail!("Protocol version mismatched. Expected {}, got {}. Please update", CURRENT_PROTO_VERSION, v)
      }
    }
  }
  Ok(hello)
}

/// read Auth message from stream
pub async fn read_auth<
  T: AsyncRead + AsyncWrite + Unpin,
>(
  conn: &mut T,
) -> Result<Auth> {
  let mut buf = vec![0u8; PACKET_LEN.auth];

  conn
    .read_exact(&mut buf)
    .await
    .with_context(|| "Failed to read auth")?;

  bincode::deserialize(&buf)
    .with_context(|| "Failed to deserialize auth")
}

/// read Ack message from stream
pub async fn read_ack<T: AsyncRead + AsyncWrite + Unpin>(
  conn: &mut T,
) -> Result<Ack> {
  let mut bytes = vec![0u8; PACKET_LEN.ack];

  conn
    .read_exact(&mut bytes)
    .await
    .with_context(|| "Failed to read ack")?;

  bincode::deserialize(&bytes)
    .with_context(|| "Failed to deserialize ack")
}

/// read control command message from stream
pub async fn read_control_cmd<
  T: AsyncRead + AsyncWrite + Unpin,
>(
  conn: &mut T,
) -> Result<ControlChannelCmd> {
  let mut bytes = vec![0u8; PACKET_LEN.c_cmd];

  conn
    .read_exact(&mut bytes)
    .await
    .with_context(|| "Failed to read control cmd")?;

  bincode::deserialize(&bytes)
    .with_context(|| "Failed to deserialize control cmd")
}

/// read data command message from stream
pub async fn read_data_cmd<
  T: AsyncRead + AsyncWrite + Unpin,
>(
  conn: &mut T,
) -> Result<DataChannelCmd> {
  let mut bytes = vec![0u8; PACKET_LEN.d_cmd];

  conn
    .read_exact(&mut bytes)
    .await
    .with_context(|| "Failed to read data cmd")?;

  bincode::deserialize(&bytes)
    .with_context(|| "Failed to deserialize data cmd")
}
