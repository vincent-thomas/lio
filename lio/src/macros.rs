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

/// Lets an [`Io`](crate::api::Io) run a built-in poll-shaped model.
///
/// `model_io!(Type)`, or `model_io!(generic [T: Bound] Type<T>)`.
macro_rules! model_io {
  (generic [$($generics:tt)*] $ty:ty) => {
    model_io!(@impl [$($generics)*] $ty);
  };
  ($ty:ty) => {
    model_io!(@impl [] $ty);
  };
  (@impl [$($generics:tt)*] $ty:ty) => {
    impl<$($generics)*> $crate::api::io::sealed::Sealed for $ty {}

    impl<$($generics)*> $crate::api::io::Runnable for $ty {
      type Item = <$ty as $crate::model::OpModel>::Item;
      type Future = $crate::model::Run<$ty>;

      fn run_callback<F>(self, lio: $crate::Lio, f: F)
      where
        F: Fn(Self::Item) + 'static,
      {
        // The task is left running: its operations keep it alive.
        let _task = $crate::model::spawn(&lio, self, f);
      }

      fn run_future(self, lio: $crate::Lio) -> Self::Future {
        $crate::model::run(&lio, self)
      }
    }
  };
}

/// Declares the public type of one operation around the memory it uses.
///
/// The memory type has two functions, `action` and `complete`: what the
/// operation submits, and how its result is read. The generated type is an
/// [`OpModel`](crate::model::OpModel) that pairs them, and an
/// [`Io`](crate::api::Io) of it is registered with the driver as it is.
macro_rules! leaf {
  (
    $(#[$meta:meta])*
    [$($generics:tt)*] $vis:vis struct $name:ident $(<$param:ident>)? ($raw:ty)
      => $item:ty
  ) => {
    $(#[$meta])*
    $vis struct $name<$($generics)*> {
      leaf: $crate::model::Leaf<$raw, $item>,
    }

    impl<$($generics)*> From<$raw> for $name$(<$param>)? {
      #[inline]
      fn from(raw: $raw) -> Self {
        Self { leaf: $crate::model::Leaf::Idle(raw) }
      }
    }

    impl<$($generics)*> $crate::model::OpModel for $name$(<$param>)? {
      type Item = $item;

      #[inline]
      fn poll(
        &mut self,
        cx: &mut $crate::model::Cx<'_>,
      ) -> $crate::model::Step<Self::Item> {
        self.leaf.poll(cx, <$raw>::action, <$raw>::complete)
      }
    }

    impl<$($generics)*> $crate::api::io::sealed::Sealed for $name$(<$param>)? {}

    impl<$($generics)*> $crate::api::io::Runnable for $name$(<$param>)? {
      type Item = $item;
      type Future = $crate::api::io::OpFuture<Self>;

      #[inline]
      fn run_callback<F>(self, lio: $crate::Lio, f: F)
      where
        F: Fn(Self::Item) + 'static,
      {
        let state = $crate::api::io::unsubmitted(self.leaf.into_idle());
        // Over capacity, the operation waits in a queue (see `Lio::is_full`).
        lio.schedule_or_queue(move |arena| {
          $crate::registration::Registration::new_in(
            arena,
            state,
            <$raw>::action,
            <$raw>::complete,
            f,
          )
        });
      }

      #[inline]
      fn run_future(self, lio: $crate::Lio) -> Self::Future {
        $crate::api::io::OpFuture::new(self, lio)
      }
    }

    impl<$($generics)*> $crate::api::io::sealed::Direct for $name$(<$param>)? {
      #[inline]
      fn start<F>(self, lio: &$crate::Lio, f: F) -> u64
      where
        F: Fn(Self::Item) + 'static,
      {
        let state = $crate::api::io::unsubmitted(self.leaf.into_idle());
        let scheduled = lio.schedule_with(move |arena| {
          $crate::registration::Registration::new_in(
            arena,
            state,
            <$raw>::action,
            <$raw>::complete,
            f,
          )
        });
        match scheduled {
          Ok(id) => id,
          Err(err) => {
            unreachable!("lio rejected an operation it had capacity for: {err}")
          }
        }
      }
    }
  };
}
