use std::{
  ffi::CString, ffi::OsString, io, os::unix::ffi::OsStringExt, path::Path,
};

use lio::api::{self, FileStat, FileType, ReadDirBuf};

use crate::{app::AppContext, util::io as io_util};

const READ_DIR_SCRATCH_BYTES: usize = 64 * 1024;
const READ_DIR_ENTRIES_CAP: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
  pub name: OsString,
  pub file_type: Option<FileType>,
}

pub fn stat_path(
  ctx: &AppContext,
  path: &Path,
  follow_symlinks: bool,
) -> io::Result<Option<FileStat>> {
  let cpath = path_to_cstring(path)?;
  let mut rx =
    api::statat(&ctx.cwd(), cpath, follow_symlinks).with_lio(ctx.lio()).send();
  match io_util::run_recv(ctx.lio(), &mut rx) {
    Ok(stat) => Ok(Some(stat)),
    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(err) => Err(err),
  }
}

pub fn read_dir_path(
  ctx: &AppContext,
  path: &Path,
) -> io::Result<Vec<DirEntry>> {
  read_dir_paths(ctx, &[path.to_path_buf()])?.into_iter().next().ok_or_else(
    || {
      io::Error::new(io::ErrorKind::UnexpectedEof, "missing directory listing")
    },
  )
}

pub fn read_dir_paths(
  ctx: &AppContext,
  paths: &[std::path::PathBuf],
) -> io::Result<Vec<Vec<DirEntry>>> {
  struct PendingDirRead {
    dir: lio::api::resource::Resource,
    rx: lio::api::io::Receiver<io::Result<ReadDirBuf>>,
    entries: Vec<DirEntry>,
  }

  let open_receivers: io::Result<Vec<_>> = paths
    .iter()
    .map(|path| {
      Ok(
        api::openat(
          &ctx.cwd(),
          path_to_cstring(path)?,
          libc::O_RDONLY | libc::O_DIRECTORY,
          0,
        )
        .with_lio(ctx.lio())
        .send(),
      )
    })
    .collect();
  let open_results = io_util::run_all(ctx.lio(), open_receivers?);

  let mut pending: Vec<Option<PendingDirRead>> = open_results
    .into_iter()
    .map(|result| {
      result.map(|dir| {
        let buf = ReadDirBuf::with_capacity(
          READ_DIR_SCRATCH_BYTES,
          READ_DIR_ENTRIES_CAP,
        );
        let rx = api::readdir(&dir, buf).with_lio(ctx.lio()).send();
        PendingDirRead { dir, rx, entries: Vec::new() }
      })
    })
    .collect::<io::Result<Vec<_>>>()?
    .into_iter()
    .map(Some)
    .collect();

  let mut results: Vec<Option<Vec<DirEntry>>> =
    Vec::with_capacity(pending.len());
  results.resize_with(pending.len(), || None);
  let mut remaining = pending.len();

  while remaining > 0 {
    let mut progressed = false;
    for (index, slot) in pending.iter_mut().enumerate() {
      let Some(state) = slot.as_mut() else {
        continue;
      };
      if let Some(result) = state.rx.try_recv() {
        progressed = true;
        let buf = result?;
        state.entries.extend(buf.iter().map(|entry| DirEntry {
          name: OsString::from_vec(entry.name.to_vec()),
          file_type: entry.file_type,
        }));
        if buf.result.eof {
          results[index] = Some(std::mem::take(&mut state.entries));
          *slot = None;
          remaining -= 1;
        } else {
          state.rx = api::readdir(&state.dir, buf).with_lio(ctx.lio()).send();
        }
      }
    }

    if remaining == 0 {
      break;
    }

    if !progressed {
      if ctx.lio().try_run()? == 0 {
        ctx.lio().run()?;
      }
    }
  }

  results
    .into_iter()
    .map(|entries| Ok(entries.expect("missing directory read result")))
    .collect()
}

pub fn path_to_cstring(path: &Path) -> io::Result<CString> {
  CString::new(path.as_os_str().to_string_lossy().as_bytes())
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))
}
