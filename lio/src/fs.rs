#![allow(clippy::never_loop)]

//! Filesystem resource types.

use std::{
  ffi::{CString, OsStr, OsString},
  io,
  path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::api::{
  FileStat, FileType, ReadDirBuf,
  io::Io,
  op::{Action, Completion, OneshotOpModel, OpModel, OpResult},
  ops,
  resource::{AsResource, FromResource, IntoResource, Resource},
};

const DEFAULT_READ_DIR_SCRATCH_BYTES: usize = 4096;
const DEFAULT_READ_DIR_ENTRIES_CAP: usize = 32;
const DEFAULT_READ_LINK_BYTES: usize = 256;
const DEFAULT_READ_TO_STRING_BYTES: usize = 64 * 1024;
const DEFAULT_REMOVE_DIR_ALL_SCRATCH_BYTES: usize = 4096;
const DEFAULT_REMOVE_DIR_ALL_ENTRIES_CAP: usize = 32;

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
  pub fn read_at(&self, buf: Vec<u8>, offset: u32) -> Io<ops::Read<Vec<u8>>> {
    crate::api::read_at(self, buf, offset)
  }

  /// Writes the provided buffer at a fixed offset.
  pub fn write_at(&self, buf: Vec<u8>, offset: u32) -> Io<ops::Write<Vec<u8>>> {
    crate::api::write_at(self, buf, offset)
  }

  /// Synchronizes file contents and metadata to stable storage.
  pub fn sync_all(&self) -> Io<ops::Fsync> {
    crate::api::fsync(self)
  }

  /// Reads metadata for this open file.
  pub fn metadata(&self) -> Io<ops::Stat> {
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
  pub fn open(path: std::ffi::CString) -> Io<OpenDirectory> {
    Io::from_op(OpenDirectory {
      inner: ops::OpenAt::new(
        Resource::cwd(),
        path,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
      ),
    })
  }

  /// Reads one batch of entries from the directory.
  pub fn readdir(&self, buf: ReadDirBuf) -> Io<ops::ReadDir> {
    crate::api::readdir(self, buf)
  }

  /// Synchronizes directory metadata to stable storage.
  pub fn sync_all(&self) -> Io<ops::Fsync> {
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
  mode: u32,
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
      mode: 0o666,
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
  pub const fn mode(mut self, mode: u32) -> Self {
    self.mode = mode;
    self
  }

  /// Opens a file relative to the current working directory.
  pub fn open(self, path: std::ffi::CString) -> Io<OpenFile> {
    Io::from_op(OpenFile {
      inner: ops::OpenAt::new(Resource::cwd(), path, self.flags(), self.mode),
    })
  }

  fn flags(&self) -> i32 {
    let mut flags = match (self.read, self.write || self.append) {
      (true, false) => libc::O_RDONLY,
      (false, true) => libc::O_WRONLY,
      (true, true) => libc::O_RDWR,
      (false, false) => libc::O_RDONLY,
    };
    if self.append {
      flags |= libc::O_APPEND;
    }
    if self.truncate {
      flags |= libc::O_TRUNC;
    }
    if self.create_new {
      flags |= libc::O_CREAT | libc::O_EXCL;
    } else if self.create {
      flags |= libc::O_CREAT;
    }
    flags
  }
}

/// Open-file operation specialized to return a typed [`File`].
pub struct OpenFile {
  inner: ops::OpenAt,
}

impl OpModel for OpenFile {
  type Item = io::Result<File>;

  fn action(&mut self) -> Action {
    self.inner.action()
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match self.inner.complete(completion) {
      OpResult::Done(Ok(resource)) => {
        OpResult::Done(Ok(File::from_resource(resource)))
      }
      OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
      OpResult::Again => OpResult::Again,
      OpResult::Yield(item) => OpResult::Yield(item.map(File::from_resource)),
    }
  }
}

impl OneshotOpModel for OpenFile {}

/// Open-directory operation specialized to return a typed [`Directory`].
pub struct OpenDirectory {
  inner: ops::OpenAt,
}

impl OpModel for OpenDirectory {
  type Item = io::Result<Directory>;

  fn action(&mut self) -> Action {
    self.inner.action()
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match self.inner.complete(completion) {
      OpResult::Done(Ok(resource)) => {
        OpResult::Done(Ok(Directory::from_resource(resource)))
      }
      OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
      OpResult::Again => OpResult::Again,
      OpResult::Yield(item) => {
        OpResult::Yield(item.map(Directory::from_resource))
      }
    }
  }
}

impl OneshotOpModel for OpenDirectory {}

/// Open-directory operation specialized to return a high-level [`ReadDir`].
pub struct OpenReadDir {
  state: OpenReadDirState,
  parent: PathBuf,
  buf: Option<ReadDirBuf>,
  entries: Vec<io::Result<DirEntry>>,
}

enum OpenReadDirState {
  Opening(ops::OpenAt),
  Reading { fd: Resource, op: ops::ReadDir },
  Done,
}

impl OpenReadDir {
  fn new(path: CString, parent: PathBuf) -> Self {
    Self {
      state: OpenReadDirState::Opening(ops::OpenAt::new(
        Resource::cwd(),
        path,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
      )),
      parent,
      buf: Some(ReadDirBuf::with_capacity(
        DEFAULT_READ_DIR_SCRATCH_BYTES,
        DEFAULT_READ_DIR_ENTRIES_CAP,
      )),
      entries: Vec::new(),
    }
  }
}

impl OpModel for OpenReadDir {
  type Item = io::Result<ReadDir>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      OpenReadDirState::Opening(op) => op.action(),
      OpenReadDirState::Reading { op, .. } => op.action(),
      OpenReadDirState::Done => {
        panic!("OpenReadDir action requested after completion")
      }
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match std::mem::replace(&mut self.state, OpenReadDirState::Done) {
      OpenReadDirState::Opening(mut op) => match op.complete(completion) {
        OpResult::Done(Ok(fd)) => {
          let buf = self.buf.take().expect("read_dir buffer missing");
          self.state = OpenReadDirState::Reading {
            fd: fd.clone(),
            op: ops::ReadDir::new(fd, buf),
          };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
        OpResult::Again => {
          self.state = OpenReadDirState::Opening(op);
          OpResult::Again
        }
        OpResult::Yield(_) => unreachable!("OpenAt is a oneshot operation"),
      },
      OpenReadDirState::Reading { fd, mut op } => match op.complete(completion)
      {
        OpResult::Done(Ok(buf)) => {
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
          self.buf = Some(buf);
          if eof {
            OpResult::Done(Ok(ReadDir::new(std::mem::take(&mut self.entries))))
          } else {
            let buf = self.buf.take().expect("read_dir buffer missing");
            self.state = OpenReadDirState::Reading {
              fd: fd.clone(),
              op: ops::ReadDir::new(fd, buf),
            };
            OpResult::Again
          }
        }
        OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
        OpResult::Again => {
          self.state = OpenReadDirState::Reading { fd, op };
          OpResult::Again
        }
        OpResult::Yield(_) => unreachable!("readdir is a oneshot operation"),
      },
      OpenReadDirState::Done => {
        panic!("OpenReadDir completed after reaching terminal state")
      }
    }
  }
}

impl OneshotOpModel for OpenReadDir {}

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
      eof: false,
    }
  }

  fn next_read(&mut self) -> ops::ReadDir {
    let buf = self.buf.take().expect("remove_dir_all buffer missing");
    ops::ReadDir::new(self.fd.clone(), buf)
  }

  fn push_entries(&mut self, buf: ReadDirBuf) {
    let entries = buf
      .iter()
      .map(|entry| PendingDirEntry {
        name: CString::new(entry.name)
          .expect("directory entry names must not contain interior NUL bytes"),
        file_type: entry.file_type,
      })
      .collect::<Vec<_>>();
    self.entries.extend(entries);
    self.buf = Some(buf);
  }
}

