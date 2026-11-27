//! `wasi:filesystem`, on lio's file operations.

use std::collections::VecDeque;
use std::ffi::CString;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::task::{Context, Poll};

use lio::api::ops::{LinkKind, OpenAt, ReadDir};
use lio::api::resource::Resource;
use lio::api::{self, FileMode, FileType, ReadDirBuf, UnlinkKind};
use rustix::fs::{AtFlags, Timespec, Timestamps};
use wasmtime::StoreContextMut;
use wasmtime::component::{
  Access, Accessor, Destination, FutureReader, Resource as Handle,
  StreamProducer, StreamReader, StreamResult, VecBuffer,
};

use crate::bindings::wasi::clocks::system_clock::Instant;
use crate::bindings::wasi::filesystem::preopens;
use crate::bindings::wasi::filesystem::types::{
  self, Advice, DescriptorFlags, DescriptorStat, DescriptorType,
  DirectoryEntry, ErrorCode, Filesize, MetadataHashValue, NewTimestamp,
  OpenFlags, PathFlags,
};
use crate::io::{Op, fs_error, op};
use crate::stream::{ByteReader, ByteWriter, ReadFrom, WriteTo};
use crate::{LioWasi, WasiCtx};

/// An open file or directory.
#[derive(Clone)]
pub struct Descriptor {
  resource: Resource,
  /// Whether this is a directory, when opening it said so. Otherwise the
  /// OS is asked the first time it matters, which for a file that is only
  /// read or written is never.
  dir: Option<bool>,
  flags: DescriptorFlags,
}

impl Descriptor {
  pub(crate) fn preopen(resource: Resource) -> Self {
    Self {
      resource,
      dir: Some(true),
      flags: DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
    }
  }

  fn is_dir(&self) -> bool {
    self.dir.unwrap_or_else(|| {
      rustix::fs::fstat(&self.resource).is_ok_and(|stat| {
        rustix::fs::FileType::from_raw_mode(stat.st_mode as _)
          == rustix::fs::FileType::Directory
      })
    })
  }

  /// The directory path operations resolve against.
  fn dir(&self) -> Result<&Resource, ErrorCode> {
    if self.is_dir() {
      Ok(&self.resource)
    } else {
      Err(ErrorCode::NotDirectory)
    }
  }

  /// The directory, for an operation that changes its entries.
  fn dir_mut(&self) -> Result<&Resource, ErrorCode> {
    let dir = self.dir()?;
    if self.flags.contains(DescriptorFlags::MUTATE_DIRECTORY) {
      Ok(dir)
    } else {
      Err(ErrorCode::NotPermitted)
    }
  }
}

type Fs<T> = wasmtime::Result<Result<T, ErrorCode>>;

fn get<T>(
  accessor: &Accessor<T, LioWasi>,
  fd: &Handle<Descriptor>,
) -> wasmtime::Result<Descriptor> {
  accessor.with(|mut store| Ok(store.get().table.get(fd)?.clone()))
}

/// A guest path as the C string lio takes, refused if it could leave the
/// directory it is relative to.
///
/// Every `..` is resolved here, against the names before it, and none
/// reaches the system. There, one that follows a symbolic link steps out of
/// wherever the link leads: with a link to `.`, `link/..` is the parent of
/// the directory itself.
fn path(path: &str) -> Result<CString, ErrorCode> {
  if path.is_empty() {
    return Err(ErrorCode::NoEntry);
  }
  if path.starts_with('/') {
    return Err(ErrorCode::NotPermitted);
  }
  let mut names: Vec<&str> = Vec::new();
  for part in path.split('/') {
    match part {
      "" | "." => {}
      ".." => {
        names.pop().ok_or(ErrorCode::NotPermitted)?;
      }
      name => names.push(name),
    }
  }
  let resolved =
    if names.is_empty() { String::from(".") } else { names.join("/") };
  CString::new(resolved).map_err(|_| ErrorCode::Invalid)
}

