//! Filesystem resource types.

use std::task::{Poll, ready};
use std::{
  ffi::{CString, OsStr, OsString},
  io,
  path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use crate::api::{
  FileMode, FileStat, FileType, OpenFlags, ReadDirBuf, UnlinkKind, ops,
  resource::{AsResource, FromResource, IntoResource, Resource},
};
use crate::model::{Cx, OpModel};

const DEFAULT_READ_DIR_SCRATCH_BYTES: usize = 4096;
const DEFAULT_READ_DIR_ENTRIES_CAP: usize = 32;
const DEFAULT_READ_LINK_BYTES: usize = 256;
const DEFAULT_READ_TO_STRING_BYTES: usize = 64 * 1024;
const DEFAULT_REMOVE_DIR_ALL_SCRATCH_BYTES: usize = 4096;
const DEFAULT_REMOVE_DIR_ALL_ENTRIES_CAP: usize = 32;
/// `rw-rw-rw-`, less the umask: what `std::fs::OpenOptions` creates files
/// with.
const DEFAULT_FILE_MODE: FileMode = match FileMode::from_octal(0o666) {
  Some(mode) => mode,
  None => panic!("0o666 is a valid file mode"),
};

/// A regular file resource opened through lio.
pub struct File(Resource);

impl IntoResource for File {
  fn into_resource(self) -> Resource {
    self.0
  }
}

impl AsResource for File {
  fn as_resource(&self) -> &Resource {
    &self.0
  }
}

impl FromResource for File {
  fn from_resource(resource: Resource) -> Self {
    Self(resource)
  }
}

impl File {
  /// Reads from a fixed offset into the provided buffer.
  ///
  /// An `offset` above `i64::MAX` completes with
  /// [`InvalidInput`](io::ErrorKind::InvalidInput); see [`crate::api::read_at`].
  pub fn read_at(&self, buf: Vec<u8>, offset: u64) -> ops::Read<Vec<u8>> {
    crate::api::read_at(self, buf, offset)
  }

  /// Writes the provided buffer at a fixed offset.
  ///
  /// An `offset` above `i64::MAX` completes with
  /// [`InvalidInput`](io::ErrorKind::InvalidInput); see [`crate::api::write_at`].
  pub fn write_at(&self, buf: Vec<u8>, offset: u64) -> ops::Write<Vec<u8>> {
    crate::api::write_at(self, buf, offset)
  }

  /// Synchronizes file contents and metadata to stable storage.
  pub fn sync_all(&self) -> ops::Fsync {
    crate::api::fsync(self)
  }

  /// Reads metadata for this open file.
  pub fn metadata(&self) -> ops::Stat {
    crate::api::fstat(self)
  }
}

/// An opened directory handle.
pub struct Directory(Resource);

impl IntoResource for Directory {
  fn into_resource(self) -> Resource {
    self.0
  }
}

impl AsResource for Directory {
  fn as_resource(&self) -> &Resource {
    &self.0
  }
}

impl FromResource for Directory {
  fn from_resource(resource: Resource) -> Self {
    Self(resource)
  }
}

impl Directory {
  /// Opens a directory relative to the current working directory.
  pub fn open(path: std::ffi::CString) -> OpenDirectory {
    OpenDirectory { inner: open_dir(Resource::cwd(), path) }
  }

  /// Reads one batch of entries from the directory.
  pub fn readdir(&self, buf: ReadDirBuf) -> ops::ReadDir {
    crate::api::readdir(self, buf)
  }

  /// Synchronizes directory metadata to stable storage.
  pub fn sync_all(&self) -> ops::Fsync {
    crate::api::fsync(self)
  }
}

/// One directory entry yielded by [`ReadDir`].
#[derive(Debug, Clone)]
pub struct DirEntry {
  parent: PathBuf,
  file_name: OsString,
  file_type: Option<FileType>,
  ino: Option<u64>,
}

impl DirEntry {
  fn from_view(parent: PathBuf, view: crate::api::DirEntryView<'_>) -> Self {
    Self {
      parent,
      file_name: os_string_from_bytes(view.name),
      file_type: view.file_type,
      ino: view.ino,
    }
  }

  pub fn file_name(&self) -> &OsStr {
    &self.file_name
  }

  pub fn path(&self) -> PathBuf {
    self.parent.join(&self.file_name)
  }

  pub fn file_type(&self) -> Option<FileType> {
    self.file_type
  }

  pub fn ino(&self) -> Option<u64> {
    self.ino
  }
}

/// High-level directory reader mirroring the standard-library shape.
#[derive(Debug)]
pub struct ReadDir {
  entries: std::vec::IntoIter<io::Result<DirEntry>>,
}

impl ReadDir {
  fn new(entries: Vec<io::Result<DirEntry>>) -> Self {
    Self { entries: entries.into_iter() }
  }
}

impl Iterator for ReadDir {
  type Item = io::Result<DirEntry>;

  fn next(&mut self) -> Option<Self::Item> {
    self.entries.next()
  }
}

/// Builder-style options for opening files.
#[derive(Debug, Clone)]
pub struct OpenOptions {
  read: bool,
  write: bool,
  append: bool,
  truncate: bool,
  create: bool,
  create_new: bool,
  mode: FileMode,
}

impl Default for OpenOptions {
  fn default() -> Self {
    Self::new()
  }
}

impl OpenOptions {
  /// Creates a fresh set of open options.
  pub const fn new() -> Self {
    Self {
      read: false,
      write: false,
      append: false,
      truncate: false,
      create: false,
      create_new: false,
      mode: DEFAULT_FILE_MODE,
    }
  }

  /// Sets read access.
  pub const fn read(mut self, read: bool) -> Self {
    self.read = read;
    self
  }

  /// Sets write access.
  pub const fn write(mut self, write: bool) -> Self {
    self.write = write;
    self
  }

  /// Sets append mode.
  pub const fn append(mut self, append: bool) -> Self {
    self.append = append;
    self
  }

  /// Sets truncate mode.
  pub const fn truncate(mut self, truncate: bool) -> Self {
    self.truncate = truncate;
    self
  }

  /// Sets create mode.
  pub const fn create(mut self, create: bool) -> Self {
    self.create = create;
    self
  }

  /// Sets create-new mode.
  pub const fn create_new(mut self, create_new: bool) -> Self {
    self.create_new = create_new;
    self
  }

  /// Sets the mode used when creating a file.
  pub const fn mode(mut self, mode: FileMode) -> Self {
    self.mode = mode;
    self
  }

  /// Opens a file relative to the current working directory.
  ///
  /// As with [`std::fs::OpenOptions`], `truncate`, `create` and `create_new`
  /// need write access, and `truncate` with `append` needs `create_new`;
  /// otherwise the open fails with [`io::ErrorKind::InvalidInput`] and
  /// touches nothing.
  pub fn open(self, path: std::ffi::CString) -> OpenFile {
    OpenFile {
      state: match self.flags() {
        Ok(flags) => OpenFileState::Opening(ops::OpenAt::new(
          Resource::cwd(),
          path,
          flags,
          self.mode,
        )),
        Err(msg) => OpenFileState::Invalid(msg, ops::Nop::new()),
      },
    }
  }

  fn flags(&self) -> Result<OpenFlags, &'static str> {
    // Without write access the backend would still truncate (or create) the
    // file on some platforms, and refuse on others.
    if self.append {
      if self.truncate && !self.create_new {
        return Err("truncating a file opened for appending needs create_new");
      }
    } else if !self.write && (self.truncate || self.create || self.create_new) {
      return Err("truncating or creating a file needs write access");
    }
    let mut flags = OpenFlags::empty();
    // With neither read nor write requested, open read-only.
    if self.read || !(self.write || self.append) {
      flags |= OpenFlags::READ;
    }
    if self.write {
      flags |= OpenFlags::WRITE;
    }
    if self.append {
      flags |= OpenFlags::APPEND;
    }
    if self.truncate {
      flags |= OpenFlags::TRUNCATE;
    }
    if self.create_new {
      flags |= OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
    } else if self.create {
      flags |= OpenFlags::CREATE;
    }
    Ok(flags)
  }
}

