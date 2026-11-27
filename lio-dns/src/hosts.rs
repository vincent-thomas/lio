//! The static host table, as `/etc/hosts` writes it.

use std::{collections::HashMap, net::IpAddr};

/// Names with fixed addresses, consulted before any nameserver.
///
/// Names match without regard to case or a trailing dot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hosts {
  by_name: HashMap<String, Vec<IpAddr>>,
}

impl Hosts {
  /// An empty table.
  pub fn new() -> Self {
    Self::default()
  }

  /// Parses the `hosts(5)` format: an address, then its names, one entry a
  /// line, with `#` starting a comment.
  ///
  /// Lines whose first field is not an address are skipped, as the C
  /// library skips them.
  pub fn parse(text: &str) -> Self {
    let mut hosts = Self::new();
    for line in text.lines() {
      let line = match line.split_once('#') {
        Some((before, _comment)) => before,
        None => line,
      };
      let mut fields = line.split_whitespace();
      let Some(addr) = fields.next().and_then(|field| field.parse().ok())
      else {
        continue;
      };
      for name in fields {
        hosts.insert(name, addr);
      }
    }
    hosts
  }

  /// Adds `addr` to the addresses of `name`.
  pub fn insert(&mut self, name: &str, addr: IpAddr) {
    let addrs = self.by_name.entry(key(name)).or_default();
    if !addrs.contains(&addr) {
      addrs.push(addr);
    }
  }

  /// The addresses of `name`, in the order the table lists them.
  pub fn get(&self, name: &str) -> Option<&[IpAddr]> {
    self.by_name.get(&key(name)).map(Vec::as_slice)
  }
}

fn key(name: &str) -> String {
  name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
  use super::Hosts;
  use std::net::IpAddr;

  fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
  }

  #[test]
  fn parses_entries_aliases_and_comments() {
    let hosts = Hosts::parse(
      "# comment\n\
       127.0.0.1\tlocalhost  loopback # trailing\n\
       ::1 localhost\n\
       not-an-ip ignored\n\
       \n\
       10.0.0.1 Box.Example.\n",
    );
    assert_eq!(hosts.get("localhost"), Some(&[ip("127.0.0.1"), ip("::1")][..]));
    assert_eq!(hosts.get("loopback"), Some(&[ip("127.0.0.1")][..]));
    assert_eq!(hosts.get("box.example"), Some(&[ip("10.0.0.1")][..]));
    assert_eq!(hosts.get("BOX.EXAMPLE."), Some(&[ip("10.0.0.1")][..]));
    assert_eq!(hosts.get("ignored"), None);
    assert_eq!(hosts.get("trailing"), None);
  }

  #[test]
  fn a_repeated_address_is_listed_once() {
    let hosts = Hosts::parse("127.0.0.1 a\n127.0.0.1 a\n");
    assert_eq!(hosts.get("a"), Some(&[ip("127.0.0.1")][..]));
  }
}
