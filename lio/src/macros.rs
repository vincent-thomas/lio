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

/// Gives a model type the ways to run it: `.await`, `when_done`, `send`,
/// `send_with`, and `with_lio` to name the driver.
///
/// Every operation and model lio provides has these. Use this for a model
/// of your own, so it is run the same way:
///
/// ```
/// use lio::api;
/// use lio::backend::testing::ScriptedBackend;
/// use lio::model::{Cx, OpModel, Step};
/// use lio::ready;
///
/// struct Twice(api::ops::Nop, bool);
///
/// impl OpModel for Twice {
///   type Item = std::io::Result<()>;
///
///   fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
///     loop {
///       if let Err(err) = ready!(self.0.poll(cx)) {
///         return Step::Done(Err(err));
///       }
///       if std::mem::replace(&mut self.1, true) {
///         return Step::Done(Ok(()));
///       }
///       self.0 = api::nop();
///     }
///   }
/// }
///
/// lio::runnable!(Twice);
///
/// let lio = lio::Lio::new_with_backend(ScriptedBackend::auto(0), 4).unwrap();
/// Twice(api::nop(), false).with_lio(&lio).when_done(|result| result.unwrap());
/// while lio.try_run().unwrap() > 0 {}
/// ```
///
/// A type with parameters lists them first:
/// `lio::runnable!([B: IoBuf] WriteAll<B>)`.
///
/// It is a macro because Rust cannot implement `.await` for every
/// [`OpModel`](crate::model::OpModel) at once.
#[macro_export]
macro_rules! runnable {
  ([$($generics:tt)*] $ty:ty) => {
    impl<$($generics)*> $ty
    where
      Self: $crate::model::OpModel + 'static,
    {
      /// Runs this on `lio` instead of the driver installed for the thread.
      #[inline]
      pub fn with_lio(self, lio: &$crate::Lio) -> $crate::api::Io<Self> {
        $crate::api::Io::from_op(self).with_lio(lio)
      }

      /// Runs this and calls `f` with the result. See
      /// [`Io::when_done`]($crate::api::Io::when_done).
      #[inline]
      pub fn when_done<F>(self, f: F)
      where
        F: Fn(<Self as $crate::model::OpModel>::Item) + 'static,
      {
        $crate::api::Io::from_op(self).when_done(f)
      }

      /// Runs this and sends the result through a channel. See
      /// [`Io::send`]($crate::api::Io::send).
      #[inline]
      pub fn send(
        self,
      ) -> $crate::api::Receiver<<Self as $crate::model::OpModel>::Item>
      where
        <Self as $crate::model::OpModel>::Item: ::std::marker::Send + 'static,
      {
        $crate::api::Io::from_op(self).send()
      }

      /// Runs this and sends the result through `sender`. See
      /// [`Io::send_with`]($crate::api::Io::send_with).
      #[inline]
      pub fn send_with(
        self,
        sender: ::std::sync::mpsc::Sender<
          <Self as $crate::model::OpModel>::Item,
        >,
      ) where
        <Self as $crate::model::OpModel>::Item: ::std::marker::Send + 'static,
      {
        $crate::api::Io::from_op(self).send_with(sender)
      }
    }

    impl<$($generics)*> ::std::future::IntoFuture for $ty
    where
      Self: $crate::model::OpModel + ::std::marker::Unpin + 'static,
    {
      type Output = <Self as $crate::model::OpModel>::Item;
      type IntoFuture = $crate::model::Run<Self>;

      #[inline]
      fn into_future(self) -> Self::IntoFuture {
        $crate::api::Io::from_op(self).into_future()
      }
    }
  };
  ($ty:ty) => {
    $crate::runnable!([] $ty);
  };
}

/// Gives a stream type the ways to run it: `start` for a handle to take
/// items from, `send` and `send_with` for a channel, and `with_lio` to name
/// the driver.
///
/// Every stream lio provides has these. Use this for an
/// [`OpStream`](crate::model::OpStream) of your own:
/// `lio::streamable!(Chunks)`, or `lio::streamable!([B: IoBuf] Chunks<B>)`.
#[macro_export]
macro_rules! streamable {
  ([$($generics:tt)*] $ty:ty) => {
    impl<$($generics)*> $ty
    where
      Self: $crate::model::OpStream + 'static,
    {
      /// Runs this on `lio` instead of the driver installed for the thread.
      #[inline]
      pub fn with_lio(self, lio: &$crate::Lio) -> $crate::api::IoStream<Self> {
        $crate::api::IoStream::from_op(self).with_lio(lio)
      }

      /// Starts this, and gives the handle its items are taken from. See
      /// [`IoStream::start`]($crate::api::IoStream::start).
      #[inline]
      pub fn start(self) -> $crate::api::Items<Self> {
        $crate::api::IoStream::from_op(self).start()
      }

      /// Runs this and sends every item through a channel. See
      /// [`IoStream::send`]($crate::api::IoStream::send).
      #[inline]
      pub fn send(
        self,
      ) -> $crate::api::StreamReceiver<<Self as $crate::model::OpStream>::Item>
      where
        <Self as $crate::model::OpStream>::Item: ::std::marker::Send,
      {
        $crate::api::IoStream::from_op(self).send()
      }

      /// Runs this and sends every item through `sender`. See
      /// [`IoStream::send_with`]($crate::api::IoStream::send_with).
      #[inline]
      pub fn send_with(
        self,
        sender: ::std::sync::mpsc::Sender<
          <Self as $crate::model::OpStream>::Item,
        >,
      ) where
        <Self as $crate::model::OpStream>::Item: ::std::marker::Send,
      {
        $crate::api::IoStream::from_op(self).send_with(sender)
      }
    }
  };
  ($ty:ty) => {
    $crate::streamable!([] $ty);
  };
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
    [$($generics:tt)*] pub struct $name:ident $(<$param:ident>)? (
      $($args:tt)*
    ) -> $item:ty = cx.$method:ident
  ) => {
    leaf!(
      @model $(#[$meta])*
      [$($generics)*] (pub) struct $name $(<$param>)? ($($args)*)
        -> $item = cx.$method
    );
    // Only lio's public operations are run by users.
    runnable!([$($generics)*] $name$(<$param>)?);
  };
  (
    $(#[$meta:meta])*
    [$($generics:tt)*] struct $name:ident $(<$param:ident>)? (
      $($args:tt)*
    ) -> $item:ty = cx.$method:ident
  ) => {
    leaf!(
      @model $(#[$meta])*
      [$($generics)*] () struct $name $(<$param>)? ($($args)*)
        -> $item = cx.$method
    );
  };
  (
    @model $(#[$meta:meta])*
    [$($generics:tt)*] ($($vis:tt)*) struct $name:ident $(<$param:ident>)? (
      $($arg:ident : $ty:ty),* $(,)?
    ) -> $item:ty = cx.$method:ident
  ) => {
    $(#[$meta])*
    #[must_use = "an operation does nothing until it is run or polled"]
    $($vis)* struct $name<$($generics)*> {
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