/// Opens a directory for reading.
fn open_dir(parent: Resource, name: CString) -> ops::OpenAt {
  ops::OpenAt::new(
    parent,
    name,
    OpenFlags::READ | OpenFlags::DIRECTORY,
    FileMode::empty(),
  )
}

/// Open-file operation specialized to return a typed [`File`].
#[must_use = "an operation does nothing until it is run or polled"]
pub struct OpenFile {
  state: OpenFileState,
}

enum OpenFileState {
  Opening(ops::OpenAt),
  /// The options were invalid: completes with this `InvalidInput` message,
  /// from the driver like any other result.
  Invalid(&'static str, ops::Nop),
}

runnable!([] OpenFile);

impl OpModel for OpenFile {
  type Item = io::Result<File>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Poll<Self::Item> {
    match &mut self.state {
      OpenFileState::Opening(open) => {
        Poll::Ready(ready!(open.poll(cx)).map(File::from_resource))
      }
      OpenFileState::Invalid(msg, nop) => {
        let _ = ready!(nop.poll(cx));
        Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidInput, *msg)))
      }
    }
  }
}

/// Open-directory operation specialized to return a typed [`Directory`].
#[must_use = "an operation does nothing until it is run or polled"]
pub struct OpenDirectory {
  inner: ops::OpenAt,
}

