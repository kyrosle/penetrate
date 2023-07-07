//! Watcher of configuration
//! - `ConfigChange`: consider the change of general, server or client.
//! - `ClientServiceChange`: Add(ClientServiceConfig), Delete(service name)
//! - `ServerServiceChange`: Add(ServerServiceConfig), Delete(service name)
//! - `ConfigWatcherHandler`: handler for watching configuration changes
//!
//! providing handle of service change diff when configuration file has changed.
//! calling ConfigWatcherHandler::new(), accepting a receiver of unbounded channel of `ConfigChange`
use std::{
  collections::HashMap,
  env,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use notify::{EventKind, Watcher};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, instrument};

use crate::config::{
  ClientConfig, ClientServiceConfig, Config, ServerConfig,
  ServerServiceConfig,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ConfigChange {
  General(Box<Config>),
  ServerChange(ServerServiceChange),
  ClientChange(ClientServiceChange),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ClientServiceChange {
  Add(ClientServiceConfig),
  Delete(String),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ServerServiceChange {
  Add(ServerServiceConfig),
  Delete(String),
}

trait InstanceConfig: Clone {
  type ServiceConfig: PartialEq + Eq + Clone;
  fn equal_without_service(&self, rhs: &Self) -> bool;
  fn service_delete_change(s: String) -> ConfigChange;
  fn service_add_change(
    cfg: Self::ServiceConfig,
  ) -> ConfigChange;
  fn get_services(
    &self,
  ) -> &HashMap<String, Self::ServiceConfig>;
}

impl InstanceConfig for ServerConfig {
  type ServiceConfig = ServerServiceConfig;
  fn equal_without_service(&self, rhs: &Self) -> bool {
    let left = ServerConfig {
      services: Default::default(),
      ..self.clone()
    };

    let right = ServerConfig {
      services: Default::default(),
      ..rhs.clone()
    };

    left == right
  }
  fn service_delete_change(s: String) -> ConfigChange {
    ConfigChange::ServerChange(ServerServiceChange::Delete(
      s,
    ))
  }
  fn service_add_change(
    cfg: Self::ServiceConfig,
  ) -> ConfigChange {
    ConfigChange::ServerChange(ServerServiceChange::Add(
      cfg,
    ))
  }
  fn get_services(
    &self,
  ) -> &HashMap<String, Self::ServiceConfig> {
    &self.services
  }
}

impl InstanceConfig for ClientConfig {
  type ServiceConfig = ClientServiceConfig;
  fn equal_without_service(&self, rhs: &Self) -> bool {
    let left = ClientConfig {
      services: Default::default(),
      ..self.clone()
    };

    let right = ClientConfig {
      services: Default::default(),
      ..rhs.clone()
    };

    left == right
  }
  fn service_delete_change(s: String) -> ConfigChange {
    ConfigChange::ClientChange(ClientServiceChange::Delete(
      s,
    ))
  }
  fn service_add_change(
    cfg: Self::ServiceConfig,
  ) -> ConfigChange {
    ConfigChange::ClientChange(ClientServiceChange::Add(
      cfg,
    ))
  }
  fn get_services(
    &self,
  ) -> &HashMap<String, Self::ServiceConfig> {
    &self.services
  }
}

pub struct ConfigWatcherHandle {
  pub event_rx: mpsc::UnboundedReceiver<ConfigChange>,
}

impl ConfigWatcherHandle {
  pub async fn new(
    path: &Path,
    shutdown_rx: broadcast::Receiver<bool>,
  ) -> Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let origin_cfg = Config::from_file(path).await?;

    // Initial start
    event_tx
      .send(ConfigChange::General(Box::new(
        origin_cfg.clone(),
      )))
      .with_context(|| {
        "ConfigWatcher Handler Initial start failed"
      })?;

    tokio::spawn(config_watcher(
      path.to_owned(),
      shutdown_rx,
      event_tx,
      origin_cfg,
    ));
    Ok(ConfigWatcherHandle { event_rx })
  }
}

#[instrument(skip(shutdown_rx, event_tx, old))]
/// watching the configuration file in another thread
async fn config_watcher(
  path: PathBuf,
  mut shutdown_rx: broadcast::Receiver<bool>,
  event_tx: mpsc::UnboundedSender<ConfigChange>,
  mut old: Config,
) -> Result<()> {
  let (fevent_tx, mut fevent_rx) =
    mpsc::unbounded_channel();

  let path = if path.is_absolute() {
    path
  } else {
    env::current_dir()?.join(path)
  };

  let parent_path: &Path = path
    .parent()
    .expect("config file should have a parent dir");

  let path_clone = path.clone();

  // startup the file notify watcher
  let mut watcher = notify::recommended_watcher(
    move |res: Result<notify::Event, _>| match res {
      Ok(e) => {
        if matches!(e.kind, EventKind::Modify(_))
          && e
            .paths
            .iter()
            .map(|x| x.file_name())
            .any(|x| x == path_clone.file_name())
        {
          let _ = fevent_tx.send(true);
        }
      }
      Err(e) => error!("watch error: {:#}", e),
    },
  )?;

  watcher.watch(
    parent_path,
    notify::RecursiveMode::NonRecursive,
  )?;

  info!("Start watching the config");

  loop {
    tokio::select! {
      // configuration file changed
      e = fevent_rx.recv() => {
        match e {
          Some(_) => {
            info!("Rescan the configuration");
            let new =
              match Config::from_file(&path).await
                .with_context(||
                  "The changed configuration is invalid. Ignored")
                  {
                    Ok(v) => v,
                    Err(e) => {
                      error!("{:#}", e);
                      // If the config is invalid, just ignore it
                      continue;
                    }
                  };

            let events = calculate_events(&old, &new).into_iter().flatten();
            for event in events {
              event_tx.send(event)?;
            }

            old = new;
          },
          None => break
        }
      }
      _ = shutdown_rx.recv() => break
    }
  }
  info!("Config watcher exiting");

  Ok(())
}

fn calculate_events(
  old: &Config,
  new: &Config,
) -> Option<Vec<ConfigChange>> {
  if old == new {
    return None;
  }

  if (old.server.is_some() != new.server.is_some())
    || (old.client.is_some() != new.client.is_some())
  {
    return Some(vec![ConfigChange::General(Box::new(
      new.clone(),
    ))]);
  }

  let mut ret = vec![];

  if old.server != new.server {
    match calculate_instance_config_events(
      old.server.as_ref().unwrap(),
      new.server.as_ref().unwrap(),
    ) {
      Some(mut v) => ret.append(&mut v),
      None => {
        return Some(vec![ConfigChange::General(Box::new(
          new.clone(),
        ))])
      }
    }
  }

  if old.client != new.client {
    match calculate_instance_config_events(
      old.client.as_ref().unwrap(),
      new.client.as_ref().unwrap(),
    ) {
      Some(mut v) => ret.append(&mut v),
      None => {
        return Some(vec![ConfigChange::General(Box::new(
          new.clone(),
        ))])
      }
    }
  }

  Some(ret)
}

// None indicates a General change needed
fn calculate_instance_config_events<T: InstanceConfig>(
  old: &T,
  new: &T,
) -> Option<Vec<ConfigChange>> {
  if !old.equal_without_service(new) {
    return None;
  }

  let old = old.get_services();
  let new = new.get_services();

  let deletions = old
    .keys()
    .filter(|&name| new.get(name).is_none())
    .map(|x| T::service_delete_change(x.to_owned()));

  let addition = new
    .iter()
    .filter(|(name, c)| old.get(*name) != Some(*c))
    .map(|(_, c)| T::service_add_change(c.clone()));

  Some(deletions.chain(addition).collect())
}

#[cfg(test)]
mod test {
  use crate::config::ServerConfig;
  use pretty_assertions::assert_eq;

  use super::*;

  // macro to create map or set literal by a two elements tuple like
  // example: services: collection!(String::from("foo") => Default::default()),
  macro_rules! collection {
        // map-like
        ($($k:expr => $v:expr),* $(,)?) => {{
            use std::iter::{Iterator, IntoIterator};
            Iterator::collect(IntoIterator::into_iter([$(($k, $v),)*]))
        }};
    }

  #[test]
  fn test_calculate_events() {
    struct Test {
      old: Config,
      new: Config,
    }

    let tests = [
      // add client configuration
      Test {
        old: Config {
          server: Some(Default::default()),
          client: None,
        },
        new: Config {
          server: Some(Default::default()),
          client: Some(Default::default()),
        },
      },
      // modify `bind_addr` in serer configuration and add a server services
      Test {
        old: Config {
          server: Some(ServerConfig {
            bind_addr: String::from("127.0.0.1:2334"),
            ..Default::default()
          }),
          client: None,
        },
        new: Config {
          server: Some(ServerConfig {
            bind_addr: String::from("127.0.0.1:2333"),
            services: collection!(String::from("foo") => Default::default()),
            ..Default::default()
          }),
          client: None,
        },
      },
      // add server whole configuration
      Test {
        old: Config {
          server: Some(Default::default()),
          client: None,
        },
        new: Config {
          server: Some(ServerConfig {
            services: collection!(String::from("foo") => Default::default()),
            ..Default::default()
          }),
          client: None,
        },
      },
      // delete server whole configuration
      Test {
        old: Config {
          server: Some(ServerConfig {
            services: collection!(String::from("foo") => Default::default()),
            ..Default::default()
          }),
          client: None,
        },
        new: Config {
          server: Some(Default::default()),
          client: None,
        },
      },
      // modify server services
      Test {
        old: Config {
          server: Some(ServerConfig {
            services: collection!(
              String::from("foo1") => ServerServiceConfig::with_name("foo1"),
              String::from("foo2") => ServerServiceConfig::with_name("foo2")
            ),
            ..Default::default()
          }),
          client: Some(ClientConfig {
            services: collection!(
              String::from("foo1") => ClientServiceConfig::with_name("foo1"),
              String::from("foo2") => ClientServiceConfig::with_name("foo2")
            ),
            ..Default::default()
          }),
        },
        new: Config {
          server: Some(ServerConfig {
            services: collection!(
              String::from("bar1") => ServerServiceConfig::with_name("bar1"),
              String::from("foo2") => ServerServiceConfig::with_name("foo2")
            ),
            ..Default::default()
          }),
          client: Some(ClientConfig {
            services: collection!(
              String::from("bar1") => ClientServiceConfig::with_name("bar1"),
              String::from("bar2") => ClientServiceConfig::with_name("bar2")
            ),
            ..Default::default()
          }),
        },
      },
    ];

    // corresponding expected configuration change event
    let mut expected = [
      // add client configuration
      vec![ConfigChange::General(Box::new(
        tests[0].new.clone(),
      ))],
      // modify `bind_addr` in serer configuration and add a server services
      vec![ConfigChange::General(Box::new(
        tests[1].new.clone(),
      ))],
      // add server whole configuration
      vec![ConfigChange::ServerChange(
        ServerServiceChange::Add(Default::default()),
      )],
      // delete server whole configuration
      vec![ConfigChange::ServerChange(
        ServerServiceChange::Delete(String::from("foo")),
      )],
      // modify server services
      vec![
        ConfigChange::ServerChange(
          ServerServiceChange::Delete(String::from("foo1")),
        ),
        ConfigChange::ServerChange(
          ServerServiceChange::Add(
            tests[4].new.server.as_ref().unwrap().services
              ["bar1"]
              .clone(),
          ),
        ),
        ConfigChange::ClientChange(
          ClientServiceChange::Delete(String::from("foo1")),
        ),
        ConfigChange::ClientChange(
          ClientServiceChange::Delete(String::from("foo2")),
        ),
        ConfigChange::ClientChange(
          ClientServiceChange::Add(
            tests[4].new.client.as_ref().unwrap().services
              ["bar1"]
              .clone(),
          ),
        ),
        ConfigChange::ClientChange(
          ClientServiceChange::Add(
            tests[4].new.client.as_ref().unwrap().services
              ["bar2"]
              .clone(),
          ),
        ),
      ],
    ];

    assert_eq!(tests.len(), expected.len());

    for i in 0..tests.len() {
      let mut actual =
        calculate_events(&tests[i].old, &tests[i].new)
          .unwrap();

      let get_key = |x: &ConfigChange| -> String {
        match x {
          ConfigChange::General(_) => String::from("g"),
          ConfigChange::ServerChange(sc) => match sc {
            ServerServiceChange::Add(c) => {
              "s_add_".to_owned() + &c.name
            }
            ServerServiceChange::Delete(s) => {
              "s_del_".to_owned() + s
            }
          },
          ConfigChange::ClientChange(sc) => match sc {
            ClientServiceChange::Add(c) => {
              "c_add_".to_owned() + &c.name
            }
            ClientServiceChange::Delete(s) => {
              "c_del_".to_owned() + s
            }
          },
        }
      };

      actual.sort_by_cached_key(get_key);
      expected[i].sort_by_cached_key(get_key);

      assert_eq!(actual, expected[i]);
    }

    // No changes
    assert_eq!(
      calculate_events(
        &Config {
          server: Default::default(),
          client: None,
        },
        &Config {
          server: Default::default(),
          client: None,
        },
      ),
      None
    );
  }
}
