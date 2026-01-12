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
        $($rest)*
    };

    // Variant 3: With short description only (no syscall)
    (
        short: $short:literal,

        $($rest:tt)*
    ) => {
        #[doc = $short]
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
        // Return negative errno - this will cause early return from function
        #[cfg(target_os = "linux")]
        {
          let errno = unsafe { *libc::__errno_location() };
          -(errno as isize)
        }
        #[cfg(any(target_os = "macos", target_os = "freebsd"))]
        {
          // SAFETY: This exists on the platform in cfg.
          let errno = unsafe { *libc::__error() };
          -(errno as isize)
        }
        #[cfg(windows)]
        {
          let last_error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
          -(last_error as isize)
        }
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

macro_rules! impl_result {
  (()) => {
      impl_result!(|_this, res: isize| -> std::io::Result<()> {
        if res < 0 {
          Err(std::io::Error::from_raw_os_error((-res) as i32))
        } else {
          Ok(())
        }
      });
  };

  (res) => {
    impl_result!(|_this, res: isize| -> std::io::Result<crate::api::resource::Resource> {
      if res < 0 {
        Err(std::io::Error::from_raw_os_error((-res) as i32))
      } else {
        // SAFETY: Caller guarrantees this is valid fd.
        let resource = unsafe {
          std::os::fd::FromRawFd::from_raw_fd(res as i32)
        };
        Ok(resource)
      }
    });
  };

  (|$this:ident, $res:ident: $res_ty:ty| -> $ret_ty:ty { $($body:tt)* }) => {
    fn result(&mut self, res: isize) -> *const () {
      let __impl_result = |$this: &mut Self, $res: $res_ty| -> $ret_ty { $($body)* };
      Box::into_raw(Box::new(__impl_result(self, res))) as *const ()
    }
  };
}

macro_rules! impl_no_readyness {
  () => {
    #[cfg(unix)]
    fn meta(&self) -> crate::operation::OpMeta {
      crate::operation::OpMeta::CAP_NONE
    }
  };
}

#[cfg(test)]
pub(crate) mod __internal {
  pub const MAX_OP_SIZE: usize = 144;
}

macro_rules! assert_op_max_size {
  ($op_type:ty) => {
    #[test]
    fn test_op_size() {
      // Inline storage size - change this one number to adjust limit for all operations

      let size = std::mem::size_of::<$op_type>();

      assert!(
        size <= crate::macros::__internal::MAX_OP_SIZE,
        concat!(
          "\n\n",
          "╔════════════════════════════════════════════════════════════╗\n",
          "║ OPERATION SIZE LIMIT EXCEEDED                              ║\n",
          "╠════════════════════════════════════════════════════════════╣\n",
          "║ Operation: ",
          stringify!($op_type),
          "\n",
          "║ Size: {} bytes\n",
          "║ Maximum allowed: {} bytes\n",
          "║\n",
          "║ This operation exceeds the bump allocator inline storage\n",
          "║ and will cause heap allocations.\n",
          "║\n",
          "║ To fix this:\n",
          "║  1. Reduce operation struct size (preferred), OR\n",
          "║  2. Increase MAX_OP_SIZE in macros.rs if intentional\n",
          "╚════════════════════════════════════════════════════════════╝\n"
        ),
        size,
        crate::macros::__internal::MAX_OP_SIZE
      );
    }
  };
}