runnable!([] OpenDirectory);

impl OpModel for OpenDirectory {
  type Item = io::Result<Directory>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Poll<Self::Item> {
    Poll::Ready(ready!(self.inner.poll(cx)).map(Directory::from_resource))
  }
}

/// Open-directory operation specialized to return a high-level [`ReadDir`].
#[must_use = "an operation does nothing until it is run or polled"]
pub struct OpenReadDir {
  state: OpenReadDirState,
  parent: PathBuf,
  entries: Vec<io::Result<DirEntry>>,
}

enum OpenReadDirState {
  /// The path could not be represented; completes with `InvalidInput`.
  InvalidPath(ops::Nop),
  Opening(ops::OpenAt),
  Reading {
    fd: Resource,
    read: ops::ReadDir,
  },
}

runnable!([] OpenReadDir);

impl OpModel for OpenReadDir {
  type Item = io::Result<ReadDir>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Poll<Self::Item> {
    loop {
      let (fd, buf) = match &mut self.state {
        OpenReadDirState::InvalidPath(nop) => {
          let _ = ready!(nop.poll(cx));
          return Poll::Ready(Err(ops::nul_path_error()));
        }
        OpenReadDirState::Opening(open) => match ready!(open.poll(cx)) {
          Ok(fd) => {
            let buf = ReadDirBuf::with_capacity(
              DEFAULT_READ_DIR_SCRATCH_BYTES,
              DEFAULT_READ_DIR_ENTRIES_CAP,
            );
            (fd, buf)
          }
          Err(err) => return Poll::Ready(Err(err)),
        },
        OpenReadDirState::Reading { fd, read } => {
          let buf = match ready!(read.poll(cx)) {
            Ok(buf) => buf,
            Err(err) => return Poll::Ready(Err(err)),
          };
          let eof = buf.result.eof;
          if !eof && buf.result.entries == 0 {
            panic!(
              "lio::fs::read_dir made no progress: low-level readdir returned zero entries before EOF; internal buffer is too small"
            );
          }
          self.entries.extend(
            buf
              .iter()
              .map(|entry| Ok(DirEntry::from_view(self.parent.clone(), entry))),
          );
          if eof {
            let entries = std::mem::take(&mut self.entries);
            return Poll::Ready(Ok(ReadDir::new(entries)));
          }
          (fd.clone(), buf)
        }
      };
      self.state = OpenReadDirState::Reading {
        read: ops::ReadDir::new(fd.clone(), buf),
        fd,
      };
    }
  }
}

