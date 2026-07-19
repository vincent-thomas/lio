/// Generates documentation for operations with automatic syscall reference links.
///
/// This macro handles consistent documentation generation for lio operations,
/// automatically adding syscall documentation links when specified.
///
/// # Syntax
///
/// ```ignore
/// doc_op! {
///     // Optional: short one-sentence description
///     short: "Brief description.",
///
///     // Optional: syscall reference
///     syscall: "name(section)",
///
///     // Optional: custom documentation link (makes syscall a clickable link)
///     doc_link: "https://custom.url/path",
///
///     // Your additional documentation and function definition
///     /// More detailed documentation here
///     pub fn function_name(...) -> Result {
///         // implementation
///     }
/// }
/// ```
///
/// # Examples
///
/// With short description, syscall, and link:
/// ```ignore
/// doc_op! {
///     short: "Shuts down part of a full-duplex connection.",
///     syscall: "shutdown(2)",
///     doc_link: "https://man7.org/linux/man-pages/man2/shutdown.2.html",
///
///     /// # Examples
///     /// ```
///     /// // example code
///     /// ```
///     pub fn shutdown(fd: RawFd, how: i32) -> io::Result<()> {
///         // implementation
///     }
/// }
/// // Generates:
/// // "Shuts down part of a full-duplex connection."
/// //
/// // "Equivalent to the [`shutdown(2)`](https://...) syscall."
/// //
/// // "# Examples ..."
/// ```
///
/// With short and syscall (no link):
/// ```ignore
/// doc_op! {
///     short: "Reads data from a file descriptor.",
///     syscall: "read(2)",
///
///     pub fn read(fd: RawFd) -> io::Result<usize> {
///         // implementation
///     }
/// }
/// // Generates:
/// // "Reads data from a file descriptor."
/// //
/// // "Equivalent to the `read(2)` syscall."
/// ```
///
/// With short only (no syscall):
/// ```ignore
/// doc_op! {
///     short: "Waits for a specified duration.",
///
///     pub fn timeout(duration: Duration) -> io::Result<()> {
///         // implementation
///     }
/// }
/// // Generates: "Waits for a specified duration."
/// ```
///
/// No short, no syscall (passthrough):
/// ```ignore
/// doc_op! {
///     /// Custom documentation here.
///     pub fn custom_fn() -> io::Result<()> {
///         // implementation
///     }
/// }
/// // Generates: Just the user's docs
/// ```
macro_rules! doc_op {
    // Variant 1: With short description, syscall, and doc link
    (
        short: $short:literal,
        syscall: $syscall:literal,
        doc_link: $url:literal,

        $($rest:tt)*
    ) => {
        #[doc = $short]
        #[doc = concat!("\n\nEquivalent to the [`", $syscall, "`](", $url, ") syscall.\n")]
        #[inline]
        $($rest)*
    };

    // Variant 2: With short description and syscall (no link)
    (
        short: $short:literal,
        syscall: $syscall:literal,

        $($rest:tt)*
    ) => {
        #[doc = $short]
        #[doc = concat!("\n\nEquivalent to the `", $syscall, "` syscall.\n")]
        #[inline]
        $($rest)*
    };

    // Variant 3: With short description only (no syscall)
    (
        short: $short:literal,

        $($rest:tt)*
    ) => {
        #[doc = $short]
        #[inline]
        $($rest)*
    };

    // Variant 4: With syscall and doc link (no short)
    (
        syscall: $syscall:literal,
        doc_link: $url:literal,

        $($rest:tt)*
    ) => {
        #[doc = concat!("\n\nEquivalent to the [`", $syscall, "`](", $url, ") syscall.\n")]
        $($rest)*
    };

    // Variant 5: With syscall only (no link, no short)
    (
        syscall: $syscall:literal,

        $($rest:tt)*
    ) => {
        #[doc = concat!("\n\nEquivalent to the `", $syscall, "` syscall.\n")]
        $($rest)*
    };

    // Variant 6: No syscall, no short - just pass through
    (
        $($rest:tt)*
    ) => {
        $($rest)*
    };
}

macro_rules! syscall {
  (raw $fn: ident ( $($arg: expr),* $(,)* ) ? ) => {{
      let val = syscall!(raw $fn ($($arg),*));

      if val < 0 {
          return val;
      }

      val
  }};
  (raw $fn: ident ( $($arg: expr),* $(,)* ) ) => {{
    #[allow(unused_unsafe)]
    {
      // SAFETY: This exists on the platform in cfg.
      let res = unsafe { libc::$fn($($arg, )*) };
      if res != -1 {
        res as isize
      }
      else {
        // Return negative errno - this will cause early return from function.
        let err = $crate::platform::errno::last_os_error_code();

        -(err as isize)
      }
    }
  }};

  ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
      let result = syscall!(raw $fn($($arg),*));
      if result >= 0 {
          Ok(result as i32)
      } else {
          Err(std::io::Error::from_raw_os_error(-(result as i32)))
      }
  }};
}
