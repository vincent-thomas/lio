//! Tests for filesystem operations: unlinkat, renameat, mkdirat.

mod common;

use common::{TempFile, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

// ============================================================================
// Helper: TempDir for directory cleanup
// ============================================================================

struct TempDir {
  path: CString,
}

impl TempDir {
  fn new(name: &str) -> Self {
    let path = CString::new(format!(
      "/tmp/lio_test_dir_{}_{}",
      name,
      std::process::id()
    ))
    .expect("Failed to create CString path");
    Self { path }
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    unsafe {
      libc::rmdir(self.path.as_ptr());
    }
  }
}

// ============================================================================
// Unlinkat tests
// ============================================================================

#[test]
fn test_unlinkat_file() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("unlinkat_file");

  // Create file first
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "Failed to create test file");
    libc::write(fd, b"test data".as_ptr() as *const _, 9);
    libc::close(fd);
  }

  // Verify file exists
  unsafe {
    let ret = libc::access(temp.path.as_ptr(), libc::F_OK);
    assert_eq!(ret, 0, "File should exist before unlink");
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Unlink the file
  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&cwd, temp.path.clone(), 0)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("unlinkat should succeed");

  // Verify file no longer exists
  unsafe {
    let ret = libc::access(temp.path.as_ptr(), libc::F_OK);
    assert_eq!(ret, -1, "File should not exist after unlink");
  }

  std::mem::forget(cwd);
  // TempFile cleanup will fail silently since file is already deleted
}

#[test]
fn test_unlinkat_nonexistent() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path = CString::new(format!(
    "/tmp/lio_test_unlinkat_nonexistent_{}.txt",
    std::process::id()
  ))
  .unwrap();

  // Try to unlink non-existent file
  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&cwd, path, 0).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "unlinkat of non-existent file should fail");

  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::ENOENT), "Should be ENOENT");

  std::mem::forget(cwd);
}

#[test]
fn test_unlinkat_directory_without_flag() {
  let mut lio = Lio::new(64).unwrap();
  let temp_dir = TempDir::new("unlinkat_dir_no_flag");

  // Create directory
  unsafe {
    let ret = libc::mkdir(temp_dir.path.as_ptr(), 0o755);
    assert_eq!(ret, 0, "Failed to create test directory");
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Try to unlink directory without AT_REMOVEDIR - should fail
  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&cwd, temp_dir.path.clone(), 0)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(
    result.is_err(),
    "unlinkat on directory without AT_REMOVEDIR should fail"
  );

  let err = result.unwrap_err();
  // Different OSes return different errors (EISDIR or EPERM)
  assert!(
    err.raw_os_error() == Some(libc::EISDIR)
      || err.raw_os_error() == Some(libc::EPERM),
    "Should be EISDIR or EPERM, got {:?}",
    err.raw_os_error()
  );

  std::mem::forget(cwd);
}

#[test]
fn test_unlinkat_directory_with_flag() {
  let mut lio = Lio::new(64).unwrap();
  let temp_dir = TempDir::new("unlinkat_dir_with_flag");

  // Create directory
  unsafe {
    let ret = libc::mkdir(temp_dir.path.as_ptr(), 0o755);
    assert_eq!(ret, 0, "Failed to create test directory");
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Unlink directory with AT_REMOVEDIR
  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&cwd, temp_dir.path.clone(), libc::AT_REMOVEDIR)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("unlinkat with AT_REMOVEDIR should succeed");

  // Verify directory no longer exists
  unsafe {
    let ret = libc::access(temp_dir.path.as_ptr(), libc::F_OK);
    assert_eq!(ret, -1, "Directory should not exist after unlink");
  }

  std::mem::forget(cwd);
}