#[derive(Clone)]
struct PendingDirEntry {
  name: CString,
  file_type: Option<FileType>,
}

struct RemoveDirFrame {
  parent: Resource,
  name: CString,
  fd: Resource,
  buf: Option<ReadDirBuf>,
  entries: Vec<PendingDirEntry>,
  /// Subdirectories found so far, removed one after another once the
  /// entries read with them are gone.
  dirs: Vec<CString>,
  eof: bool,
}

impl RemoveDirFrame {
  fn new(parent: Resource, name: CString, fd: Resource) -> Self {
    Self {
      parent,
      name,
      fd,
      buf: Some(ReadDirBuf::with_capacity(
        DEFAULT_REMOVE_DIR_ALL_SCRATCH_BYTES,
        DEFAULT_REMOVE_DIR_ALL_ENTRIES_CAP,
      )),
      entries: Vec::new(),
      dirs: Vec::new(),
      eof: false,
    }
  }

  fn next_read(&mut self) -> ops::ReadDir {
    let Some(buf) = self.buf.take() else {
      unreachable!("remove_dir_all buffer missing");
    };
    ops::ReadDir::new(self.fd.clone(), buf)
  }

  fn push_entries(&mut self, buf: ReadDirBuf) {
    let entries = buf
      .iter()
      .map(|entry| {
        let Ok(name) = CString::new(entry.name) else {
          unreachable!("directory entry names never contain NUL bytes");
        };
        PendingDirEntry { name, file_type: entry.file_type }
      })
      .collect::<Vec<_>>();
    self.entries.extend(entries);
    self.buf = Some(buf);
  }
}

/// How many entries of one directory are removed at the same time.
const REMOVE_DIR_ALL_CONCURRENCY: usize = 16;

/// Recursive directory-removal operation specialized to return `()`.
///
/// The files of a directory are removed concurrently, up to
/// [`REMOVE_DIR_ALL_CONCURRENCY`] at a time. Its subdirectories are removed
/// one after another, so the number of open directories stays the depth of
/// the tree.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct RemoveDirAll {
  stack: Vec<RemoveDirFrame>,
  state: RemoveDirAllState,
}

enum RemoveDirAllState {
  Opening {
    parent: Resource,
    name: CString,
    open: ops::OpenAt,
  },
  Reading {
    read: ops::ReadDir,
  },
  /// Removing the entries read so far that are not directories.
  Clearing {
    in_flight: Vec<Clearing>,
  },
  /// Removing a directory that has been emptied.
  Removing {
    unlink: ops::UnlinkAt,
  },
}

/// One entry of the directory being cleared.
enum Clearing {
  /// An entry whose type the directory listing did not say.
  Stat {
    name: CString,
    stat: ops::Stat,
  },
  Unlink(ops::UnlinkAt),
}

impl RemoveDirAllState {
  fn opening(parent: Resource, name: CString) -> Self {
    Self::Opening { open: open_dir(parent.clone(), name.clone()), parent, name }
  }
}

impl RemoveDirAll {
  fn new(path: CString) -> Self {
    Self {
      stack: Vec::new(),
      state: RemoveDirAllState::opening(Resource::cwd(), path),
    }
  }