/// The text a symbolic link a guest makes may store: relative, and with no
/// `..`. The system resolves it, wherever the link is followed from, so
/// one with a `..` could leave the directory by way of another link.
fn link_target(target: &str) -> Result<CString, ErrorCode> {
  if target.is_empty() {
    return Err(ErrorCode::NoEntry);
  }
  if target.starts_with('/') || target.split('/').any(|part| part == "..") {
    return Err(ErrorCode::NotPermitted);
  }
  CString::new(target).map_err(|_| ErrorCode::Invalid)
}

fn mode(octal: u32) -> FileMode {
  match FileMode::from_octal(octal) {
    Some(mode) => mode,
    None => unreachable!("{octal:o} is permission bits only"),
  }
}

fn kind(file_type: FileType) -> DescriptorType {
  match file_type {
    FileType::File => DescriptorType::RegularFile,
    FileType::Directory => DescriptorType::Directory,
    FileType::Symlink => DescriptorType::SymbolicLink,
    FileType::BlockDevice => DescriptorType::BlockDevice,
    FileType::CharDevice => DescriptorType::CharacterDevice,
    FileType::Fifo => DescriptorType::Fifo,
    FileType::Socket => DescriptorType::Socket,
    FileType::Unknown => DescriptorType::Other(None),
  }
}

/// lio's stat carries no timestamps, which WASI's does, so `stat` and
/// `stat-at` ask the OS directly. It never waits.
fn stat(stat: rustix::fs::Stat) -> DescriptorStat {
  use rustix::fs::FileType as Type;
  let instant = |seconds: i64, nanoseconds: i64| {
    Some(Instant {
      seconds,
      nanoseconds: u32::try_from(nanoseconds).unwrap_or(0),
    })
  };
  DescriptorStat {
    type_: match Type::from_raw_mode(stat.st_mode as _) {
      Type::RegularFile => DescriptorType::RegularFile,
      Type::Directory => DescriptorType::Directory,
      Type::Symlink => DescriptorType::SymbolicLink,
      Type::BlockDevice => DescriptorType::BlockDevice,
      Type::CharacterDevice => DescriptorType::CharacterDevice,
      Type::Fifo => DescriptorType::Fifo,
      Type::Socket => DescriptorType::Socket,
      Type::Unknown => DescriptorType::Other(None),
    },
    link_count: stat.st_nlink as u64,
    size: u64::try_from(stat.st_size).unwrap_or(0),
    data_access_timestamp: instant(
      stat.st_atime as i64,
      stat.st_atime_nsec as i64,
    ),
    data_modification_timestamp: instant(
      stat.st_mtime as i64,
      stat.st_mtime_nsec as i64,
    ),
    status_change_timestamp: instant(
      stat.st_ctime as i64,
      stat.st_ctime_nsec as i64,
    ),
  }
}

fn timespec(time: NewTimestamp) -> Result<Timespec, ErrorCode> {
  Ok(match time {
    NewTimestamp::NoChange => {
      Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_OMIT as _ }
    }
    NewTimestamp::Now => {
      Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_NOW as _ }
    }
    NewTimestamp::Timestamp(at) => {
      if at.nanoseconds >= 1_000_000_000 {
        return Err(ErrorCode::Invalid);
      }
      Timespec { tv_sec: at.seconds, tv_nsec: at.nanoseconds as _ }
    }
  })
}

fn timestamps(
  access: NewTimestamp,
  modification: NewTimestamp,
) -> Result<Timestamps, ErrorCode> {
  Ok(Timestamps {
    last_access: timespec(access)?,
    last_modification: timespec(modification)?,
  })
}

fn errno(error: rustix::io::Errno) -> ErrorCode {
  fs_error(&std::io::Error::from(error))
}

fn hash(stat: &rustix::fs::Stat) -> MetadataHashValue {
  let part = |salt: u8| {
    let mut hasher = std::hash::DefaultHasher::new();
    salt.hash(&mut hasher);
    stat.st_dev.hash(&mut hasher);
    stat.st_ino.hash(&mut hasher);
    stat.st_size.hash(&mut hasher);
    stat.st_mtime.hash(&mut hasher);
    stat.st_mtime_nsec.hash(&mut hasher);
    hasher.finish()
  };
  MetadataHashValue { lower: part(0), upper: part(1) }
}