#[test]
fn test_unlinkat_directory_not_empty() {
  let mut lio = Lio::new(64).unwrap();
  let temp_dir = TempDir::new("unlinkat_dir_not_empty");

  // Create directory with a file inside
  unsafe {
    let ret = libc::mkdir(temp_dir.path.as_ptr(), 0o755);
    assert_eq!(ret, 0, "Failed to create test directory");

    let file_path =
      CString::new(format!("{}/file.txt", temp_dir.path.to_str().unwrap()))
        .unwrap();
    let fd =
      libc::open(file_path.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Try to remove non-empty directory - should fail
  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&cwd, temp_dir.path.clone(), libc::AT_REMOVEDIR)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "unlinkat on non-empty directory should fail");

  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::ENOTEMPTY), "Should be ENOTEMPTY");

  // Cleanup the file inside first
  unsafe {
    let file_path =
      CString::new(format!("{}/file.txt", temp_dir.path.to_str().unwrap()))
        .unwrap();
    libc::unlink(file_path.as_ptr());
  }

  std::mem::forget(cwd);
}

#[test]
fn test_unlinkat_with_directory_fd() {
  let mut lio = Lio::new(64).unwrap();

  // Create a file in /tmp
  let filename =
    CString::new(format!("lio_test_unlinkat_dirfd_{}.txt", std::process::id()))
      .unwrap();
  let full_path =
    CString::new(format!("/tmp/{}", filename.to_str().unwrap())).unwrap();

  unsafe {
    let fd =
      libc::open(full_path.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644);
    assert!(fd >= 0, "Failed to create test file");
    libc::close(fd);
  }

  // Open /tmp as directory fd
  let tmp_path = CString::new("/tmp").unwrap();
  let dir_fd = unsafe {
    libc::open(tmp_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "Failed to open /tmp directory");
  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  // Unlink using relative path
  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&dir_res, filename, 0).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("unlinkat with directory fd should succeed");

  // Verify file no longer exists
  unsafe {
    let ret = libc::access(full_path.as_ptr(), libc::F_OK);
    assert_eq!(ret, -1, "File should not exist after unlink");
  }
}

// ============================================================================
// Renameat tests
// ============================================================================

