fn main() {
  cfg_aliases::cfg_aliases! {
      linux: { target_os = "linux" },
      macos: { target_os = "macos" },
      apple: { target_vendor = "apple" },
      kqueue: { any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
      )
    }
  }
}