fn at_flags(flags: PathFlags) -> AtFlags {
  if flags.contains(PathFlags::SYMLINK_FOLLOW) {
    AtFlags::empty()
  } else {
    AtFlags::SYMLINK_NOFOLLOW
  }
}

fn nothing(_: &Resource) {}

// ─────────────────────────────── directories ────────────────────────────────

/// Entries asked of the OS per read, and the bytes their names may take.
const ENTRIES: usize = 256;
const NAME_BYTES: usize = 32 * 1024;

enum Listing {
  /// Opening the directory again, so that the listing has a position of its
  /// own.
  Opening(Op<OpenAt>),
  Idle(Resource, ReadDirBuf),
  Reading(Resource, Op<ReadDir>),
  Ended,
}

/// The host end of a directory's `stream<directory-entry>`.
struct DirReader {
  listing: Listing,
  /// Entries read and not yet given to the guest.
  ready: VecDeque<DirectoryEntry>,
  status: Option<crate::status::Sender<ErrorCode>>,
}

impl DirReader {
  fn end(&mut self, how: Result<(), ErrorCode>) {
    if let Some(status) = self.status.take() {
      status.send(how);
    }
  }

  /// Queues a batch. Returns whether the listing is over.
  fn take(&mut self, batch: &ReadDirBuf) -> Result<bool, ErrorCode> {
    let mut entries = 0;
    for entry in batch.iter() {
      entries += 1;
      if entry.name == b"." || entry.name == b".." {
        continue;
      }
      let Ok(name) = std::str::from_utf8(entry.name) else {
        return Err(ErrorCode::IllegalByteSequence);
      };
      self.ready.push_back(DirectoryEntry {
        type_: entry.file_type.map_or(DescriptorType::Other(None), kind),
        name: name.to_owned(),
      });
    }
    Ok(batch.result.eof || entries == 0)
  }
}

impl Drop for DirReader {
  fn drop(&mut self) {
    self.end(Ok(()));
  }
}

impl<D> StreamProducer<D> for DirReader {
  type Item = DirectoryEntry;
  type Buffer = VecBuffer<DirectoryEntry>;

  fn poll_produce<'a>(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    mut store: StoreContextMut<'a, D>,
    mut dst: Destination<'a, Self::Item, Self::Buffer>,
    finish: bool,
  ) -> Poll<wasmtime::Result<StreamResult>> {
    let me = self.get_mut();
    let wanted = dst.remaining(&mut store)?;
    // A read of nothing asks whether there is more: not once the listing
    // is over and everything read has been given out.
    if wanted == Some(0) {
      let over = matches!(me.listing, Listing::Ended) && me.ready.is_empty();
      return Poll::Ready(Ok(if over {
        StreamResult::Dropped
      } else {
        StreamResult::Completed
      }));
    }
    let waiting = |finish: bool| {
      if finish {
        Poll::Ready(Ok(StreamResult::Cancelled))
      } else {
        Poll::Pending
      }
    };
    loop {
      if !me.ready.is_empty() {
        let count = wanted.unwrap_or(ENTRIES).min(me.ready.len());
        let batch: Vec<DirectoryEntry> = me.ready.drain(..count).collect();
        dst.set_buffer(batch.into());
        return Poll::Ready(Ok(StreamResult::Completed));
      }
      match std::mem::replace(&mut me.listing, Listing::Ended) {
        Listing::Ended => return Poll::Ready(Ok(StreamResult::Dropped)),
        Listing::Opening(mut open) => match Pin::new(&mut open).poll(cx) {
          Poll::Pending => {
            me.listing = Listing::Opening(open);
            return waiting(finish);
          }
          Poll::Ready(Ok(dir)) => {
            let buf = ReadDirBuf::with_capacity(NAME_BYTES, ENTRIES);
            me.listing = Listing::Idle(dir, buf);
          }
          Poll::Ready(Err(error)) => me.end(Err(fs_error(&error))),
        },
        Listing::Idle(dir, buf) => {
          let read = op(api::readdir(&dir, buf));
          me.listing = Listing::Reading(dir, read);
        }
        Listing::Reading(dir, mut read) => match Pin::new(&mut read).poll(cx) {
          Poll::Pending => {
            me.listing = Listing::Reading(dir, read);
            return waiting(finish);
          }
          Poll::Ready(Ok(batch)) => match me.take(&batch) {
            Ok(true) => me.end(Ok(())),
            Ok(false) => me.listing = Listing::Idle(dir, batch),
            Err(error) => {
              me.ready.clear();
              me.end(Err(error));
            }
          },
          Poll::Ready(Err(error)) => me.end(Err(fs_error(&error))),
        },
      }
    }
  }
}