#[test]
fn test_renameat_file() {
  let mut lio = Lio::new(64).unwrap();
  let temp_src = TempFile::new("renameat_src");
  let dest_path = CString::new(format!(
    "/tmp/lio_test_renameat_dest_{}.txt",
    std::process::id()
  ))
  .unwrap();

  // Create source file
  unsafe {
    let fd = libc::open(
      temp_src.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "Failed to create source file");
    libc::write(fd, b"rename test".as_ptr() as *const _, 11);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Rename file
  let (sender, receiver) = mpsc::channel();
  api::renameat(&cwd, temp_src.path.clone(), &cwd, dest_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("renameat should succeed");

  // Verify source no longer exists
  unsafe {
    let ret = libc::access(temp_src.path.as_ptr(), libc::F_OK);
    assert_eq!(ret, -1, "Source file should not exist after rename");
  }

  // Verify destination exists
  unsafe {
    let ret = libc::access(dest_path.as_ptr(), libc::F_OK);
    assert_eq!(ret, 0, "Destination file should exist after rename");

    // Cleanup
    libc::unlink(dest_path.as_ptr());
  }

  std::mem::forget(cwd);
}

#[test]
fn test_renameat_overwrite() {
  let mut lio = Lio::new(64).unwrap();
  let temp_src = TempFile::new("renameat_overwrite_src");
  let temp_dest = TempFile::new("renameat_overwrite_dest");

  // Create source file with specific content
  unsafe {
    let fd = libc::open(
      temp_src.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"new content".as_ptr() as *const _, 11);
    libc::close(fd);
  }

  // Create destination file with different content
  unsafe {
    let fd = libc::open(
      temp_dest.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"old content".as_ptr() as *const _, 11);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Rename (overwrite destination)
  let (sender, receiver) = mpsc::channel();
  api::renameat(&cwd, temp_src.path.clone(), &cwd, temp_dest.path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("renameat overwrite should succeed");

  // Verify destination has new content
  unsafe {
    let fd = libc::open(temp_dest.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 20];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 20);
    libc::close(fd);
    assert_eq!(n, 11);
    assert_eq!(&buf[..11], b"new content");
  }

  std::mem::forget(cwd);
}

#[test]
fn test_renameat_directory() {
  let mut lio = Lio::new(64).unwrap();
  let temp_src = TempDir::new("renameat_dir_src");
  let dest_path = CString::new(format!(
    "/tmp/lio_test_renameat_dir_dest_{}",
    std::process::id()
  ))
  .unwrap();

  // Create source directory
  unsafe {
    let ret = libc::mkdir(temp_src.path.as_ptr(), 0o755);
    assert_eq!(ret, 0, "Failed to create source directory");
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Rename directory
  let (sender, receiver) = mpsc::channel();
  api::renameat(&cwd, temp_src.path.clone(), &cwd, dest_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("renameat directory should succeed");

  // Verify source no longer exists
  unsafe {
    let ret = libc::access(temp_src.path.as_ptr(), libc::F_OK);
    assert_eq!(ret, -1, "Source directory should not exist after rename");
  }

  // Verify destination exists and is a directory
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    let ret = libc::stat(dest_path.as_ptr(), &mut stat);
    assert_eq!(ret, 0, "Destination should exist");
    assert!(
      (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR,
      "Should be a directory"
    );

    // Cleanup
    libc::rmdir(dest_path.as_ptr());
  }

  std::mem::forget(cwd);
}

#[test]
fn test_renameat_nonexistent_source() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let src_path = CString::new(format!(
    "/tmp/lio_test_renameat_nonexistent_src_{}.txt",
    std::process::id()
  ))
  .unwrap();
  let dest_path = CString::new(format!(
    "/tmp/lio_test_renameat_nonexistent_dest_{}.txt",
    std::process::id()
  ))
  .unwrap();

  // Try to rename non-existent file
  let (sender, receiver) = mpsc::channel();
  api::renameat(&cwd, src_path, &cwd, dest_path)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "renameat of non-existent file should fail");

  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::ENOENT), "Should be ENOENT");

  std::mem::forget(cwd);
}

#[test]
fn test_renameat_cross_directory() {
  let mut lio = Lio::new(64).unwrap();
  let temp_dir = TempDir::new("renameat_cross");

  // Create subdirectory
  unsafe {
    let ret = libc::mkdir(temp_dir.path.as_ptr(), 0o755);
    assert_eq!(ret, 0, "Failed to create subdirectory");
  }

  // Create source file in /tmp
  let src_path =
    CString::new(format!("/tmp/lio_test_cross_src_{}.txt", std::process::id()))
      .unwrap();
  let dest_path =
    CString::new(format!("{}/cross_dest.txt", temp_dir.path.to_str().unwrap()))
      .unwrap();

  unsafe {
    let fd =
      libc::open(src_path.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644);
    libc::write(fd, b"cross dir".as_ptr() as *const _, 9);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Rename across directories
  let (sender, receiver) = mpsc::channel();
  api::renameat(&cwd, src_path.clone(), &cwd, dest_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("renameat cross directory should succeed");

  // Verify source gone and destination exists
  unsafe {
    assert_eq!(
      libc::access(src_path.as_ptr(), libc::F_OK),
      -1,
      "Source should not exist"
    );
    assert_eq!(
      libc::access(dest_path.as_ptr(), libc::F_OK),
      0,
      "Destination should exist"
    );

    // Cleanup
    libc::unlink(dest_path.as_ptr());
  }

  std::mem::forget(cwd);
}

// ============================================================================
// Mkdirat tests
// ============================================================================

#[test]
fn test_mkdirat_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp_dir = TempDir::new("mkdirat_basic");

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Create directory
  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&cwd, temp_dir.path.clone(), 0o755)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("mkdirat should succeed");

  // Verify directory exists
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    let ret = libc::stat(temp_dir.path.as_ptr(), &mut stat);
    assert_eq!(ret, 0, "Directory should exist");
    assert!(
      (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR,
      "Should be a directory"
    );
  }

  std::mem::forget(cwd);
}

#[test]
fn test_mkdirat_with_permissions() {
  // Skip if running as root (root ignores permissions)
  if unsafe { libc::getuid() } == 0 {
    return;
  }

  let mut lio = Lio::new(64).unwrap();
  let temp_dir = TempDir::new("mkdirat_perms");

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Create directory with restricted permissions
  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&cwd, temp_dir.path.clone(), 0o700)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("mkdirat should succeed");

  // Verify permissions (mask with 0o777 to ignore umask effects)
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::stat(temp_dir.path.as_ptr(), &mut stat);
    let perms = stat.st_mode & 0o777;
    // Note: actual perms may be affected by umask, so just verify it's not world-readable
    assert!(
      perms & 0o007 == 0,
      "Should not be world-accessible, got {:o}",
      perms
    );
  }

  std::mem::forget(cwd);
}

