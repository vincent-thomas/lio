#![cfg(feature = "backend_impls")]

mod backends {
  #[cfg(target_os = "linux")]
  mod io_uring;
  #[cfg(windows)]
  mod iocp;
  #[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  mod poller;
}
