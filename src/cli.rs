use clap::{Parser, ValueEnum};

#[derive(
  Copy,
  Clone,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  ValueEnum,
  Debug,
)]
pub enum KeyPairType {
  X22519,
  X448,
}

#[derive(Parser, Debug, Clone, Default)]
pub struct Cli {
  /// The path to teh configuration file
  ///
  /// Running as a client or a server is automatically determined
  /// according to the configuration file.
  #[arg(name = "CONFIG")]
  pub config_path: Option<std::path::PathBuf>,

  /// Run as a server
  #[arg(long, short, group = "mode")]
  pub server: bool,

  /// Run as a client
  #[arg(long, short, group = "mode")]
  pub client: bool,

  /// Generate a keypair for the use of the noise protocol
  ///
  /// The DH function to use is X25519
  #[arg(long, name = "CURVE")]
  pub genkey: Option<Option<KeyPairType>>,
}
