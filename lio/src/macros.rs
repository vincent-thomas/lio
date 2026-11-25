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

/// Builds an [`Action::Io`](crate::api::op::Action::Io) from an [`Op`](crate::backend::op::Op)
/// whose addresses the surrounding model owns.
///
/// Crate-private. [`BorrowedOp::from_op`](crate::backend::op::BorrowedOp::from_op)
/// brands the operation `'static`. A public macro would let a downstream
/// model clone an [`Op`](crate::backend::op::Op) and submit stack memory
/// after `action` returns. External models build a [`BorrowedOp`](crate::backend::op::BorrowedOp)
/// with the safe constructors and convert it with [`From`].
///
/// The operation is bound outside the `unsafe` block. The driver keeps the
/// model pinned until the backend finishes the submission.
macro_rules! action_io {
  ($op:expr) => {{
    let op = $op;
    // SAFETY: `op` points only at memory owned by the model that builds this
    // action, or it borrows nothing. The driver keeps that model pinned, and
    // does not move those buffers, until the backend reports the operation
    // finished.
    $crate::api::op::Action::Io(unsafe {
      $crate::backend::op::BorrowedOp::from_op(op)
    })
  }};
}

/// Declares the type of one operation: what it was asked to do, held
/// until it is polled, and the [`Cx`](crate::model::Cx) method that then
/// starts it.
///
/// The generated type is an [`OpModel`](crate::model::OpModel) whose `poll`
/// calls that method.
macro_rules! leaf {
  (
    $(#[$meta:meta])*
    [$($generics:tt)*] $vis:vis struct $name:ident $(<$param:ident>)? (
      $($arg:ident : $ty:ty),* $(,)?
    ) -> $item:ty = cx.$method:ident
  ) => {
    $(#[$meta])*
    $vis struct $name<$($generics)*> {
      state: $crate::model::Leaf<($($ty,)*), $item>,
    }

    impl<$($generics)*> $name$(<$param>)? {
      #[inline]
      pub(crate) fn new($($arg: $ty),*) -> Self {
        Self { state: $crate::model::Leaf::Idle(($($arg,)*)) }
      }
    }

    impl<$($generics)*> $crate::model::OpModel for $name$(<$param>)? {
      type Item = $item;

      #[inline]
      fn poll(
        &mut self,
        cx: &mut $crate::model::Cx<'_>,
      ) -> $crate::model::Step<Self::Item> {
        self.state.poll(cx, |cx, ($($arg,)*)| cx.$method($($arg),*))
      }
    }
  };
}