#[test]
fn test_mkdirat_already_exists() {
  let mut lio = Lio::new(64).unwrap();
  let temp_dir = TempDir::new("mkdirat_exists");

  // Create directory first
  unsafe {
    let ret = libc::mkdir(temp_dir.path.as_ptr(), 0o755);
    assert_eq!(ret, 0, "Failed to create test directory");
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Try to create again - should fail
  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&cwd, temp_dir.path.clone(), 0o755)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "mkdirat on existing directory should fail");

  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::EEXIST), "Should be EEXIST");

  std::mem::forget(cwd);
}

#[test]
fn test_mkdirat_nested_missing_parent() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path = CString::new(format!(
    "/tmp/lio_nonexistent_{}/nested/dir",
    std::process::id()
  ))
  .unwrap();

  // Try to create nested directory without parent - should fail
  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&cwd, path, 0o755).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "mkdirat with missing parent should fail");

  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::ENOENT), "Should be ENOENT");

  std::mem::forget(cwd);
}

#[test]
fn test_mkdirat_with_directory_fd() {
  let mut lio = Lio::new(64).unwrap();

  // Use /tmp as the base directory
  let tmp_path = CString::new("/tmp").unwrap();
  let dir_fd = unsafe {
    libc::open(tmp_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "Failed to open /tmp directory");
  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  let dirname =
    CString::new(format!("lio_test_mkdirat_dirfd_{}", std::process::id()))
      .unwrap();
  let full_path =
    CString::new(format!("/tmp/{}", dirname.to_str().unwrap())).unwrap();

  // Create directory using relative path
  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&dir_res, dirname, 0o755).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("mkdirat with directory fd should succeed");

  // Verify directory exists
  unsafe {
    let ret = libc::access(full_path.as_ptr(), libc::F_OK);
    assert_eq!(ret, 0, "Directory should exist");

    // Cleanup
    libc::rmdir(full_path.as_ptr());
  }
}

#[test]
fn test_mkdirat_permission_denied() {
  // Skip if running as root (root can create anywhere)
  if unsafe { libc::getuid() } == 0 {
    return;
  }

  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  // Try to create in root-owned directory
  let path =
    CString::new(format!("/root/lio_test_{}", std::process::id())).unwrap();

  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&cwd, path, 0o755).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "mkdirat in /root should fail for non-root");

  let err = result.unwrap_err();
  // Could be EACCES or ENOENT depending on whether /root exists
  assert!(
    err.raw_os_error() == Some(libc::EACCES)
      || err.raw_os_error() == Some(libc::ENOENT),
    "Should be EACCES or ENOENT, got {:?}",
    err.raw_os_error()
  );

  std::mem::forget(cwd);
}

// ============================================================================
// Combined operation tests
// ============================================================================