  /// The next step for the directory on top of the stack, or `None` when
  /// every directory has been removed.
  fn next_state(&mut self) -> Option<RemoveDirAllState> {
    let frame = self.stack.last_mut()?;

    if !frame.entries.is_empty() {
      return Some(RemoveDirAllState::Clearing { in_flight: Vec::new() });
    }
    if let Some(name) = frame.dirs.pop() {
      return Some(RemoveDirAllState::opening(frame.fd.clone(), name));
    }
    if !frame.eof {
      return Some(RemoveDirAllState::Reading { read: frame.next_read() });
    }

    let frame = self.stack.pop()?;
    Some(RemoveDirAllState::Removing {
      unlink: ops::UnlinkAt::new(
        frame.parent,
        frame.name,
        UnlinkKind::Directory,
      ),
    })
  }
}

impl RemoveDirFrame {
  /// Starts removing entries until `in_flight` is full or none are left.
  /// Directories are set aside.
  fn start_clearing(&mut self, in_flight: &mut Vec<Clearing>) -> bool {
    let mut started = false;
    while in_flight.len() < REMOVE_DIR_ALL_CONCURRENCY
      && let Some(entry) = self.entries.pop()
    {
      match entry.file_type {
        Some(FileType::Directory) => self.dirs.push(entry.name),
        Some(_) => {
          in_flight.push(Clearing::Unlink(self.unlink_file(entry.name)));
          started = true;
        }
        None => {
          in_flight.push(Clearing::Stat {
            stat: ops::Stat::new(ops::StatOf::Path {
              dir: self.fd.clone(),
              path: entry.name.clone(),
              follow_symlinks: false,
            }),
            name: entry.name,
          });
          started = true;
        }
      }
    }
    started
  }

  fn unlink_file(&self, name: CString) -> ops::UnlinkAt {
    ops::UnlinkAt::new(self.fd.clone(), name, UnlinkKind::File)
  }

  /// Polls every entry being cleared. Returns whether any of them moved on,
  /// or the first error.
  fn poll_clearing(
    &mut self,
    in_flight: &mut Vec<Clearing>,
    cx: &mut Cx<'_>,
  ) -> io::Result<bool> {
    let mut progressed = false;
    let mut i = 0;
    while let Some(entry) = in_flight.get_mut(i) {
      match entry {
        Clearing::Stat { name, stat } => {
          let Poll::Ready(stat) = stat.poll(cx) else {
            i += 1;
            continue;
          };
          let name = std::mem::take(name);
          if stat?.is_dir() {
            self.dirs.push(name);
            in_flight.swap_remove(i);
          } else {
            // Polled, and so submitted, on the next round.
            *entry = Clearing::Unlink(self.unlink_file(name));
            i += 1;
          }
          progressed = true;
        }
        Clearing::Unlink(unlink) => {
          let Poll::Ready(done) = unlink.poll(cx) else {
            i += 1;
            continue;
          };
          done?;
          in_flight.swap_remove(i);
          progressed = true;
        }
      }
    }
    Ok(progressed)
  }
}

runnable!([] RemoveDirAll);

impl OpModel for RemoveDirAll {
  type Item = io::Result<()>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Poll<Self::Item> {
    loop {
      let next = match &mut self.state {
        RemoveDirAllState::Opening { parent, name, open } => {
          let fd = match ready!(open.poll(cx)) {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
          };
          let mut frame = RemoveDirFrame::new(parent.clone(), name.clone(), fd);
          let read = frame.next_read();
          self.stack.push(frame);
          Some(RemoveDirAllState::Reading { read })
        }
        RemoveDirAllState::Reading { read } => {
          let buf = match ready!(read.poll(cx)) {
            Ok(buf) => buf,
            Err(err) => return Poll::Ready(Err(err)),
          };
          if !buf.result.eof && buf.result.entries == 0 {
            panic!(
              "lio::fs::remove_dir_all made no progress: low-level readdir returned zero entries before EOF; internal buffer is too small"
            );
          }
          let Some(frame) = self.stack.last_mut() else {
            unreachable!("remove_dir_all missing frame for directory read");
          };
          frame.eof = buf.result.eof;
          frame.push_entries(buf);
          self.next_state()
        }
        RemoveDirAllState::Clearing { in_flight } => {
          let Some(frame) = self.stack.last_mut() else {
            unreachable!("remove_dir_all missing frame for its entries");
          };
          loop {
            let started = frame.start_clearing(in_flight);
            let progressed = match frame.poll_clearing(in_flight, cx) {
              Ok(progressed) => progressed,
              // Dropping the rest cancels what is still in flight.
              Err(err) => return Poll::Ready(Err(err)),
            };
            if in_flight.is_empty() && frame.entries.is_empty() {
              break;
            }
            if !started && !progressed {
              return Poll::Pending;
            }
          }
          self.next_state()
        }
        RemoveDirAllState::Removing { unlink } => {
          if let Err(err) = ready!(unlink.poll(cx)) {
            return Poll::Ready(Err(err));
          }
          self.next_state()
        }
      };
      match next {
        Some(state) => self.state = state,
        None => return Poll::Ready(Ok(())),
      }
    }
  }
}

