//! Calls on `wasi:filesystem/types@0.3.2`.
//!
//! Each operation value holds a call's arguments and its result in the
//! canonical-ABI layout the host reads and writes. Nothing is allocated to
//! make a call: paths are passed by pointer, and the result lands in the
//! operation value. Make the call with
//! [`WaitableSet::start`](crate::WaitableSet::start); once it has returned,
//! at once or by [`Event::Returned`](crate::Event::Returned), take the
//! result with its `result` method.
//!
//! Calls that do not wait are not here: use `wasip3` directly for
//! `read_via_stream`, `write_via_stream`, `append_via_stream`,
//! `read_directory` and `filesystem::preopens`, and drop a [`Descriptor`] to
//! close it. A directory is listed through the stream `read_directory`
//! returns: see [`DirEntryBuf`].
//!
//! The layouts are written by hand to match what `wit-bindgen` generates for
//! the same functions. The assertions next to each one pin them for
//! `wasm32`.

use core::marker::PhantomData;

use wasip3::clocks::system_clock::Instant;
pub use wasip3::filesystem::types::{
  Advice, Descriptor, DescriptorFlags, DescriptorStat, DescriptorType,
  DirectoryEntry, ErrorCode, NewTimestamp, OpenFlags, PathFlags,
};
use wasip3::wit_bindgen::FutureReader;
use wasip3::wit_future::FuturePayload;

use crate::reply::{
  LiftError, LiftOk, NO_RESULT, ResultFuture, Slot, Words, call,
};
use crate::sealed::{self, FutureStart};
use crate::sys::wasm_imports;
use crate::{Call, ReadableFuture};