/// Recursive directory-removal operation specialized to return `()`.
pub struct RemoveDirAll {
  stack: Vec<RemoveDirFrame>,
  state: RemoveDirAllState,
}

enum RemoveDirAllState {
  Opening { parent: Resource, name: CString, op: ops::OpenAt },
  Reading { op: ops::ReadDir },
  Stating { parent: Resource, name: CString, op: ops::Stat },
  RemovingEntry { op: ops::UnlinkAt },
  RemovingDir { op: ops::UnlinkAt },
  Done,
}

impl RemoveDirAll {
  fn new(path: CString) -> Self {
    let parent = Resource::cwd();
    Self {
      stack: Vec::new(),
      state: RemoveDirAllState::Opening {
        op: ops::OpenAt::new(
          parent.clone(),
          path.clone(),
          libc::O_RDONLY | libc::O_DIRECTORY,
          0,
        ),
        parent,
        name: path,
      },
    }
  }

  fn advance(&mut self) -> OpResult<io::Result<()>> {
    loop {
      let Some(frame) = self.stack.last_mut() else {
        self.state = RemoveDirAllState::Done;
        return OpResult::Done(Ok(()));
      };

      if let Some(entry) = frame.entries.pop() {
        match entry.file_type {
          Some(FileType::Directory) => {
            self.state = RemoveDirAllState::Opening {
              op: ops::OpenAt::new(
                frame.fd.clone(),
                entry.name.clone(),
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
              ),
              parent: frame.fd.clone(),
              name: entry.name,
            };
          }
          Some(_) => {
            self.state = RemoveDirAllState::RemovingEntry {
              op: ops::UnlinkAt::new(frame.fd.clone(), entry.name, 0),
            };
          }
          None => {
            self.state = RemoveDirAllState::Stating {
              op: ops::Stat::new_at(
                frame.fd.clone(),
                entry.name.clone(),
                false,
              ),
              parent: frame.fd.clone(),
              name: entry.name,
            };
          }
        }
        return OpResult::Again;
      }

      if !frame.eof {
        self.state = RemoveDirAllState::Reading { op: frame.next_read() };
        return OpResult::Again;
      }

      let frame = self.stack.pop().expect("remove_dir_all stack missing frame");
      self.state = RemoveDirAllState::RemovingDir {
        op: ops::UnlinkAt::new(frame.parent, frame.name, libc::AT_REMOVEDIR),
      };
      return OpResult::Again;
    }
  }
}