/// High-level symbolic-link target reader.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct ReadLink {
  path: CString,
  state: ReadLinkState,
}

enum ReadLinkState {
  /// The path could not be represented; completes with `InvalidInput`.
  InvalidPath(ops::Nop),
  Reading(ops::ReadlinkAt<Vec<u8>>),
}

impl ReadLink {
  fn new(path: CString) -> Self {
    let state = ReadLinkState::reading(&path, DEFAULT_READ_LINK_BYTES);
    Self { path, state }
  }
}

impl ReadLinkState {
  fn reading(path: &CString, len: usize) -> Self {
    Self::Reading(ops::ReadlinkAt::new(
      Resource::cwd(),
      path.clone(),
      vec![0u8; len],
    ))
  }
}

runnable!([] ReadLink);

impl OpModel for ReadLink {
  type Item = io::Result<PathBuf>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Poll<Self::Item> {
    loop {
      match &mut self.state {
        ReadLinkState::InvalidPath(nop) => {
          let _ = ready!(nop.poll(cx));
          return Poll::Ready(Err(ops::nul_path_error()));
        }
        ReadLinkState::Reading(read) => match ready!(read.poll(cx)) {
          // The target may have been cut off: try again with more room.
          (Ok(n), buf) if n == buf.capacity() => {
            let next_len = buf.capacity().saturating_mul(2).max(1);
            self.state = ReadLinkState::reading(&self.path, next_len);
          }
          (Ok(_), buf) => {
            return Poll::Ready(Ok(PathBuf::from(os_string_from_bytes(&buf))));
          }
          (Err(err), _) => return Poll::Ready(Err(err)),
        },
      }
    }
  }
}

/// High-level UTF-8 file reader.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct ReadToString {
  state: ReadToStringState,
  bytes: Vec<u8>,
}

#[allow(clippy::large_enum_variant)]
enum ReadToStringState {
  /// The path could not be represented; completes with `InvalidInput`.
  InvalidPath(ops::Nop),
  Opening(ops::OpenAt),
  Reading {
    fd: Resource,
    read: ops::Read<Vec<u8>>,
  },
}

impl ReadToString {
  fn new(path: CString) -> Self {
    Self {
      state: ReadToStringState::Opening(ops::OpenAt::new(
        Resource::cwd(),
        path,
        OpenFlags::READ,
        FileMode::empty(),
      )),
      bytes: Vec::new(),
    }
  }
}

runnable!([] ReadToString);