// ─────────────────────────────── the interface ──────────────────────────────

impl types::Host for WasiCtx {}

impl types::HostDescriptor for WasiCtx {
  fn drop(&mut self, rep: Handle<Descriptor>) -> wasmtime::Result<()> {
    self.table.delete(rep)?;
    Ok(())
  }
}

impl preopens::Host for WasiCtx {
  fn get_directories(
    &mut self,
  ) -> wasmtime::Result<Vec<(Handle<Descriptor>, String)>> {
    let mut directories = Vec::with_capacity(self.preopens.len());
    for (descriptor, name) in &self.preopens {
      directories.push((self.table.push(descriptor.clone())?, name.clone()));
    }
    Ok(directories)
  }
}

impl<T: Send + 'static> types::HostDescriptorWithStore<T> for LioWasi {
  fn read_via_stream(
    mut host: Access<T, Self>,
    fd: Handle<Descriptor>,
    offset: Filesize,
  ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)>
  {
    let descriptor = host.get().table.get(&fd)?.clone();
    // Reading a directory fails with `is-directory` in the stream's status.
    let readable = descriptor.flags.contains(DescriptorFlags::READ);
    let from = ReadFrom::File(descriptor.resource, offset);
    let lending = host.get().lending.clone();
    let (mut reader, status) =
      ByteReader::new(from, fs_error, nothing, lending);
    // The system opened it for reading whether or not the guest asked to
    // read it, so the guest's own flags are what say.
    if !readable {
      reader.end(Err(ErrorCode::NotPermitted));
    }
    Ok((
      StreamReader::new(&mut host, reader)?,
      FutureReader::new(&mut host, status)?,
    ))
  }

  fn write_via_stream(
    mut host: Access<T, Self>,
    fd: Handle<Descriptor>,
    data: StreamReader<u8>,
    offset: Filesize,
  ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    let descriptor = host.get().table.get(&fd)?.clone();
    write(host, descriptor, data, |file| WriteTo::File(file, offset))
  }

  fn append_via_stream(
    mut host: Access<T, Self>,
    fd: Handle<Descriptor>,
    data: StreamReader<u8>,
  ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    let descriptor = host.get().table.get(&fd)?.clone();
    write(host, descriptor, data, WriteTo::Append)
  }

  async fn advise(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    _offset: Filesize,
    _length: Filesize,
    _advice: Advice,
  ) -> Fs<()> {
    // Advice changes no result, and lio has no operation for it.
    let descriptor = get(accessor, &fd)?;
    Ok(if descriptor.is_dir() { Err(ErrorCode::BadDescriptor) } else { Ok(()) })
  }

  async fn sync_data(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
  ) -> Fs<()> {
    // lio syncs data and metadata together.
    let descriptor = get(accessor, &fd)?;
    Ok(op(api::fsync(&descriptor.resource)).await.map_err(|e| fs_error(&e)))
  }

  async fn get_flags(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
  ) -> Fs<DescriptorFlags> {
    Ok(Ok(get(accessor, &fd)?.flags))
  }

  async fn get_type(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
  ) -> Fs<DescriptorType> {
    let descriptor = get(accessor, &fd)?;
    Ok(
      op(api::fstat(&descriptor.resource))
        .await
        .map(|stat| kind(stat.file_type))
        .map_err(|e| fs_error(&e)),
    )
  }

  async fn set_size(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    size: Filesize,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    if descriptor.is_dir() {
      return Ok(Err(ErrorCode::BadDescriptor));
    }
    if !descriptor.flags.contains(DescriptorFlags::WRITE) {
      return Ok(Err(ErrorCode::NotPermitted));
    }
    Ok(rustix::fs::ftruncate(&descriptor.resource, size).map_err(errno))
  }

  async fn set_times(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    access: NewTimestamp,
    modification: NewTimestamp,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    // The system lets a file's owner set its times through any descriptor.
    // Here it takes the right to change what the descriptor names.
    let may = if descriptor.is_dir() {
      DescriptorFlags::MUTATE_DIRECTORY
    } else {
      DescriptorFlags::WRITE
    };
    if !descriptor.flags.contains(may) {
      return Ok(Err(ErrorCode::NotPermitted));
    }
    Ok(timestamps(access, modification).and_then(|times| {
      rustix::fs::futimens(&descriptor.resource, &times).map_err(errno)
    }))
  }

  fn read_directory(
    mut host: Access<T, Self>,
    fd: Handle<Descriptor>,
  ) -> wasmtime::Result<(
    StreamReader<DirectoryEntry>,
    FutureReader<Result<(), ErrorCode>>,
  )> {
    let descriptor = host.get().table.get(&fd)?.clone();
    let (status, ended) = crate::status::channel();
    let mut reader = DirReader {
      listing: Listing::Ended,
      ready: VecDeque::new(),
      status: Some(status),
    };
    match descriptor.dir() {
      // As for a file: the guest's flags say whether it may be read.
      Ok(_) if !descriptor.flags.contains(DescriptorFlags::READ) => {
        reader.end(Err(ErrorCode::NotPermitted));
      }
      Ok(dir) => {
        let flags = api::OpenFlags::READ | api::OpenFlags::DIRECTORY;
        reader.listing = Listing::Opening(op(api::openat(
          dir,
          c".".to_owned(),
          flags,
          mode(0),
        )));
      }
      Err(error) => reader.end(Err(error)),
    }
    Ok((
      StreamReader::new(&mut host, reader)?,
      FutureReader::new(&mut host, ended)?,
    ))
  }

  async fn sync(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    Ok(op(api::fsync(&descriptor.resource)).await.map_err(|e| fs_error(&e)))
  }

  async fn create_directory_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    at: String,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    Ok(
      async {
        let dir = descriptor.dir_mut()?;
        op(api::mkdirat(dir, path(&at)?, mode(0o777)))
          .await
          .map_err(|e| fs_error(&e))
      }
      .await,
    )
  }

  async fn stat(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
  ) -> Fs<DescriptorStat> {
    let descriptor = get(accessor, &fd)?;
    Ok(rustix::fs::fstat(&descriptor.resource).map(stat).map_err(errno))
  }

  async fn stat_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    path_flags: PathFlags,
    at: String,
  ) -> Fs<DescriptorStat> {
    let descriptor = get(accessor, &fd)?;
    Ok((|| {
      let dir = descriptor.dir()?;
      rustix::fs::statat(dir, path(&at)?, at_flags(path_flags))
        .map(stat)
        .map_err(errno)
    })())
  }

  async fn set_times_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    path_flags: PathFlags,
    at: String,
    access: NewTimestamp,
    modification: NewTimestamp,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    Ok((|| {
      let dir = descriptor.dir_mut()?;
      let times = timestamps(access, modification)?;
      rustix::fs::utimensat(dir, path(&at)?, &times, at_flags(path_flags))
        .map_err(errno)
    })())
  }

  async fn link_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    old_path_flags: PathFlags,
    old_path: String,
    new_fd: Handle<Descriptor>,
    new_path: String,
  ) -> Fs<()> {
    let (old, new) = (get(accessor, &fd)?, get(accessor, &new_fd)?);
    Ok(
      async {
        if old_path_flags.contains(PathFlags::SYMLINK_FOLLOW) {
          return Err(ErrorCode::Invalid);
        }
        let link = api::linkat(
          old.dir_mut()?,
          path(&old_path)?,
          new.dir_mut()?,
          path(&new_path)?,
          LinkKind::Hard,
        );
        op(link).await.map_err(|e| fs_error(&e))
      }
      .await,
    )
  }

  async fn open_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    path_flags: PathFlags,
    at: String,
    open_flags: OpenFlags,
    flags: DescriptorFlags,
  ) -> Fs<Handle<Descriptor>> {
    let parent = get(accessor, &fd)?;
    let opened = async {
      let dir = parent.dir()?;
      let at = path(&at)?;
      let directory = open_flags.contains(OpenFlags::DIRECTORY);
      let writes = flags.contains(DescriptorFlags::WRITE) && !directory;
      let changes = writes
        || open_flags.intersects(OpenFlags::CREATE | OpenFlags::TRUNCATE);
      // A directory is made with `create-directory-at`. The system's own
      // `open` would make a file of that name, or refuse.
      if directory
        && open_flags.intersects(
          OpenFlags::CREATE | OpenFlags::EXCLUSIVE | OpenFlags::TRUNCATE,
        )
      {
        return Err(ErrorCode::Invalid);
      }
      // The right to change a directory's entries comes from its parent:
      // a descriptor cannot have it if the one it was opened from did not.
      let mutates = flags.contains(DescriptorFlags::MUTATE_DIRECTORY);
      if (changes || mutates)
        && !parent.flags.contains(DescriptorFlags::MUTATE_DIRECTORY)
      {
        return Err(ErrorCode::NotPermitted);
      }

      let mut how = api::OpenFlags::empty();
      if flags.contains(DescriptorFlags::READ) || !writes {
        how |= api::OpenFlags::READ;
      }
      // A file that is created or cut short is opened for writing whatever
      // the guest asked: the system does neither through a descriptor that
      // only reads. What the guest may do with it is in its own flags.
      if changes {
        how |= api::OpenFlags::WRITE;
      }
      if directory {
        how |= api::OpenFlags::DIRECTORY;
      }
      if open_flags.contains(OpenFlags::CREATE) {
        how |= api::OpenFlags::CREATE;
      }
      if open_flags.contains(OpenFlags::EXCLUSIVE) {
        how |= api::OpenFlags::EXCLUSIVE;
      }
      if open_flags.contains(OpenFlags::TRUNCATE) {
        how |= api::OpenFlags::TRUNCATE;
      }

      if !path_flags.contains(PathFlags::SYMLINK_FOLLOW) {
        how |= api::OpenFlags::NOFOLLOW;
      }
      let resource = op(api::openat(dir, at, how, mode(0o666)))
        .await
        .map_err(|e| fs_error(&e))?;
      let dir = directory.then_some(true);
      Ok(Descriptor { resource, dir, flags })
    }
    .await;
    match opened {
      Ok(descriptor) => {
        let handle =
          accessor.with(|mut store| store.get().table.push(descriptor))?;
        Ok(Ok(handle))
      }
      Err(error) => Ok(Err(error)),
    }
  }

  async fn readlink_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    at: String,
  ) -> Fs<String> {
    let descriptor = get(accessor, &fd)?;
    Ok(
      async {
        let buf = Vec::with_capacity(4096);
        let (result, buf) =
          op(api::readlinkat(descriptor.dir()?, path(&at)?, buf)).await;
        result.map_err(|e| fs_error(&e))?;
        String::from_utf8(buf).map_err(|_| ErrorCode::IllegalByteSequence)
      }
      .await,
    )
  }

  async fn remove_directory_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    at: String,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    Ok(
      async {
        let dir = descriptor.dir_mut()?;
        op(api::unlinkat(dir, path(&at)?, UnlinkKind::Directory))
          .await
          .map_err(|e| fs_error(&e))
      }
      .await,
    )
  }

  async fn rename_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    old_path: String,
    new_fd: Handle<Descriptor>,
    new_path: String,
  ) -> Fs<()> {
    let (old, new) = (get(accessor, &fd)?, get(accessor, &new_fd)?);
    Ok(
      async {
        let rename = api::renameat(
          old.dir_mut()?,
          path(&old_path)?,
          new.dir_mut()?,
          path(&new_path)?,
        );
        op(rename).await.map_err(|e| fs_error(&e))
      }
      .await,
    )
  }

  async fn symlink_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    old_path: String,
    new_path: String,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    Ok(
      async {
        let dir = descriptor.dir_mut()?;
        // The target is text the link stores, which the system resolves.
        let link = api::linkat(
          dir,
          link_target(&old_path)?,
          dir,
          path(&new_path)?,
          LinkKind::Soft,
        );
        op(link).await.map_err(|e| fs_error(&e))
      }
      .await,
    )
  }

  async fn unlink_file_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    at: String,
  ) -> Fs<()> {
    let descriptor = get(accessor, &fd)?;
    Ok(
      async {
        let dir = descriptor.dir_mut()?;
        op(api::unlinkat(dir, path(&at)?, UnlinkKind::File))
          .await
          .map_err(|e| fs_error(&e))
      }
      .await,
    )
  }

  async fn is_same_object(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    other: Handle<Descriptor>,
  ) -> wasmtime::Result<bool> {
    let (a, b) = (get(accessor, &fd)?, get(accessor, &other)?);
    let (a, b) =
      (rustix::fs::fstat(&a.resource)?, rustix::fs::fstat(&b.resource)?);
    Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
  }

  async fn metadata_hash(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
  ) -> Fs<MetadataHashValue> {
    let descriptor = get(accessor, &fd)?;
    Ok(
      rustix::fs::fstat(&descriptor.resource)
        .map(|stat| hash(&stat))
        .map_err(errno),
    )
  }

  async fn metadata_hash_at(
    accessor: &Accessor<T, Self>,
    fd: Handle<Descriptor>,
    path_flags: PathFlags,
    at: String,
  ) -> Fs<MetadataHashValue> {
    let descriptor = get(accessor, &fd)?;
    Ok((|| {
      let dir = descriptor.dir()?;
      rustix::fs::statat(dir, path(&at)?, at_flags(path_flags))
        .map(|stat| hash(&stat))
        .map_err(errno)
    })())
  }
}