impl OpModel for RemoveDirAll {
  type Item = io::Result<()>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      RemoveDirAllState::Opening { op, .. } => op.action(),
      RemoveDirAllState::Reading { op } => op.action(),
      RemoveDirAllState::Stating { op, .. } => op.action(),
      RemoveDirAllState::RemovingEntry { op } => op.action(),
      RemoveDirAllState::RemovingDir { op } => op.action(),
      RemoveDirAllState::Done => {
        panic!("RemoveDirAll action requested after completion")
      }
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match std::mem::replace(&mut self.state, RemoveDirAllState::Done) {
      RemoveDirAllState::Opening { parent, name, mut op } => {
        match op.complete(completion) {
          OpResult::Done(Ok(fd)) => {
            let mut frame = RemoveDirFrame::new(parent, name, fd);
            let op = frame.next_read();
            self.stack.push(frame);
            self.state = RemoveDirAllState::Reading { op };
            OpResult::Again
          }
          OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
          OpResult::Again => {
            self.state = RemoveDirAllState::Opening { parent, name, op };
            OpResult::Again
          }
          OpResult::Yield(_) => unreachable!("openat is a oneshot operation"),
        }
      }
      RemoveDirAllState::Reading { mut op } => match op.complete(completion) {
        OpResult::Done(Ok(buf)) => {
          if !buf.result.eof && buf.result.entries == 0 {
            panic!(
              "lio::fs::remove_dir_all made no progress: low-level readdir returned zero entries before EOF; internal buffer is too small"
            );
          }

          let frame = self
            .stack
            .last_mut()
            .expect("remove_dir_all missing frame for directory read");
          frame.eof = buf.result.eof;
          frame.push_entries(buf);
          self.advance()
        }
        OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
        OpResult::Again => {
          self.state = RemoveDirAllState::Reading { op };
          OpResult::Again
        }
        OpResult::Yield(_) => unreachable!("readdir is a oneshot operation"),
      },
      RemoveDirAllState::Stating { parent, name, mut op } => {
        match op.complete(completion) {
          OpResult::Done(Ok(stat)) => {
            self.state = if stat.is_dir() {
              RemoveDirAllState::Opening {
                op: ops::OpenAt::new(
                  parent.clone(),
                  name.clone(),
                  libc::O_RDONLY | libc::O_DIRECTORY,
                  0,
                ),
                parent,
                name,
              }
            } else {
              RemoveDirAllState::RemovingEntry {
                op: ops::UnlinkAt::new(parent, name, 0),
              }
            };
            OpResult::Again
          }
          OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
          OpResult::Again => {
            self.state = RemoveDirAllState::Stating { parent, name, op };
            OpResult::Again
          }
          OpResult::Yield(_) => unreachable!("stat is a oneshot operation"),
        }
      }
      RemoveDirAllState::RemovingEntry { mut op } => {
        match op.complete(completion) {
          OpResult::Done(Ok(())) => self.advance(),
          OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
          OpResult::Again => {
            self.state = RemoveDirAllState::RemovingEntry { op };
            OpResult::Again
          }
          OpResult::Yield(_) => unreachable!("unlinkat is a oneshot operation"),
        }
      }
      RemoveDirAllState::RemovingDir { mut op } => {
        match op.complete(completion) {
          OpResult::Done(Ok(())) => self.advance(),
          OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
          OpResult::Again => {
            self.state = RemoveDirAllState::RemovingDir { op };
            OpResult::Again
          }
          OpResult::Yield(_) => unreachable!("unlinkat is a oneshot operation"),
        }
      }
      RemoveDirAllState::Done => {
        panic!("RemoveDirAll completed after reaching terminal state")
      }
    }
  }
}