wasm_imports!("wasi:filesystem/types@0.3.2" {
  "[async-lower][method]descriptor.sync"
    fn sync(file: i32, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.sync-data"
    fn sync_data(file: i32, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.get-flags"
    fn get_flags(file: i32, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.get-type"
    fn get_type(file: i32, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.set-size"
    fn set_size(file: i32, size: i64, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.advise"
    fn advise(file: i32, offset: i64, length: i64, advice: i32, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.stat"
    fn stat(file: i32, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.stat-at"
    fn stat_at(dir: i32, path_flags: i32, path: *const u8, len: usize, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.create-directory-at"
    fn create_directory_at(dir: i32, path: *const u8, len: usize, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.remove-directory-at"
    fn remove_directory_at(dir: i32, path: *const u8, len: usize, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.unlink-file-at"
    fn unlink_file_at(dir: i32, path: *const u8, len: usize, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.readlink-at"
    fn readlink_at(dir: i32, path: *const u8, len: usize, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.open-at"
    fn open_at(params: *const u8, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.rename-at"
    fn rename_at(params: *const u8, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.link-at"
    fn link_at(params: *const u8, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.symlink-at"
    fn symlink_at(params: *const u8, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.set-times"
    fn set_times(params: *const u8, reply: *mut u8) -> i32;
  "[async-lower][method]descriptor.set-times-at"
    fn set_times_at(params: *const u8, reply: *mut u8) -> i32;
});

impl LiftError for ErrorCode {
  fn plain(case: u8) -> Option<Self> {
    Some(match case {
      0 => ErrorCode::Access,
      1 => ErrorCode::Already,
      2 => ErrorCode::BadDescriptor,
      3 => ErrorCode::Busy,
      4 => ErrorCode::Deadlock,
      5 => ErrorCode::Quota,
      6 => ErrorCode::Exist,
      7 => ErrorCode::FileTooLarge,
      8 => ErrorCode::IllegalByteSequence,
      9 => ErrorCode::InProgress,
      10 => ErrorCode::Interrupted,
      11 => ErrorCode::Invalid,
      12 => ErrorCode::Io,
      13 => ErrorCode::IsDirectory,
      14 => ErrorCode::Loop,
      15 => ErrorCode::TooManyLinks,
      16 => ErrorCode::MessageSize,
      17 => ErrorCode::NameTooLong,
      18 => ErrorCode::NoDevice,
      19 => ErrorCode::NoEntry,
      20 => ErrorCode::NoLock,
      21 => ErrorCode::InsufficientMemory,
      22 => ErrorCode::InsufficientSpace,
      23 => ErrorCode::NotDirectory,
      24 => ErrorCode::NotEmpty,
      25 => ErrorCode::NotRecoverable,
      26 => ErrorCode::Unsupported,
      27 => ErrorCode::NoTty,
      28 => ErrorCode::NoSuchDevice,
      29 => ErrorCode::Overflow,
      30 => ErrorCode::NotPermitted,
      31 => ErrorCode::Pipe,
      32 => ErrorCode::ReadOnly,
      33 => ErrorCode::InvalidSeek,
      34 => ErrorCode::TextFileBusy,
      35 => ErrorCode::CrossDevice,
      36 => return None,
      other => unreachable!("unknown error-code case {other}"),
    })
  }

  fn other(message: Option<String>) -> Self {
    ErrorCode::Other(message)
  }
}

impl LiftOk for Descriptor {
  unsafe fn lift(words: &Words) -> Self {
    // SAFETY: the host returned an owned descriptor handle in the first
    // word, and it is adopted once. A handle is 32 bits.
    unsafe { Descriptor::from_handle(words.a as u32) }
  }
}

impl LiftOk for DescriptorFlags {
  unsafe fn lift(words: &Words) -> Self {
    DescriptorFlags::from_bits_retain(words.case())
  }
}

impl LiftOk for DescriptorType {
  unsafe fn lift(words: &Words) -> Self {
    match words.case() {
      0 => DescriptorType::BlockDevice,
      1 => DescriptorType::CharacterDevice,
      2 => DescriptorType::Directory,
      3 => DescriptorType::Fifo,
      4 => DescriptorType::SymbolicLink,
      5 => DescriptorType::RegularFile,
      6 => DescriptorType::Socket,
      // SAFETY: the case with a payload carries an optional name.
      7 => DescriptorType::Other(unsafe { words.message() }),
      other => unreachable!("unknown descriptor-type case {other}"),
    }
  }
}

/// A descriptor argument: its handle, as the ABI passes it.
fn handle(descriptor: &Descriptor) -> i32 {
  descriptor.handle().cast_signed()
}

// ───────────────────────────── whole-file calls ─────────────────────────────

/// Flushes a file's data and metadata to storage, like `fsync`.
pub struct Sync<'a> {
  file: i32,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> Sync<'a> {
  pub fn new(file: &'a Descriptor) -> Self {
    Self { file: handle(file), reply: Slot::new(), borrows: PhantomData }
  }
}

call!(Sync => (), |op, reply| imports::sync(op.file, reply));

/// Flushes a file's data to storage, like `fdatasync`.
pub struct SyncData<'a> {
  file: i32,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> SyncData<'a> {
  pub fn new(file: &'a Descriptor) -> Self {
    Self { file: handle(file), reply: Slot::new(), borrows: PhantomData }
  }
}

call!(SyncData => (), |op, reply| imports::sync_data(op.file, reply));

/// Reads the flags a descriptor was opened with.
pub struct GetFlags<'a> {
  file: i32,
  reply: Slot<DescriptorFlags, ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> GetFlags<'a> {
  pub fn new(file: &'a Descriptor) -> Self {
    Self { file: handle(file), reply: Slot::new(), borrows: PhantomData }
  }
}

call!(GetFlags => DescriptorFlags, |op, reply| {
  imports::get_flags(op.file, reply)
});

/// Reads what kind of object a descriptor refers to.
pub struct GetType<'a> {
  file: i32,
  reply: Slot<DescriptorType, ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> GetType<'a> {
  pub fn new(file: &'a Descriptor) -> Self {
    Self { file: handle(file), reply: Slot::new(), borrows: PhantomData }
  }
}

call!(GetType => DescriptorType, |op, reply| {
  imports::get_type(op.file, reply)
});

/// Sets a file's size, truncating it or extending it with zeros, like
/// `ftruncate`.
pub struct SetSize<'a> {
  file: i32,
  size: u64,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> SetSize<'a> {
  pub fn new(file: &'a Descriptor, size: u64) -> Self {
    Self { file: handle(file), size, reply: Slot::new(), borrows: PhantomData }
  }
}

call!(SetSize => (), |op, reply| {
  imports::set_size(op.file, op.size.cast_signed(), reply)
});

/// Tells the host how a range of a file will be accessed, like
/// `posix_fadvise`.
pub struct Advise<'a> {
  file: i32,
  offset: u64,
  length: u64,
  advice: Advice,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> Advise<'a> {
  pub fn new(
    file: &'a Descriptor,
    offset: u64,
    length: u64,
    advice: Advice,
  ) -> Self {
    Self {
      file: handle(file),
      offset,
      length,
      advice,
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(Advise => (), |op, reply| {
  imports::advise(
    op.file,
    op.offset.cast_signed(),
    op.length.cast_signed(),
    op.advice as i32,
    reply,
  )
});

// ──────────────────────────── one-path calls ────────────────────────────────

/// Creates a directory, like `mkdirat`.
pub struct CreateDirectoryAt<'a> {
  dir: i32,
  path: &'a str,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> CreateDirectoryAt<'a> {
  pub fn new(dir: &'a Descriptor, path: &'a str) -> Self {
    Self { dir: handle(dir), path, reply: Slot::new(), borrows: PhantomData }
  }
}

call!(CreateDirectoryAt => (), |op, reply| {
  imports::create_directory_at(op.dir, op.path.as_ptr(), op.path.len(), reply)
});

/// Removes an empty directory, like `unlinkat` with `AT_REMOVEDIR`.
pub struct RemoveDirectoryAt<'a> {
  dir: i32,
  path: &'a str,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> RemoveDirectoryAt<'a> {
  pub fn new(dir: &'a Descriptor, path: &'a str) -> Self {
    Self { dir: handle(dir), path, reply: Slot::new(), borrows: PhantomData }
  }
}

call!(RemoveDirectoryAt => (), |op, reply| {
  imports::remove_directory_at(op.dir, op.path.as_ptr(), op.path.len(), reply)
});

/// Removes a file or symbolic link, like `unlinkat`.
pub struct UnlinkFileAt<'a> {
  dir: i32,
  path: &'a str,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> UnlinkFileAt<'a> {
  pub fn new(dir: &'a Descriptor, path: &'a str) -> Self {
    Self { dir: handle(dir), path, reply: Slot::new(), borrows: PhantomData }
  }
}

call!(UnlinkFileAt => (), |op, reply| {
  imports::unlink_file_at(op.dir, op.path.as_ptr(), op.path.len(), reply)
});

/// Reads the target of a symbolic link, like `readlinkat`.
///
/// The host allocates the target string in this component.
pub struct ReadlinkAt<'a> {
  dir: i32,
  path: &'a str,
  reply: Slot<String, ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> ReadlinkAt<'a> {
  pub fn new(dir: &'a Descriptor, path: &'a str) -> Self {
    Self { dir: handle(dir), path, reply: Slot::new(), borrows: PhantomData }
  }
}

call!(ReadlinkAt => String, |op, reply| {
  imports::readlink_at(op.dir, op.path.as_ptr(), op.path.len(), reply)
});

// ───────────────────────── calls with argument blocks ───────────────────────
//
// A call with more than four flat arguments passes a pointer to a block
// holding all of them.

/// Arguments of `descriptor.open-at`.
#[repr(C)]
struct OpenAtArgs {
  dir: i32,
  path_flags: u8,
  path: *const u8,
  path_len: usize,
  open_flags: u8,
  flags: u8,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<OpenAtArgs>() == 20);
  assert!(core::mem::offset_of!(OpenAtArgs, path_flags) == 4);
  assert!(core::mem::offset_of!(OpenAtArgs, path) == 8);
  assert!(core::mem::offset_of!(OpenAtArgs, path_len) == 12);
  assert!(core::mem::offset_of!(OpenAtArgs, open_flags) == 16);
  assert!(core::mem::offset_of!(OpenAtArgs, flags) == 17);
};

/// Opens a path relative to a directory, like `openat`.
///
/// ```rust,ignore
/// let mut open = OpenAt::new(
///   &dir,
///   PathFlags::empty(),
///   "data.txt",
///   OpenFlags::empty(),
///   DescriptorFlags::READ,
/// );
/// // SAFETY: `open` stays put until the call has returned, just below.
/// if let Some(subtask) = unsafe { set.start(&mut open) } {
///   assert_eq!(set.wait(), Some(Event::Returned(subtask)));
/// }
/// let file = open.result();
/// ```
pub struct OpenAt<'a> {
  args: OpenAtArgs,
  reply: Slot<Descriptor, ErrorCode>,
  borrows: PhantomData<(&'a Descriptor, &'a str)>,
}

impl<'a> OpenAt<'a> {
  pub fn new(
    dir: &'a Descriptor,
    path_flags: PathFlags,
    path: &'a str,
    open_flags: OpenFlags,
    flags: DescriptorFlags,
  ) -> Self {
    Self {
      args: OpenAtArgs {
        dir: handle(dir),
        path_flags: path_flags.bits(),
        path: path.as_ptr(),
        path_len: path.len(),
        open_flags: open_flags.bits(),
        flags: flags.bits(),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(OpenAt => Descriptor, |op, reply| {
  imports::open_at((&raw const op.args).cast(), reply)
});

/// Arguments of `descriptor.rename-at`.
#[repr(C)]
struct RenameAtArgs {
  old_dir: i32,
  old_path: *const u8,
  old_len: usize,
  new_dir: i32,
  new_path: *const u8,
  new_len: usize,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<RenameAtArgs>() == 24);
  assert!(core::mem::offset_of!(RenameAtArgs, old_path) == 4);
  assert!(core::mem::offset_of!(RenameAtArgs, new_dir) == 12);
  assert!(core::mem::offset_of!(RenameAtArgs, new_path) == 16);
};

/// Renames a file or directory, like `renameat`.
pub struct RenameAt<'a> {
  args: RenameAtArgs,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<(&'a Descriptor, &'a str)>,
}

impl<'a> RenameAt<'a> {
  pub fn new(
    old_dir: &'a Descriptor,
    old_path: &'a str,
    new_dir: &'a Descriptor,
    new_path: &'a str,
  ) -> Self {
    Self {
      args: RenameAtArgs {
        old_dir: handle(old_dir),
        old_path: old_path.as_ptr(),
        old_len: old_path.len(),
        new_dir: handle(new_dir),
        new_path: new_path.as_ptr(),
        new_len: new_path.len(),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(RenameAt => (), |op, reply| {
  imports::rename_at((&raw const op.args).cast(), reply)
});

/// Arguments of `descriptor.link-at`.
#[repr(C)]
struct LinkAtArgs {
  old_dir: i32,
  old_path_flags: u8,
  old_path: *const u8,
  old_len: usize,
  new_dir: i32,
  new_path: *const u8,
  new_len: usize,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<LinkAtArgs>() == 28);
  assert!(core::mem::offset_of!(LinkAtArgs, old_path_flags) == 4);
  assert!(core::mem::offset_of!(LinkAtArgs, old_path) == 8);
  assert!(core::mem::offset_of!(LinkAtArgs, new_dir) == 16);
  assert!(core::mem::offset_of!(LinkAtArgs, new_path) == 20);
};

/// Creates a hard link, like `linkat`.
pub struct LinkAt<'a> {
  args: LinkAtArgs,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<(&'a Descriptor, &'a str)>,
}

impl<'a> LinkAt<'a> {
  pub fn new(
    old_dir: &'a Descriptor,
    old_path_flags: PathFlags,
    old_path: &'a str,
    new_dir: &'a Descriptor,
    new_path: &'a str,
  ) -> Self {
    Self {
      args: LinkAtArgs {
        old_dir: handle(old_dir),
        old_path_flags: old_path_flags.bits(),
        old_path: old_path.as_ptr(),
        old_len: old_path.len(),
        new_dir: handle(new_dir),
        new_path: new_path.as_ptr(),
        new_len: new_path.len(),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(LinkAt => (), |op, reply| {
  imports::link_at((&raw const op.args).cast(), reply)
});

/// Arguments of `descriptor.symlink-at`.
#[repr(C)]
struct SymlinkAtArgs {
  dir: i32,
  target: *const u8,
  target_len: usize,
  path: *const u8,
  path_len: usize,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<SymlinkAtArgs>() == 20);
  assert!(core::mem::offset_of!(SymlinkAtArgs, target) == 4);
  assert!(core::mem::offset_of!(SymlinkAtArgs, path) == 12);
};

/// Creates a symbolic link at `path` whose contents are `target`, like
/// `symlinkat`.
pub struct SymlinkAt<'a> {
  args: SymlinkAtArgs,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<(&'a Descriptor, &'a str)>,
}

impl<'a> SymlinkAt<'a> {
  pub fn new(dir: &'a Descriptor, target: &'a str, path: &'a str) -> Self {
    Self {
      args: SymlinkAtArgs {
        dir: handle(dir),
        target: target.as_ptr(),
        target_len: target.len(),
        path: path.as_ptr(),
        path_len: path.len(),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(SymlinkAt => (), |op, reply| {
  imports::symlink_at((&raw const op.args).cast(), reply)
});

/// `new-timestamp` as an argument.
#[repr(C)]
struct TimestampArg {
  /// `0` leaves the time alone, `1` sets it to now, `2` sets it to the
  /// instant that follows.
  tag: u8,
  seconds: i64,
  nanoseconds: u32,
}

const _: () = {
  assert!(size_of::<TimestampArg>() == 24);
  assert!(core::mem::offset_of!(TimestampArg, seconds) == 8);
  assert!(core::mem::offset_of!(TimestampArg, nanoseconds) == 16);
};

impl From<NewTimestamp> for TimestampArg {
  fn from(time: NewTimestamp) -> Self {
    match time {
      NewTimestamp::NoChange => Self { tag: 0, seconds: 0, nanoseconds: 0 },
      NewTimestamp::Now => Self { tag: 1, seconds: 0, nanoseconds: 0 },
      NewTimestamp::Timestamp(Instant { seconds, nanoseconds }) => {
        Self { tag: 2, seconds, nanoseconds }
      }
    }
  }
}

/// Arguments of `descriptor.set-times`.
#[repr(C)]
struct SetTimesArgs {
  file: i32,
  accessed: TimestampArg,
  modified: TimestampArg,
}

const _: () = {
  assert!(size_of::<SetTimesArgs>() == 56);
  assert!(core::mem::offset_of!(SetTimesArgs, accessed) == 8);
  assert!(core::mem::offset_of!(SetTimesArgs, modified) == 32);
};

/// Sets a file's access and modification times, like `futimens`.
pub struct SetTimes<'a> {
  args: SetTimesArgs,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> SetTimes<'a> {
  pub fn new(
    file: &'a Descriptor,
    accessed: NewTimestamp,
    modified: NewTimestamp,
  ) -> Self {
    Self {
      args: SetTimesArgs {
        file: handle(file),
        accessed: accessed.into(),
        modified: modified.into(),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(SetTimes => (), |op, reply| {
  imports::set_times((&raw const op.args).cast(), reply)
});

/// Arguments of `descriptor.set-times-at`.
#[repr(C)]
struct SetTimesAtArgs {
  dir: i32,
  path_flags: u8,
  path: *const u8,
  path_len: usize,
  accessed: TimestampArg,
  modified: TimestampArg,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<SetTimesAtArgs>() == 64);
  assert!(core::mem::offset_of!(SetTimesAtArgs, path_flags) == 4);
  assert!(core::mem::offset_of!(SetTimesAtArgs, path) == 8);
  assert!(core::mem::offset_of!(SetTimesAtArgs, accessed) == 16);
  assert!(core::mem::offset_of!(SetTimesAtArgs, modified) == 40);
};

/// Sets the access and modification times of a path, like `utimensat`.
pub struct SetTimesAt<'a> {
  args: SetTimesAtArgs,
  reply: Slot<(), ErrorCode>,
  borrows: PhantomData<(&'a Descriptor, &'a str)>,
}

impl<'a> SetTimesAt<'a> {
  pub fn new(
    dir: &'a Descriptor,
    path_flags: PathFlags,
    path: &'a str,
    accessed: NewTimestamp,
    modified: NewTimestamp,
  ) -> Self {
    Self {
      args: SetTimesAtArgs {
        dir: handle(dir),
        path_flags: path_flags.bits(),
        path: path.as_ptr(),
        path_len: path.len(),
        accessed: accessed.into(),
        modified: modified.into(),
      },
      reply: Slot::new(),
      borrows: PhantomData,
    }
  }
}

call!(SetTimesAt => (), |op, reply| {
  imports::set_times_at((&raw const op.args).cast(), reply)
});

// ─────────────────────────────────── stat ───────────────────────────────────

/// `option<instant>` in a stat result.
#[repr(C)]
struct OptionalInstant {
  tag: u8,
  seconds: i64,
  nanoseconds: u32,
}

impl OptionalInstant {
  const fn none() -> Self {
    Self { tag: 0, seconds: 0, nanoseconds: 0 }
  }

  fn lift(&self) -> Option<Instant> {
    match self.tag {
      0 => None,
      1 => {
        Some(Instant { seconds: self.seconds, nanoseconds: self.nanoseconds })
      }
      other => unreachable!("unknown option tag {other:#x}"),
    }
  }
}

/// `result<descriptor-stat, error-code>` as the host writes it. It holds
/// 64-bit fields, so its payload starts at offset 8.
#[repr(C)]
struct StatReply {
  /// `0` for ok, `1` for error, [`NO_RESULT`] otherwise.
  tag: u8,
  pad: [u8; 7],
  /// Ok: the descriptor type. Error: the error code.
  words: Words,
  link_count: u64,
  size: u64,
  accessed: OptionalInstant,
  modified: OptionalInstant,
  changed: OptionalInstant,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<StatReply>() == 112);
  assert!(align_of::<StatReply>() == 8);
  assert!(core::mem::offset_of!(StatReply, words) == 8);
  assert!(core::mem::offset_of!(StatReply, link_count) == 24);
  assert!(core::mem::offset_of!(StatReply, size) == 32);
  assert!(core::mem::offset_of!(StatReply, accessed) == 40);
  assert!(core::mem::offset_of!(StatReply, modified) == 64);
  assert!(core::mem::offset_of!(StatReply, changed) == 88);
};

/// Storage for one stat result.
struct StatSlot {
  /// The host writes through a pointer into this.
  reply: core::cell::UnsafeCell<StatReply>,
}

impl StatSlot {
  const fn new() -> Self {
    Self {
      reply: core::cell::UnsafeCell::new(StatReply {
        tag: NO_RESULT,
        pad: [0; 7],
        words: Words::zeroed(),
        link_count: 0,
        size: 0,
        accessed: OptionalInstant::none(),
        modified: OptionalInstant::none(),
        changed: OptionalInstant::none(),
      }),
    }
  }

  /// Releases any result still here and returns where the host writes the
  /// next one.
  fn arm(&mut self) -> *mut u8 {
    drop(self.take());
    self.reply.get().cast()
  }

  fn take(&mut self) -> Option<Result<DescriptorStat, ErrorCode>> {
    let reply = self.reply.get_mut();
    match core::mem::replace(&mut reply.tag, NO_RESULT) {
      NO_RESULT => None,
      0 => Some(Ok(DescriptorStat {
        // SAFETY: the host wrote a descriptor type, and the tag was
        // cleared, so it is lifted once.
        type_: unsafe { DescriptorType::lift(&reply.words) },
        link_count: reply.link_count,
        size: reply.size,
        data_access_timestamp: reply.accessed.lift(),
        data_modification_timestamp: reply.modified.lift(),
        status_change_timestamp: reply.changed.lift(),
      })),
      // SAFETY: as above, for the error.
      1 => Some(Err(unsafe { reply.words.error() })),
      other => unreachable!("unknown result tag {other:#x}"),
    }
  }
}

impl Drop for StatSlot {
  fn drop(&mut self) {
    // An untaken result may own a string. Release it.
    drop(self.take());
  }
}

/// Reads a file's metadata, like `fstat`.
pub struct Stat<'a> {
  file: i32,
  reply: StatSlot,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> Stat<'a> {
  pub fn new(file: &'a Descriptor) -> Self {
    Self { file: handle(file), reply: StatSlot::new(), borrows: PhantomData }
  }
}

call!(Stat => DescriptorStat, |op, reply| imports::stat(op.file, reply));

/// Reads the metadata of a path, like `fstatat`.
pub struct StatAt<'a> {
  dir: i32,
  path_flags: PathFlags,
  path: &'a str,
  reply: StatSlot,
  borrows: PhantomData<&'a Descriptor>,
}

impl<'a> StatAt<'a> {
  pub fn new(
    dir: &'a Descriptor,
    path_flags: PathFlags,
    path: &'a str,
  ) -> Self {
    Self {
      dir: handle(dir),
      path_flags,
      path,
      reply: StatSlot::new(),
      borrows: PhantomData,
    }
  }
}

call!(StatAt => DescriptorStat, |op, reply| {
  imports::stat_at(
    op.dir,
    i32::from(op.path_flags.bits()),
    op.path.as_ptr(),
    op.path.len(),
    reply,
  )
});

// ───────────────────────────── directory listing ────────────────────────────

/// The stream of a directory's entries, owned by a
/// [`WaitableSet`](crate::WaitableSet).
///
/// Returned by
/// [`WaitableSet::add_dir_reader`](crate::WaitableSet::add_dir_reader). It
/// stays valid until
/// [`WaitableSet::close_dir_reader`](crate::WaitableSet::close_dir_reader)
/// or until the set is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DirStream(pub(crate) u32);

/// Stored in a slot's type tag while the host has not written an entry
/// there. A real `descriptor-type` case is below it.
const NO_ENTRY: usize = 0xff;

/// `directory-entry` as the host writes it: the entry's type, then its
/// name.
#[repr(C)]
struct RawDirEntry {
  kind: Words,
  name: *mut u8,
  name_len: usize,
}

#[cfg(target_pointer_width = "32")]
const _: () = {
  assert!(size_of::<RawDirEntry>() == 24);
  assert!(align_of::<RawDirEntry>() == 4);
  assert!(core::mem::offset_of!(RawDirEntry, name) == 16);
  assert!(core::mem::offset_of!(RawDirEntry, name_len) == 20);
};

impl RawDirEntry {
  const fn empty() -> Self {
    Self {
      kind: Words { a: NO_ENTRY, ..Words::zeroed() },
      name: core::ptr::null_mut(),
      name_len: 0,
    }
  }

  /// Takes the entry out, leaving the slot empty.
  fn take(&mut self) -> Option<DirectoryEntry> {
    if self.kind.case() as usize == NO_ENTRY {
      return None;
    }
    let raw = core::mem::replace(self, Self::empty());
    Some(DirectoryEntry {
      // SAFETY: the host wrote a descriptor type here, and the slot was
      // emptied, so it is lifted once.
      type_: unsafe { DescriptorType::lift(&raw.kind) },
      // SAFETY: the host allocated the name in this component and handed
      // it over; as above, it is adopted once.
      name: unsafe { crate::reply::adopt_string(raw.name, raw.name_len) },
    })
  }
}

/// Where directory entries land.
///
/// Pass it to [`WaitableSet::read_dir`](crate::WaitableSet::read_dir). The
/// read's result counts the entries that arrived, and
/// [`drain`](Self::drain) hands them over. Entries for `.` and `..` are not
/// among them.
///
/// The host allocates each name in this component. Entries left in the
/// buffer are freed when it is dropped or read into again.
pub struct DirEntryBuf {
  /// The host writes entries through a pointer into this.
  slots: core::cell::UnsafeCell<Box<[RawDirEntry]>>,
}

impl DirEntryBuf {
  /// A buffer for up to `capacity` entries per read.
  pub fn with_capacity(capacity: usize) -> Self {
    let slots = (0..capacity).map(|_| RawDirEntry::empty()).collect();
    Self { slots: core::cell::UnsafeCell::new(slots) }
  }

  /// How many entries one read can deliver.
  pub fn capacity(&mut self) -> usize {
    self.slots.get_mut().len()
  }

  /// Takes the entries the last read delivered, in order.
  pub fn drain(&mut self) -> impl Iterator<Item = DirectoryEntry> + '_ {
    // The host fills slots from the front, so the first empty one ends
    // them.
    self.slots.get_mut().iter_mut().map_while(RawDirEntry::take)
  }

  /// Frees what the last read left and returns where the host writes the
  /// next entries, and how many fit.
  pub(crate) fn arm(&mut self) -> (*mut u8, usize) {
    let slots = self.slots.get_mut();
    for slot in slots.iter_mut() {
      drop(slot.take());
    }
    (slots.as_mut_ptr().cast(), slots.len())
  }
}

impl Drop for DirEntryBuf {
  fn drop(&mut self) {
    // Untaken entries own their names. Release them.
    self.arm();
  }
}

// ────────────────────────────── stream status ───────────────────────────────

/// How a file stream ended: `Ok` at the end of the data, or the error that
/// cut it short.
///
/// `read_via_stream`, `write_via_stream` and `append_via_stream` return it
/// next to the stream as a future. A stream alone only reports
/// [`StreamResult::Dropped`](crate::StreamResult::Dropped), for an error as
/// for the end of the file. Read this with
/// [`WaitableSet::read_future`](crate::WaitableSet::read_future) to learn
/// which it was. It is delivered once the stream has ended.
///
/// # Panics
///
/// Reading it again after it was delivered panics: a status is delivered
/// once.
pub struct StreamStatus {
  future: ResultFuture<ErrorCode>,
}

impl StreamStatus {
  pub fn new(result: FutureReader<Result<(), ErrorCode>>) -> Self {
    let vtable = <Result<(), ErrorCode> as FuturePayload>::VTABLE;
    Self { future: ResultFuture::new(result, vtable) }
  }

  /// Takes the status.
  ///
  /// Returns `None` if it has not arrived, was cancelled, or was already
  /// taken.
  pub fn result(&mut self) -> Option<Result<(), ErrorCode>> {
    self.future.take()
  }
}

impl ReadableFuture for StreamStatus {}

impl sealed::Future for StreamStatus {
  unsafe fn start_read(&mut self) -> FutureStart {
    // SAFETY: the caller keeps `self` in place until the read completes.
    unsafe { self.future.start() }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn words(a: usize) -> Words {
    Words { a, ..Words::zeroed() }
  }

  #[test]
  fn error_cases_are_in_wit_order() {
    assert!(matches!(ErrorCode::plain(0), Some(ErrorCode::Access)));
    assert!(matches!(ErrorCode::plain(19), Some(ErrorCode::NoEntry)));
    assert!(matches!(ErrorCode::plain(35), Some(ErrorCode::CrossDevice)));
    assert!(ErrorCode::plain(36).is_none());
  }

  #[test]
  fn descriptor_types_are_in_wit_order() {
    // SAFETY: plain cases read nothing but the case byte.
    unsafe {
      assert!(matches!(
        DescriptorType::lift(&words(0)),
        DescriptorType::BlockDevice
      ));
      assert!(matches!(
        DescriptorType::lift(&words(2)),
        DescriptorType::Directory
      ));
      assert!(matches!(
        DescriptorType::lift(&words(5)),
        DescriptorType::RegularFile
      ));
      assert!(matches!(
        DescriptorType::lift(&words(7)),
        DescriptorType::Other(None)
      ));
    }
  }

  /// Writes an entry as the host would: an exact allocation for the name.
  fn host_entry(slot: &mut RawDirEntry, kind: usize, name: &str) {
    let name = String::from(name).into_boxed_str();
    slot.name_len = name.len();
    slot.name = Box::into_raw(name).cast();
    slot.kind = words(kind);
  }

  #[test]
  fn a_new_entry_buffer_is_empty() {
    let mut buf = DirEntryBuf::with_capacity(4);
    assert_eq!(buf.capacity(), 4);
    assert_eq!(buf.drain().count(), 0);
  }

  #[test]
  fn entries_drain_in_order_and_only_once() {
    let mut buf = DirEntryBuf::with_capacity(4);
    {
      let slots = buf.slots.get_mut();
      host_entry(&mut slots[0], 5, "file.txt");
      host_entry(&mut slots[1], 2, "sub");
    }
    let entries: Vec<DirectoryEntry> = buf.drain().collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "file.txt");
    assert!(matches!(entries[0].type_, DescriptorType::RegularFile));
    assert_eq!(entries[1].name, "sub");
    assert!(matches!(entries[1].type_, DescriptorType::Directory));
    assert_eq!(buf.drain().count(), 0);
  }

  #[test]
  fn arming_frees_entries_that_were_not_taken() {
    let mut buf = DirEntryBuf::with_capacity(2);
    host_entry(&mut buf.slots.get_mut()[0], 5, "left behind");
    let (_, capacity) = buf.arm();
    assert_eq!(capacity, 2);
    assert_eq!(buf.drain().count(), 0);
  }

  #[test]
  fn dropping_frees_entries_that_were_not_taken() {
    let mut buf = DirEntryBuf::with_capacity(2);
    host_entry(&mut buf.slots.get_mut()[0], 5, "left behind");
    drop(buf);
  }

  #[test]
  fn flags_come_from_the_low_byte() {
    // SAFETY: flags read nothing but the case byte.
    let flags = unsafe { DescriptorFlags::lift(&words(0xff00 | 0b11)) };
    assert_eq!(flags, DescriptorFlags::READ | DescriptorFlags::WRITE);
  }

  #[test]
  fn a_timestamp_argument_carries_its_case() {
    let now: TimestampArg = NewTimestamp::Now.into();
    assert_eq!(now.tag, 1);
    let at: TimestampArg =
      NewTimestamp::Timestamp(Instant { seconds: 5, nanoseconds: 9 }).into();
    assert_eq!((at.tag, at.seconds, at.nanoseconds), (2, 5, 9));
    let keep: TimestampArg = NewTimestamp::NoChange.into();
    assert_eq!(keep.tag, 0);
  }

  #[test]
  fn a_stat_result_is_lifted_field_by_field() {
    let mut slot = StatSlot::new();
    {
      let reply = slot.reply.get_mut();
      reply.tag = 0;
      reply.words = words(5);
      reply.link_count = 2;
      reply.size = 4096;
      reply.modified = OptionalInstant { tag: 1, seconds: 7, nanoseconds: 3 };
    }
    let Some(Ok(stat)) = slot.take() else {
      panic!("expected a stat");
    };
    assert!(matches!(stat.type_, DescriptorType::RegularFile));
    assert_eq!((stat.link_count, stat.size), (2, 4096));
    assert!(stat.data_access_timestamp.is_none());
    let Some(modified) = stat.data_modification_timestamp else {
      panic!("expected a modification time");
    };
    assert_eq!((modified.seconds, modified.nanoseconds), (7, 3));
    assert!(slot.take().is_none());
  }

  #[test]
  fn a_stat_error_is_lifted() {
    let mut slot = StatSlot::new();
    {
      let reply = slot.reply.get_mut();
      reply.tag = 1;
      reply.words = words(19);
    }
    assert!(matches!(slot.take(), Some(Err(ErrorCode::NoEntry))));
  }
}