/// Pipes the guest's stream into a file.
fn write<T: Send + 'static>(
  mut host: Access<T, LioWasi>,
  descriptor: Descriptor,
  data: StreamReader<u8>,
  to: impl FnOnce(Resource) -> WriteTo,
) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
  let writable = descriptor.flags.contains(DescriptorFlags::WRITE);
  let lending = host.get().lending.clone();
  let (mut writer, status) =
    ByteWriter::new(to(descriptor.resource), fs_error, nothing, lending);
  // The system may have opened it for writing for its own reasons (to
  // create it, or to cut it short), so the guest's own flags are what say.
  if !writable {
    writer.end(Err(ErrorCode::NotPermitted));
  }
  data.pipe(&mut host, writer)?;
  FutureReader::new(&mut host, status)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn resolved(guest: &str) -> Option<String> {
    path(guest).ok().map(|path| path.into_string().unwrap())
  }

  #[test]
  fn no_parent_step_reaches_the_system() {
    assert_eq!(resolved("a/b").as_deref(), Some("a/b"));
    assert_eq!(resolved("./a//b/").as_deref(), Some("a/b"));
    assert_eq!(resolved("a/../b").as_deref(), Some("b"));
    // A link named `a` that leads to `.` would make `a/..` the parent of
    // the directory itself.
    assert_eq!(resolved("a/..").as_deref(), Some("."));
    assert_eq!(resolved("a/b/../..").as_deref(), Some("."));
  }

  #[test]
  fn a_path_that_leaves_its_directory_is_refused() {
    for leaves in ["..", "a/../..", "../a", "/a", "/"] {
      assert!(matches!(path(leaves), Err(ErrorCode::NotPermitted)), "{leaves}");
    }
    assert!(matches!(path(""), Err(ErrorCode::NoEntry)));
  }

  #[test]
  fn a_link_a_guest_makes_stores_no_parent_step() {
    assert!(link_target(".").is_ok());
    assert!(link_target("a/b").is_ok());
    for leaves in ["..", "a/..", "a/../b", "/etc"] {
      assert!(
        matches!(link_target(leaves), Err(ErrorCode::NotPermitted)),
        "{leaves}"
      );
    }
  }
}
