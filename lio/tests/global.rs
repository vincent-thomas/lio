//! The thread-local global `Lio`.

use lio::{Lio, install_global, uninstall_global};

#[test]
fn guard_uninstalls_the_lio_it_installed() {
  let guard = install_global(Lio::new(4).unwrap());
  drop(guard);
  assert!(uninstall_global().is_none());
}

#[test]
fn stale_guard_leaves_a_newer_global_installed() {
  let first = install_global(Lio::new(4).unwrap());
  // The first driver is uninstalled by hand and another takes its place.
  assert!(uninstall_global().is_some());
  let second = install_global(Lio::new(4).unwrap());

  drop(first);
  assert!(uninstall_global().is_some(), "the second driver is still installed");
  drop(second);
}
