//! Calls on `wasi:sockets/ip-name-lookup@0.3.2`.

use std::alloc::Layout;

pub use wasip3::sockets::ip_name_lookup::ErrorCode;
pub use wasip3::sockets::types::IpAddress;

use crate::Call;
use crate::reply::{LiftError, LiftOk, Slot, Words, call};
use crate::sealed;
use crate::sys::wasm_imports;

wasm_imports!("wasi:sockets/ip-name-lookup@0.3.2" {
  "[async-lower]resolve-addresses"
    fn resolve_addresses(name: *const u8, len: usize, reply: *mut u8) -> i32;
});

impl LiftError for ErrorCode {
  fn plain(case: u8) -> Option<Self> {
    Some(match case {
      0 => ErrorCode::AccessDenied,
      1 => ErrorCode::InvalidArgument,
      2 => ErrorCode::NameUnresolvable,
      3 => ErrorCode::TemporaryResolverFailure,
      4 => ErrorCode::PermanentResolverFailure,
      5 => return None,
      other => unreachable!("unknown error-code case {other}"),
    })
  }

  fn other(message: Option<String>) -> Self {
    ErrorCode::Other(message)
  }
}

/// The size of one `ip-address` in a list: a case byte, then at offset 2
/// four bytes of IPv4 or eight little-endian 16-bit groups of IPv6.
const ADDRESS_SIZE: usize = 18;
const ADDRESS_ALIGN: usize = 2;

/// Lifts one `ip-address` as the host writes it.
fn lift_address(raw: &[u8; ADDRESS_SIZE]) -> IpAddress {
  match raw[0] {
    0 => IpAddress::Ipv4((raw[2], raw[3], raw[4], raw[5])),
    1 => {
      let group = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
      IpAddress::Ipv6((
        group(2),
        group(4),
        group(6),
        group(8),
        group(10),
        group(12),
        group(14),
        group(16),
      ))
    }
    other => unreachable!("unknown ip-address case {other}"),
  }
}

impl LiftOk for Vec<IpAddress> {
  unsafe fn lift(words: &Words) -> Self {
    // A list is its pointer then its length.
    let (ptr, len) = (words.a as *mut u8, words.b);
    if len == 0 {
      return Vec::new();
    }
    let addresses = (0..len)
      .map(|index| {
        // SAFETY: the host wrote `len` addresses of `ADDRESS_SIZE` bytes
        // each at `ptr`, and a byte array has no alignment to respect.
        lift_address(unsafe {
          &*ptr.add(index * ADDRESS_SIZE).cast::<[u8; ADDRESS_SIZE]>()
        })
      })
      .collect();
    let Ok(layout) = Layout::from_size_align(len * ADDRESS_SIZE, ADDRESS_ALIGN)
    else {
      unreachable!("the host allocated a list with this layout")
    };
    // SAFETY: the host allocated the list through this component's
    // allocator with this layout and handed it over. It is freed once.
    unsafe { std::alloc::dealloc(ptr, layout) };
    addresses
  }
}

/// Resolves a host name to its addresses, like `getaddrinfo`.
///
/// `name` is a host name or an address in text form. The host allocates
/// the list of addresses in this component.
pub struct ResolveAddresses<'a> {
  name: &'a str,
  reply: Slot<Vec<IpAddress>, ErrorCode>,
}

impl<'a> ResolveAddresses<'a> {
  pub fn new(name: &'a str) -> Self {
    Self { name, reply: Slot::new() }
  }
}

call!(ResolveAddresses => Vec<IpAddress>, |op, reply| {
  imports::resolve_addresses(op.name.as_ptr(), op.name.len(), reply)
});

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn error_cases_are_in_wit_order() {
    assert!(matches!(ErrorCode::plain(0), Some(ErrorCode::AccessDenied)));
    assert!(matches!(ErrorCode::plain(2), Some(ErrorCode::NameUnresolvable)));
    assert!(matches!(
      ErrorCode::plain(4),
      Some(ErrorCode::PermanentResolverFailure)
    ));
    assert!(ErrorCode::plain(5).is_none());
  }

  #[test]
  fn an_ipv4_address_is_four_bytes_after_the_case() {
    let mut raw = [0xee; ADDRESS_SIZE];
    raw[0] = 0;
    raw[2..6].copy_from_slice(&[127, 0, 0, 1]);
    assert!(matches!(lift_address(&raw), IpAddress::Ipv4((127, 0, 0, 1))));
  }

  #[test]
  fn an_ipv6_address_is_eight_little_endian_groups() {
    let mut raw = [0; ADDRESS_SIZE];
    raw[0] = 1;
    raw[2..4].copy_from_slice(&0x2001u16.to_le_bytes());
    raw[16..18].copy_from_slice(&1u16.to_le_bytes());
    assert!(matches!(
      lift_address(&raw),
      IpAddress::Ipv6((0x2001, 0, 0, 0, 0, 0, 0, 1))
    ));
  }

  #[test]
  fn an_empty_list_reads_no_pointer() {
    // SAFETY: a zero length means nothing was allocated.
    let addresses = unsafe { Vec::<IpAddress>::lift(&Words::zeroed()) };
    assert!(addresses.is_empty());
  }

  #[test]
  fn a_list_is_lifted_and_freed() {
    let layout = Layout::from_size_align(2 * ADDRESS_SIZE, ADDRESS_ALIGN);
    let layout = layout.unwrap();
    // SAFETY: the layout has a non-zero size, and the list is handed to
    // `lift`, which frees it, as the host would hand one over.
    let addresses = unsafe {
      let ptr = std::alloc::alloc_zeroed(layout);
      assert!(!ptr.is_null());
      ptr.add(2).copy_from([10u8, 0, 0, 7].as_ptr(), 4);
      ptr.add(ADDRESS_SIZE).write(1);
      Vec::<IpAddress>::lift(&Words {
        a: ptr as usize,
        b: 2,
        ..Words::zeroed()
      })
    };
    assert!(matches!(addresses[0], IpAddress::Ipv4((10, 0, 0, 7))));
    assert!(matches!(addresses[1], IpAddress::Ipv6((0, 0, 0, 0, 0, 0, 0, 0))));
  }
}
