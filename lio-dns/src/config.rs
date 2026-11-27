//! What a [`Resolver`](crate::Resolver) asks, and whom.

use std::{
  net::{IpAddr, SocketAddr},
  time::Duration,
};

use hickory_proto::rr::Name;

use crate::Hosts;

/// The port nameservers listen on.
pub const DNS_PORT: u16 = 53;

/// How a [`Resolver`](crate::Resolver) looks names up.
///
/// The defaults are those of `resolv.conf(5)`.
#[derive(Clone, Debug)]
pub struct ResolverConfig {
  /// The nameservers, tried in order.
  pub nameservers: Vec<SocketAddr>,
  /// Domains appended to a name that is not fully qualified.
  pub search: Vec<Name>,
  /// A name with at least this many dots is tried as it is before the
  /// search domains are appended; one with fewer, after.
  pub ndots: u8,
  /// How long one nameserver gets to answer one query.
  pub timeout: Duration,
  /// How many times every nameserver is tried, for one name.
  pub attempts: u8,
  /// Names answered without asking a nameserver.
  pub hosts: Hosts,
}

impl ResolverConfig {
  /// Asks `nameservers`, with no search domains and no host table.
  pub fn new(nameservers: Vec<SocketAddr>) -> Self {
    Self {
      nameservers,
      search: Vec::new(),
      ndots: 1,
      timeout: Duration::from_secs(5),
      attempts: 2,
      hosts: Hosts::new(),
    }
  }

  /// Reads the `resolv.conf(5)` format.
  ///
  /// Unknown or malformed lines are skipped, as the C library skips them.
  /// With no `nameserver` line the local host is asked, as it is there.
  /// A link-local IPv6 nameserver loses its `%interface` scope.
  pub fn from_resolv_conf(text: &str) -> Self {
    let (conf, _errors) =
      resolv_conf::Config::parse_with_errors(text.as_bytes());
    let nameservers = conf
      .get_nameservers_or_local()
      .into_iter()
      .map(|ip| {
        let ip = match ip {
          resolv_conf::ScopedIp::V4(v4) => IpAddr::V4(v4),
          resolv_conf::ScopedIp::V6(v6, _scope) => IpAddr::V6(v6),
        };
        SocketAddr::new(ip, DNS_PORT)
      })
      .collect();
    let search = conf
      .get_last_search_or_domain()
      .filter_map(|domain| Name::from_ascii(domain).ok())
      .map(|mut name| {
        name.set_fqdn(true);
        name
      })
      .collect();
    Self {
      nameservers,
      search,
      ndots: u8::try_from(conf.ndots).unwrap_or(u8::MAX),
      timeout: Duration::from_secs(u64::from(conf.timeout.max(1))),
      attempts: u8::try_from(conf.attempts.max(1)).unwrap_or(u8::MAX),
      hosts: Hosts::new(),
    }
  }

  /// The fully qualified names to ask for, in order, when asked for
  /// `name`.
  pub(crate) fn candidates(&self, name: &Name) -> Vec<Name> {
    if name.is_fqdn() {
      return vec![name.clone()];
    }
    let mut as_is = name.clone();
    as_is.set_fqdn(true);
    let searched = self
      .search
      .iter()
      .filter_map(|domain| name.clone().append_domain(domain).ok());
    // The root label does not count as a dot.
    let dots = name.num_labels().saturating_sub(1);
    if dots >= self.ndots {
      std::iter::once(as_is).chain(searched).collect()
    } else {
      searched.chain(std::iter::once(as_is)).collect()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{DNS_PORT, ResolverConfig};
  use hickory_proto::rr::Name;
  use std::{net::SocketAddr, time::Duration};

  fn names(list: &[Name]) -> Vec<String> {
    list.iter().map(Name::to_ascii).collect()
  }

  #[test]
  fn reads_nameservers_search_and_options() {
    let config = ResolverConfig::from_resolv_conf(
      "# generated\n\
       nameserver 10.0.0.53\n\
       nameserver 2001:db8::1\n\
       search corp.example example.org\n\
       options ndots:2 timeout:3 attempts:4 rotate\n\
       bogus line here\n",
    );
    assert_eq!(
      config.nameservers,
      vec![
        SocketAddr::new("10.0.0.53".parse().unwrap(), DNS_PORT),
        SocketAddr::new("2001:db8::1".parse().unwrap(), DNS_PORT),
      ]
    );
    assert_eq!(names(&config.search), ["corp.example.", "example.org."]);
    assert_eq!(config.ndots, 2);
    assert_eq!(config.timeout, Duration::from_secs(3));
    assert_eq!(config.attempts, 4);
  }

  #[test]
  fn with_no_nameserver_the_local_host_is_asked() {
    let config = ResolverConfig::from_resolv_conf("search example\n");
    assert!(!config.nameservers.is_empty());
    assert!(config.nameservers.iter().all(|addr| addr.ip().is_loopback()));
  }

  #[test]
  fn a_later_domain_line_replaces_search() {
    let config =
      ResolverConfig::from_resolv_conf("search a.example\ndomain b.example\n");
    assert_eq!(names(&config.search), ["b.example."]);
  }

  #[test]
  fn candidates_follow_ndots() {
    let mut config = ResolverConfig::new(Vec::new());
    config.search = vec![Name::from_ascii("corp.example.").unwrap()];

    let short = Name::from_ascii("db").unwrap();
    assert_eq!(names(&config.candidates(&short)), ["db.corp.example.", "db."]);

    let dotted = Name::from_ascii("db.internal").unwrap();
    assert_eq!(
      names(&config.candidates(&dotted)),
      ["db.internal.", "db.internal.corp.example."]
    );

    config.ndots = 2;
    assert_eq!(
      names(&config.candidates(&dotted)),
      ["db.internal.corp.example.", "db.internal."]
    );
  }

  #[test]
  fn a_fully_qualified_name_skips_the_search_list() {
    let mut config = ResolverConfig::new(Vec::new());
    config.search = vec![Name::from_ascii("corp.example.").unwrap()];
    let fqdn = Name::from_ascii("db.").unwrap();
    assert_eq!(names(&config.candidates(&fqdn)), ["db."]);
  }
}