impl OneshotOpModel for RemoveDirAll {}

/// High-level symbolic-link target reader.
pub struct ReadLink {
  path: CString,
  state: ReadLinkState,
}

enum ReadLinkState {
  Reading(ops::ReadlinkAt<Vec<u8>>),
  Done,
}

impl ReadLink {
  fn new(path: CString) -> Self {
    Self {
      path: path.clone(),
      state: ReadLinkState::Reading(ops::ReadlinkAt::new(
        Resource::cwd(),
        path,
        vec![0u8; DEFAULT_READ_LINK_BYTES],
      )),
    }
  }
}

impl OpModel for ReadLink {
  type Item = io::Result<PathBuf>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      ReadLinkState::Reading(op) => op.action(),
      ReadLinkState::Done => panic!("ReadLink polled after completion"),
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match std::mem::replace(&mut self.state, ReadLinkState::Done) {
      ReadLinkState::Reading(mut op) => match op.complete(completion) {
        OpResult::Done((Ok(n), buf)) if n as usize == buf.capacity() => {
          let next_len = buf.capacity().saturating_mul(2).max(1);
          self.state = ReadLinkState::Reading(ops::ReadlinkAt::new(
            Resource::cwd(),
            self.path.clone(),
            vec![0u8; next_len],
          ));
          OpResult::Again
        }
        OpResult::Done((Ok(_), buf)) => {
          OpResult::Done(Ok(PathBuf::from(os_string_from_bytes(&buf))))
        }
        OpResult::Done((Err(err), _)) => OpResult::Done(Err(err)),
        OpResult::Again => {
          self.state = ReadLinkState::Reading(op);
          OpResult::Again
        }
        OpResult::Yield(_) => unreachable!("readlinkat is a oneshot operation"),
      },
      ReadLinkState::Done => panic!("ReadLink completed after terminal state"),
    }
  }
}

impl OneshotOpModel for ReadLink {}

/// High-level UTF-8 file reader.
pub struct ReadToString {
  state: ReadToStringState,
  bytes: Vec<u8>,
}

#[allow(clippy::large_enum_variant)]
enum ReadToStringState {
  Opening(ops::OpenAt),
  Reading { fd: Resource, op: ops::Read<Vec<u8>> },
  Done,
}

impl ReadToString {
  fn new(path: CString) -> Self {
    Self {
      state: ReadToStringState::Opening(ops::OpenAt::new(
        Resource::cwd(),
        path,
        libc::O_RDONLY,
        0,
      )),
      bytes: Vec::new(),
    }
  }
}

impl OpModel for ReadToString {
  type Item = io::Result<String>;