impl OpModel for ReadToString {
  type Item = io::Result<String>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Poll<Self::Item> {
    loop {
      let (fd, buf) = match &mut self.state {
        ReadToStringState::InvalidPath(nop) => {
          let _ = ready!(nop.poll(cx));
          return Poll::Ready(Err(ops::nul_path_error()));
        }
        ReadToStringState::Opening(open) => match ready!(open.poll(cx)) {
          Ok(fd) => (fd, vec![0u8; DEFAULT_READ_TO_STRING_BYTES]),
          Err(err) => return Poll::Ready(Err(err)),
        },
        ReadToStringState::Reading { fd, read } => {
          match ready!(read.poll(cx)) {
            (Ok(0), _) => {
              let bytes = std::mem::take(&mut self.bytes);
              return Poll::Ready(String::from_utf8(bytes).map_err(|err| {
                io::Error::new(io::ErrorKind::InvalidData, err)
              }));
            }
            (Ok(n), buf) => {
              self.bytes.extend_from_slice(&buf[..n]);
              (fd.clone(), buf)
            }
            (Err(err), _) => return Poll::Ready(Err(err)),
          }
        }
      };
      self.state = ReadToStringState::Reading {
        read: ops::Read::new(fd.clone(), buf, None),
        fd,
      };
    }
  }
}

/// Opens a directory reader relative to the current working directory.
///
/// A path containing a NUL byte fails with [`io::ErrorKind::InvalidInput`].
pub fn read_dir<P: AsRef<Path>>(path: P) -> OpenReadDir {
  let parent = path.as_ref().to_path_buf();
  let state = match path_to_cstring(path.as_ref()) {
    Some(path) => OpenReadDirState::Opening(open_dir(Resource::cwd(), path)),
    None => OpenReadDirState::InvalidPath(ops::Nop::new()),
  };
  OpenReadDir { state, parent, entries: Vec::new() }
}

/// Reads metadata for a path relative to the current working directory, following symlinks.
///
/// A path containing a NUL byte fails with [`io::ErrorKind::InvalidInput`].
pub fn metadata<P: AsRef<Path>>(path: P) -> ops::Stat {
  match path_to_cstring(path.as_ref()) {
    Some(path) => metadata_cstring(path, true),
    None => ops::Stat::new(ops::StatOf::InvalidPath),
  }
}

/// Reads metadata for a path relative to the current working directory, without following symlinks.
///
/// A path containing a NUL byte fails with [`io::ErrorKind::InvalidInput`].
pub fn symlink_metadata<P: AsRef<Path>>(path: P) -> ops::Stat {
  match path_to_cstring(path.as_ref()) {
    Some(path) => metadata_cstring(path, false),
    None => ops::Stat::new(ops::StatOf::InvalidPath),
  }
}

/// Reads metadata for a C string path relative to the current working directory.
pub fn metadata_cstring(
  path: std::ffi::CString,
  follow_symlinks: bool,
) -> ops::Stat {
  crate::api::statat(&Resource::cwd(), path, follow_symlinks)
}

/// Reads the target of a symbolic link relative to the current working directory.
///
/// A path containing a NUL byte fails with [`io::ErrorKind::InvalidInput`].
pub fn read_link<P: AsRef<Path>>(path: P) -> ReadLink {
  match path_to_cstring(path.as_ref()) {
    Some(path) => ReadLink::new(path),
    None => ReadLink {
      path: CString::default(),
      state: ReadLinkState::InvalidPath(ops::Nop::new()),
    },
  }
}

/// Reads a UTF-8 file into a string relative to the current working directory.
///
/// A path containing a NUL byte fails with [`io::ErrorKind::InvalidInput`].
pub fn read_to_string<P: AsRef<Path>>(path: P) -> ReadToString {
  match path_to_cstring(path.as_ref()) {
    Some(path) => ReadToString::new(path),
    None => ReadToString {
      state: ReadToStringState::InvalidPath(ops::Nop::new()),
      bytes: Vec::new(),
    },
  }
}

/// Creates a directory relative to the current working directory.
pub fn create_dir(path: std::ffi::CString, mode: FileMode) -> ops::MkdirAt {
  crate::api::mkdirat(&Resource::cwd(), path, mode)
}

