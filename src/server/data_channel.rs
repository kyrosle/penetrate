use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::transport::{SocketOpts, Transport};

use super::{ControlChannelMap, Nonce};

pub(super) async fn do_data_channel_handshake<
  T: 'static + Transport,
>(
  conn: T::Stream,
  control_channels: Arc<RwLock<ControlChannelMap<T>>>,
  nonce: Nonce,
) -> Result<()> {
  debug!("Try to handshake a data channel");

  // Validate
  let control_channel_guard = control_channels.read().await;

  match control_channel_guard.get2(&nonce) {
    Some(handle) => {
      T::hint(
        &conn,
        SocketOpts::from_server_cfg(&handle.service),
      );

      // Send the data channel to the corresponding control channel
      handle.data_ch_tx.send(conn).await.with_context(
        || "Data channel for a stale control channel",
      )?;
    }
    None => {
      warn!("Data channel has incorrect nonce");
    }
  }
  Ok(())
}
