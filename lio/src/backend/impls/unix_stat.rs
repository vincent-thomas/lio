//! Conversions from Unix `stat`/`dirent` data into lio's portable metadata types.

use crate::backend::op::{FileStat, FileType};

/// File type encoded in the Unix mode bits (`S_IFMT`).
///
/// Permission bits are ignored. A mode this table does not name is
/// [`FileType::Unknown`].
#[allow(
  clippy::unnecessary_cast,
  reason = "mode_t is u16 on some BSDs and u32 on Linux"
)]
pub(crate) fn file_type_from_mode(mode: u32) -> FileType {
  match mode & libc::S_IFMT as u32 {
    bits if bits == libc::S_IFREG as u32 => FileType::File,
    bits if bits == libc::S_IFDIR as u32 => FileType::Directory,
    bits if bits == libc::S_IFLNK as u32 => FileType::Symlink,
    bits if bits == libc::S_IFBLK as u32 => FileType::BlockDevice,
    bits if bits == libc::S_IFCHR as u32 => FileType::CharDevice,
    bits if bits == libc::S_IFIFO as u32 => FileType::Fifo,
    bits if bits == libc::S_IFSOCK as u32 => FileType::Socket,
    _ => FileType::Unknown,
  }
}

#[allow(clippy::useless_conversion)]
pub(crate) fn file_stat_from_raw(stat: &libc::stat) -> FileStat {
  let file_type = file_type_from_mode(stat.st_mode.into());

  FileStat {
    file_type,
    size: stat.st_size as u64,
    permissions: (stat.st_mode & 0o7777).into(),
    mode: stat.st_mode.into(),
    nlink: stat.st_nlink.into(),
    uid: stat.st_uid,
    gid: stat.st_gid,
  }
}

pub(crate) fn file_type_from_dirent_dtype(dtype: u8) -> Option<FileType> {
  match dtype {
    libc::DT_BLK => Some(FileType::BlockDevice),
    libc::DT_CHR => Some(FileType::CharDevice),
    libc::DT_DIR => Some(FileType::Directory),
    libc::DT_FIFO => Some(FileType::Fifo),
    libc::DT_LNK => Some(FileType::Symlink),
    libc::DT_REG => Some(FileType::File),
    libc::DT_SOCK => Some(FileType::Socket),
    libc::DT_UNKNOWN => None,
    _ => None,
  }
}

#[cfg(test)]
#[allow(
  clippy::unnecessary_cast,
  reason = "mode_t is u16 on some BSDs and u32 on Linux"
)]
mod tests {
  use super::*;

  #[test]
  fn mode_bits_name_every_file_type_and_ignore_permissions() {
    let cases = [
      (libc::S_IFREG, FileType::File),
      (libc::S_IFDIR, FileType::Directory),
      (libc::S_IFLNK, FileType::Symlink),
      (libc::S_IFBLK, FileType::BlockDevice),
      (libc::S_IFCHR, FileType::CharDevice),
      (libc::S_IFIFO, FileType::Fifo),
      (libc::S_IFSOCK, FileType::Socket),
    ];
    for (bits, kind) in cases {
      assert_eq!(file_type_from_mode(bits as u32), kind);
      assert_eq!(file_type_from_mode((bits as u32) | 0o640), kind);
    }
    assert_eq!(file_type_from_mode(0o640), FileType::Unknown);
    assert_eq!(file_type_from_mode(0), FileType::Unknown);
  }

  #[test]
  fn stat_copies_size_owner_link_count_and_permissions() {
    // SAFETY: `stat` is plain data. Every field the conversion reads is set below.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    stat.st_mode = ((libc::S_IFREG as u32) | 0o640) as _;
    stat.st_size = 12 as _;
    stat.st_nlink = 2 as _;
    stat.st_uid = 7 as _;
    stat.st_gid = 8 as _;

    let got = file_stat_from_raw(&stat);
    assert_eq!(got.file_type, FileType::File);
    assert_eq!(got.size, 12);
    assert_eq!(got.permissions, 0o640);
    assert_eq!(got.nlink, 2);
    assert_eq!(got.uid, 7);
    assert_eq!(got.gid, 8);
    assert_eq!(got.mode & 0o7777, 0o640);
  }
}