/// Renames a path relative to the current working directory.
pub fn rename(
  old_path: std::ffi::CString,
  new_path: std::ffi::CString,
) -> ops::RenameAt {
  crate::api::renameat(&Resource::cwd(), old_path, &Resource::cwd(), new_path)
}

/// Removes a file relative to the current working directory.
pub fn remove_file(path: std::ffi::CString) -> ops::UnlinkAt {
  crate::api::unlinkat(&Resource::cwd(), path, UnlinkKind::File)
}

/// Removes an empty directory relative to the current working directory.
pub fn remove_dir(path: std::ffi::CString) -> ops::UnlinkAt {
  crate::api::unlinkat(&Resource::cwd(), path, UnlinkKind::Directory)
}

/// Recursively removes a directory relative to the current working directory.
pub fn remove_dir_all(path: std::ffi::CString) -> RemoveDirAll {
  RemoveDirAll::new(path)
}

#[allow(dead_code)]
fn _assert_file_stat(_: FileStat) {}

fn os_string_from_bytes(bytes: &[u8]) -> OsString {
  #[cfg(unix)]
  {
    OsString::from_vec(bytes.to_vec())
  }

  #[cfg(not(unix))]
  {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
  }
}

/// `None` if the path contains a NUL byte.
fn path_to_cstring(path: &std::path::Path) -> Option<CString> {
  CString::new(path.as_os_str().as_encoded_bytes()).ok()
}

#[cfg(test)]
mod open_options_tests {
  use super::*;

  #[test]
  fn flags_follow_std_validation() {
    let o = OpenOptions::new;
    let invalid = [
      o().truncate(true),
      o().read(true).truncate(true),
      o().read(true).create(true),
      o().read(true).create_new(true),
      o().append(true).truncate(true),
      o().write(true).append(true).truncate(true),
    ];
    for opts in invalid {
      assert!(opts.flags().is_err(), "{opts:?}");
    }

    let valid = [
      (o(), OpenFlags::READ),
      (o().write(true).truncate(true), OpenFlags::WRITE | OpenFlags::TRUNCATE),
      (o().write(true).create(true), OpenFlags::WRITE | OpenFlags::CREATE),
      (o().append(true).create(true), OpenFlags::APPEND | OpenFlags::CREATE),
      (
        o().append(true).truncate(true).create_new(true),
        OpenFlags::APPEND
          | OpenFlags::TRUNCATE
          | OpenFlags::CREATE
          | OpenFlags::EXCLUSIVE,
      ),
      (
        o().read(true).write(true).create_new(true),
        OpenFlags::READ
          | OpenFlags::WRITE
          | OpenFlags::CREATE
          | OpenFlags::EXCLUSIVE,
      ),
    ];
    for (opts, flags) in valid {
      assert_eq!(opts.flags(), Ok(flags), "{opts:?}");
    }
  }

  #[test]
  fn invalid_options_complete_without_io() {
    use crate::backend::op::Op as BackendOp;
    use crate::backend::testing::ScriptedBackend;
    use std::{cell::RefCell, rc::Rc};

    let backend = ScriptedBackend::new();
    let lio = crate::Lio::new_with_backend(backend.clone(), 1).unwrap();
    let result = Rc::new(RefCell::new(None));
    let out = Rc::clone(&result);
    OpenOptions::new()
      .truncate(true)
      .open(c"file".to_owned())
      .with_lio(&lio)
      .when_done(move |res| *out.borrow_mut() = Some(res));

    // The error arrives from the driver, after a nop, never inline.
    assert!(result.borrow().is_none());
    lio.try_run().unwrap();
    let id = backend.in_flight()[0];
    assert!(matches!(backend.op(id), Some(BackendOp::Nop)));
    backend.complete(id, 0);
    lio.try_run().unwrap();

    let Some(Err(err)) = result.borrow_mut().take() else {
      panic!("invalid options must fail");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(backend.in_flight().is_empty());
  }
}
