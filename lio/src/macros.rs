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
  ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
      #[allow(unused_unsafe)]
      let res = unsafe { libc::$fn($($arg, )*) };
      if res == -1 {
          Err(std::io::Error::last_os_error())
      } else {
          Ok(res)
      }
  }};
}

/// Raw syscall wrapper with early return on error.
///
/// Returns the syscall result as `isize` on success, or returns early with `-errno`.
/// Works with the `?` operator by implementing a custom `Try` conversion.
///
/// # Returns
///
/// - On success: Continues execution with result as `isize` (>= 0)
/// - On error: Returns `-errno` from the enclosing function
///
/// # Platform-specific behavior
///
/// - Unix: Returns `-errno` on error
/// - Windows: Returns `-GetLastError()` on error
///
/// # Example
///
/// ```ignore
/// fn do_io(fd: i32) -> isize {
///     // Chain multiple syscalls - returns -errno on first error
///     let n = syscall_raw!(read(fd, buf.as_mut_ptr() as *mut libc::c_void, len));
///     let m = syscall_raw!(write(fd, data.as_ptr() as *const libc::c_void, data.len()));
///     n + m  // Returns sum if all succeed
/// }
///
/// let result = do_io(fd);
/// if result < 0 {
///     let errno = -result;
///     eprintln!("Error: errno {}", errno);
/// }
/// ```
macro_rules! syscall_raw {
  ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
      #[allow(unused_unsafe)]
      let res = unsafe { libc::$fn($($arg, )*) };
      if res == -1 {
          // Return negative errno - this will cause early return from function
          #[cfg(target_os = "linux")]
          {
              let errno = unsafe { *libc::__errno_location() };
              return -(errno as isize);
          }
          #[cfg(target_os = "macos")]
          {
              let errno = unsafe { *libc::__error() };
              return -(errno as isize);
          }
          #[cfg(target_os = "freebsd")]
          {
              let errno = unsafe { *libc::__error() };
              return -(errno as isize);
          }
          #[cfg(windows)]
          {
              let last_error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
              return -(last_error as isize);
          }
      }
      res as isize
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
    impl_result!(|_this, res: isize| -> std::io::Result<crate::resource::Resource> {
      if res < 0 {
        Err(std::io::Error::from_raw_os_error((-res) as i32))
      } else {
        Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(res as i32) })
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
    fn meta(&self) -> crate::op::OpMeta {
      crate::op::OpMeta::CAP_NONE
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