#[test]
fn test_create_write_rename_unlink() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let src_path = CString::new(format!(
    "/tmp/lio_test_combined_src_{}.txt",
    std::process::id()
  ))
  .unwrap();
  let dest_path = CString::new(format!(
    "/tmp/lio_test_combined_dest_{}.txt",
    std::process::id()
  ))
  .unwrap();

  // Create and write file
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(
    &cwd,
    src_path.clone(),
    libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
  )
  .with_lio(&mut lio)
  .send_with(sender_open);

  let fd =
    poll_until_recv(&mut lio, &receiver_open).expect("Failed to create file");

  let data = b"combined test data".to_vec();
  let (sender_write, receiver_write) = mpsc::channel();
  api::write(&fd, data).with_lio(&mut lio).send_with(sender_write);
  poll_until_recv(&mut lio, &receiver_write).0.expect("Failed to write");

  drop(fd);

  // Rename
  let (sender_rename, receiver_rename) = mpsc::channel();
  api::renameat(&cwd, src_path.clone(), &cwd, dest_path.clone())
    .with_lio(&mut lio)
    .send_with(sender_rename);
  poll_until_recv(&mut lio, &receiver_rename).expect("Failed to rename");

  // Verify renamed file has correct content
  unsafe {
    let fd = libc::open(dest_path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 50];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 50);
    libc::close(fd);
    assert_eq!(n, 18);
    assert_eq!(&buf[..18], b"combined test data");
  }

  // Unlink
  let (sender_unlink, receiver_unlink) = mpsc::channel();
  api::unlinkat(&cwd, dest_path.clone(), 0)
    .with_lio(&mut lio)
    .send_with(sender_unlink);
  poll_until_recv(&mut lio, &receiver_unlink).expect("Failed to unlink");

  // Verify file is gone
  unsafe {
    assert_eq!(
      libc::access(dest_path.as_ptr(), libc::F_OK),
      -1,
      "File should not exist"
    );
  }

  std::mem::forget(cwd);
}

#[test]
fn test_mkdir_create_file_unlink_rmdir() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let dir_path =
    CString::new(format!("/tmp/lio_test_mkdir_flow_{}", std::process::id()))
      .unwrap();
  let file_path = CString::new(format!(
    "/tmp/lio_test_mkdir_flow_{}/file.txt",
    std::process::id()
  ))
  .unwrap();

  // Create directory
  let (sender_mkdir, receiver_mkdir) = mpsc::channel();
  api::mkdirat(&cwd, dir_path.clone(), 0o755)
    .with_lio(&mut lio)
    .send_with(sender_mkdir);
  poll_until_recv(&mut lio, &receiver_mkdir)
    .expect("Failed to create directory");

  // Create file inside directory
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(&cwd, file_path.clone(), libc::O_CREAT | libc::O_WRONLY)
    .with_lio(&mut lio)
    .send_with(sender_open);
  let fd =
    poll_until_recv(&mut lio, &receiver_open).expect("Failed to create file");
  drop(fd);

  // Unlink file
  let (sender_unlink, receiver_unlink) = mpsc::channel();
  api::unlinkat(&cwd, file_path.clone(), 0)
    .with_lio(&mut lio)
    .send_with(sender_unlink);
  poll_until_recv(&mut lio, &receiver_unlink).expect("Failed to unlink file");

  // Remove directory
  let (sender_rmdir, receiver_rmdir) = mpsc::channel();
  api::unlinkat(&cwd, dir_path.clone(), libc::AT_REMOVEDIR)
    .with_lio(&mut lio)
    .send_with(sender_rmdir);
  poll_until_recv(&mut lio, &receiver_rmdir)
    .expect("Failed to remove directory");

  // Verify everything is gone
  unsafe {
    assert_eq!(
      libc::access(dir_path.as_ptr(), libc::F_OK),
      -1,
      "Directory should not exist"
    );
  }

  std::mem::forget(cwd);
}
