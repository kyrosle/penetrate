use std::sync::Arc;

use anyhow::{bail, Result};

use crate::{
  config::ServiceType,
  protocol::{read_data_cmd, DataChannelCmd},
  transport::Transport,
};

use super::data_channel::{
  do_data_channel_handshake, run_data_channel_for_tcp,
  run_data_channel_for_udp, RunDataChannelArgs,
};

pub(super) async fn run_data_channel<T: Transport>(
  args: Arc<RunDataChannelArgs<T>>,
) -> Result<()> {
  // Do the handshake
  let mut conn =
    do_data_channel_handshake(args.clone()).await?;

  // Forward
  match read_data_cmd(&mut conn).await? {
    DataChannelCmd::StartForwardTcp => {
      if args.service.service_type != ServiceType::Tcp {
        bail!("Expect TCP traffic. Please check the configuration.")
      }
      run_data_channel_for_tcp::<T>(
        conn,
        &args.service.local_addr,
      )
      .await?;
    }
    DataChannelCmd::StartForwardUdp => {
      if args.service.service_type != ServiceType::Udp {
        bail!("Expect UDP traffic. Please check the configuration.")
      }
      run_data_channel_for_udp::<T>(
        conn,
        &args.service.local_addr,
      )
      .await?;
    }
  }
  Ok(())
}
