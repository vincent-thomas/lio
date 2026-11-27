//! A DNS stub resolver for lio.
//!
//! [`hickory-proto`](hickory_proto) encodes the queries and decodes the
//! answers; lio sends and receives them. A lookup is an [`OpModel`], so it
//! runs the way every lio operation does: `.await` it, or run it with
//! `send`, `when_done` or `with_lio`.
//!
//! ```no_run
//! use lio_dns::Resolver;
//!
//! async fn example() -> Result<(), Box<dyn std::error::Error>> {
//!   // Reads /etc/resolv.conf and /etc/hosts.
//!   let resolver = Resolver::system().await?;
//!   for addr in resolver.lookup_ip("example.com").await? {
//!     println!("{addr}");
//!   }
//!   Ok(())
//! }
//! ```
//!
//! Without the system's files, name the nameservers:
//!
//! ```no_run
//! use lio_dns::{Resolver, ResolverConfig, proto::rr::RecordType};
//!
//! let lio = lio::Lio::new(64).unwrap();
//! let config = ResolverConfig::new(vec!["9.9.9.9:53".parse().unwrap()]);
//! let resolver = Resolver::new(config);
//! resolver
//!   .lookup("example.com", RecordType::MX)
//!   .with_lio(&lio)
//!   .when_done(|answer| {
//!     for record in answer.unwrap().answers {
//!       println!("{record}");
//!     }
//!   });
//! while lio.try_run().unwrap() > 0 {}
//! ```
//!
//! # What it does not do
//!
//! It asks over UDP, and over TCP when an answer is truncated. It does not
//! cache, validate DNSSEC, or speak DNS over TLS, HTTPS or QUIC. It reads
//! `resolv.conf` and `hosts`, not what `nsswitch.conf` or the platform's
//! own resolver configuration (mDNS, per-domain resolvers) would add.
//!
//! [`OpModel`]: lio::model::OpModel

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod config;
mod error;
mod exchange;
mod hosts;
mod lookup;
mod system;

use std::{path::Path, sync::Arc};

use lio::api::{self, Io};

pub use config::{DNS_PORT, ResolverConfig};
pub use error::Error;
pub use hickory_proto as proto;
pub use hosts::Hosts;
pub use lookup::{Lookup, LookupIp};
pub use system::LoadSystem;

use proto::rr::RecordType;

/// Looks names up.
///
/// Cloning it is cheap: the clones share one configuration.
#[derive(Clone, Debug)]
pub struct Resolver {
  config: Arc<ResolverConfig>,
}

impl Resolver {
  /// A resolver with this configuration.
  pub fn new(config: ResolverConfig) -> Self {
    Self { config: Arc::new(config) }
  }

  /// The system's resolver, from `/etc/resolv.conf` and `/etc/hosts`.
  ///
  /// Fails if `/etc/resolv.conf` cannot be read. A missing `/etc/hosts`
  /// leaves the host table empty.
  pub fn system() -> Io<LoadSystem> {
    Self::load("/etc/resolv.conf", "/etc/hosts")
  }

  /// A resolver from a `resolv.conf(5)` and a `hosts(5)` file, as
  /// [`system`](Self::system) reads them.
  pub fn load(
    resolv_conf: impl AsRef<Path>,
    hosts: impl AsRef<Path>,
  ) -> Io<LoadSystem> {
    api::run(LoadSystem::new(resolv_conf.as_ref(), hosts.as_ref()))
  }

  /// How this resolver looks names up.
  pub fn config(&self) -> &ResolverConfig {
    &self.config
  }

  /// The records of `record_type` for `name`.
  ///
  /// The result is the whole answer; its
  /// [`answers`](proto::op::Message::answers) hold the records, with any
  /// `CNAME` records that led to them.
  pub fn lookup(&self, name: &str, record_type: RecordType) -> Io<Lookup> {
    api::run(Lookup::new(Arc::clone(&self.config), name, record_type))
  }

  /// The addresses of `host`: IPv4 first, then IPv6.
  pub fn lookup_ip(&self, host: &str) -> Io<LookupIp> {
    api::run(LookupIp::new(Arc::clone(&self.config), host))
  }
}
