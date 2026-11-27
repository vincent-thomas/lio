//! Reading the resolver configuration from files.

use std::{io, path::Path};

use lio::{
  api::Io,
  fs::{self, ReadToString},
  model::{Cx, Join, OpModel, Step},
};

use crate::{Hosts, Resolver, ResolverConfig};

/// Reads a `resolv.conf(5)` and a `hosts(5)` file into a [`Resolver`],
/// from [`Resolver::system`] or [`Resolver::load`].
pub struct LoadSystem {
  files: Join<Io<ReadToString>, Io<ReadToString>>,
}

impl LoadSystem {
  pub(crate) fn new(resolv_conf: &Path, hosts: &Path) -> Self {
    Self {
      files: Join::new(
        fs::read_to_string(resolv_conf),
        fs::read_to_string(hosts),
      ),
    }
  }
}

impl OpModel for LoadSystem {
  type Item = io::Result<Resolver>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    let (resolv_conf, hosts) = lio::ready!(self.files.poll(cx));
    let resolv_conf = match resolv_conf {
      Ok(text) => text,
      Err(err) => return Step::Done(Err(err)),
    };
    let hosts = match hosts {
      Ok(text) => Hosts::parse(&text),
      Err(err) if err.kind() == io::ErrorKind::NotFound => Hosts::new(),
      Err(err) => return Step::Done(Err(err)),
    };
    let mut config = ResolverConfig::from_resolv_conf(&resolv_conf);
    config.hosts = hosts;
    Step::Done(Ok(Resolver::new(config)))
  }
}