  fn action(&mut self) -> Action {
    match &mut self.state {
      ReadToStringState::Opening(op) => op.action(),
      ReadToStringState::Reading { op, .. } => op.action(),
      ReadToStringState::Done => panic!("ReadToString polled after completion"),
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    match std::mem::replace(&mut self.state, ReadToStringState::Done) {
      ReadToStringState::Opening(mut op) => match op.complete(completion) {
        OpResult::Done(Ok(fd)) => {
          self.state = ReadToStringState::Reading {
            fd: fd.clone(),
            op: ops::Read::new(fd, vec![0u8; DEFAULT_READ_TO_STRING_BYTES], -1),
          };
          OpResult::Again
        }
        OpResult::Done(Err(err)) => OpResult::Done(Err(err)),
        OpResult::Again => {
          self.state = ReadToStringState::Opening(op);
          OpResult::Again
        }
        OpResult::Yield(_) => unreachable!("openat is a oneshot operation"),
      },
      ReadToStringState::Reading { fd, mut op } => {
        match op.complete(completion) {
          OpResult::Done((Ok(0), _buf)) => {
            let bytes = std::mem::take(&mut self.bytes);
            OpResult::Done(
              String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            )
          }
          OpResult::Done((Ok(n), buf)) => {
            self.bytes.extend_from_slice(&buf[..n as usize]);
            self.state = ReadToStringState::Reading {
              fd: fd.clone(),
              op: ops::Read::new(fd, buf, -1),
            };
            OpResult::Again
          }
          OpResult::Done((Err(err), _)) => OpResult::Done(Err(err)),
          OpResult::Again => {
            self.state = ReadToStringState::Reading { fd, op };
            OpResult::Again
          }
          OpResult::Yield(_) => unreachable!("read is a oneshot operation"),
        }
      }
      ReadToStringState::Done => {
        panic!("ReadToString completed after terminal state")
      }
    }
  }
}

impl OneshotOpModel for ReadToString {}

/// Opens a directory reader relative to the current working directory.
pub fn read_dir<P: AsRef<Path>>(path: P) -> Io<OpenReadDir> {
  let parent = path.as_ref().to_path_buf();
  let path = path_to_cstring(path.as_ref())
    .expect("lio::fs::read_dir path must not contain interior NUL bytes");
  Io::from_op(OpenReadDir::new(path, parent))
}

/// Reads metadata for a path relative to the current working directory, following symlinks.
pub fn metadata<P: AsRef<Path>>(path: P) -> Io<ops::Stat> {
  let path = path_to_cstring(path.as_ref())
    .expect("lio::fs::metadata path must not contain interior NUL bytes");
  metadata_cstring(path, true)
}

/// Reads metadata for a path relative to the current working directory, without following symlinks.
pub fn symlink_metadata<P: AsRef<Path>>(path: P) -> Io<ops::Stat> {
  let path = path_to_cstring(path.as_ref()).expect(
    "lio::fs::symlink_metadata path must not contain interior NUL bytes",
  );
  metadata_cstring(path, false)
}

/// Reads metadata for a C string path relative to the current working directory.
pub fn metadata_cstring(
  path: std::ffi::CString,
  follow_symlinks: bool,
) -> Io<ops::Stat> {
  crate::api::statat(&Resource::cwd(), path, follow_symlinks)
}

/// Reads the target of a symbolic link relative to the current working directory.
pub fn read_link<P: AsRef<Path>>(path: P) -> Io<ReadLink> {
  let path = path_to_cstring(path.as_ref())
    .expect("lio::fs::read_link path must not contain interior NUL bytes");
  Io::from_op(ReadLink::new(path))
}

/// Reads a UTF-8 file into a string relative to the current working directory.
pub fn read_to_string<P: AsRef<Path>>(path: P) -> Io<ReadToString> {
  let path = path_to_cstring(path.as_ref())
    .expect("lio::fs::read_to_string path must not contain interior NUL bytes");
  Io::from_op(ReadToString::new(path))
}

/// Creates a directory relative to the current working directory.
pub fn create_dir(path: std::ffi::CString, mode: u32) -> Io<ops::MkdirAt> {
  crate::api::mkdirat(&Resource::cwd(), path, mode)
}

/// Renames a path relative to the current working directory.
pub fn rename(
  old_path: std::ffi::CString,
  new_path: std::ffi::CString,
) -> Io<ops::RenameAt> {
  crate::api::renameat(&Resource::cwd(), old_path, &Resource::cwd(), new_path)
}

/// Removes a file relative to the current working directory.
pub fn remove_file(path: std::ffi::CString) -> Io<ops::UnlinkAt> {
  crate::api::unlinkat(&Resource::cwd(), path, 0)
}

/// Removes an empty directory relative to the current working directory.
pub fn remove_dir(path: std::ffi::CString) -> Io<ops::UnlinkAt> {
  crate::api::unlinkat(&Resource::cwd(), path, libc::AT_REMOVEDIR)
}

/// Recursively removes a directory relative to the current working directory.
pub fn remove_dir_all(path: std::ffi::CString) -> Io<RemoveDirAll> {
  Io::from_op(RemoveDirAll::new(path))
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

fn path_to_cstring(path: &std::path::Path) -> io::Result<CString> {
  #[cfg(unix)]
  {
    CString::new(path.as_os_str().as_bytes())
      .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
  }

  #[cfg(not(unix))]
  {
    CString::new(path.to_string_lossy().as_bytes())
      .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
  }
}
