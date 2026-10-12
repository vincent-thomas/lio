//! Conversions from Unix `stat`/`dirent` data into lio's portable metadata types.

use crate::backend::op::{FileStat, FileType};

#[allow(clippy::useless_conversion)]
pub(crate) fn file_stat_from_raw(stat: &libc::stat) -> FileStat {
  let file_type = match stat.st_mode & libc::S_IFMT {
    libc::S_IFREG => FileType::File,
    libc::S_IFDIR => FileType::Directory,
    libc::S_IFLNK => FileType::Symlink,
    libc::S_IFBLK => FileType::BlockDevice,
    libc::S_IFCHR => FileType::CharDevice,
    libc::S_IFIFO => FileType::Fifo,
    libc::S_IFSOCK => FileType::Socket,
    _ => FileType::Unknown,
  };

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
