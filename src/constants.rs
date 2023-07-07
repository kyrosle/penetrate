//! ExponentialBackoff is a backoff implementation that increases the backoff period
//! for each retry attempt using a randomization function that grows exponentially.
//! next_backoff is calculated using the following formula:
//! ```
//! randomized interval =
//!       retry_interval * (random value in range [1 - randomization_factor, 1 + randomization_factor])
//! ```
use std::time::Duration;

use backoff::ExponentialBackoff;

// FIXME: Determine reasonable size
/// UDP MTU. Currently far larger than necessary
pub const UDP_BUFFER_SIZE: usize = 2048;
pub const UDP_SEND_Q_SIZE: usize = 1024;
pub const UDP_TIMEOUT: u64 = 60;

pub fn listen_backoff() -> ExponentialBackoff {
  ExponentialBackoff {
    max_elapsed_time: None,
    max_interval: Duration::from_secs(1),
    ..Default::default()
  }
}

pub fn run_control_channel_backoff(
  interval: u64,
) -> ExponentialBackoff {
  ExponentialBackoff {
    randomization_factor: 0.2,
    max_elapsed_time: None,
    multiplier: 3.0,
    max_interval: Duration::from_secs(interval),
    ..Default::default()
  }
}
