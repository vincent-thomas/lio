#![allow(dead_code)]

use std::{
  io,
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread,
};

use ignore::overrides::OverrideBuilder;
use kanal::unbounded;
use lio::api::FileStat;

use crate::app::AppContext;
use crate::util::{
  fs as fs_util, io as io_util,
  walker::{self as shared_walker, WalkControl},
};

use super::search::TargetOrder;
use super::{SortSpec, TraversalSpec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WalkFile {
  pub path: PathBuf,
  pub display_path: String,
  pub depth: usize,
}

#[derive(Debug, Clone)]
pub(super) struct ParallelWalkTask {
  pub input_path: Arc<str>,
  pub preserve_dot_prefix: bool,
  pub shard: shared_walker::ParallelDirShard,
}

#[derive(Debug, Default)]
pub(super) struct ParallelWalkWork {
  pub immediate_files: Vec<WalkFile>,
  pub shard_tasks: Vec<ParallelWalkTask>,
}

impl WalkFile {
  #[cfg(test)]
  fn for_display(cwd: &Path, display_path: &str) -> Self {
    let normalized_path =
      display_path.strip_prefix("./").unwrap_or(display_path);
    let path = cwd.join(normalized_path);
    let depth = display_path_depth(display_path);
    Self { path, display_path: display_path.to_owned(), depth }
  }
}

#[derive(Debug, Clone)]
pub(super) struct FileWalker {
  base: shared_walker::FileWalker,
  traversal: TraversalSpec,
  globs: Vec<String>,
  sort: SortSpec,
  target_order: TargetOrder,
}

impl FileWalker {
  pub(super) fn new(
    cwd: PathBuf,
    traversal: TraversalSpec,
    globs: Vec<String>,
    sort: SortSpec,
    target_order: TargetOrder,
  ) -> io::Result<Self> {
    let unordered_fast_walk = matches!(sort.kind, super::SortKind::None)
      && matches!(target_order, TargetOrder::Cli);
    let base = shared_walker::FileWalker::new(
      cwd,
      shared_walker::WalkOptions {
        hidden: traversal.hidden,
        no_ignore: traversal.no_ignore,
        no_ignore_vcs: false,
        follow_symlinks: false,
        one_file_system: false,
        min_depth: None,
        max_depth: None,
        overrides: build_overrides(&traversal, &globs)?,
        max_filesize: None,
        sort_entries: !unordered_fast_walk,
        prefetch_children: !unordered_fast_walk,
      },
    );
    Ok(Self { base, traversal, globs, sort, target_order })
  }

  pub(super) fn walk_paths(
    &self,
    ctx: &AppContext,
    paths: &[String],
  ) -> io::Result<Vec<WalkFile>> {
    let search_paths =
      if paths.is_empty() { vec![".".to_owned()] } else { paths.to_vec() };

    let search_path_bufs: Vec<PathBuf> =
      search_paths.iter().map(|path| self.cwd().join(path)).collect();
    let stats = self.stat_paths(ctx, &search_path_bufs)?;
    let mut files = Vec::new();
    for (path, metadata) in search_paths.into_iter().zip(stats) {
      files.extend(self.walk_path_with_stat(ctx, &path, metadata)?);
    }

    self.order_files(&mut files);
    Ok(files)
  }

  pub(super) fn walk_paths_streaming<F>(
    &self,
    ctx: &AppContext,
    paths: &[String],
    mut visit: F,
  ) -> io::Result<()>
  where
    F: FnMut(WalkFile) -> io::Result<WalkControl>,
  {
    let search_paths =
      if paths.is_empty() { vec![".".to_owned()] } else { paths.to_vec() };

    let search_path_bufs: Vec<PathBuf> =
      search_paths.iter().map(|path| self.cwd().join(path)).collect();
    let stats = self.stat_paths(ctx, &search_path_bufs)?;
    for (path, metadata) in search_paths.into_iter().zip(stats) {
      if self.walk_path_with_stat_streaming(ctx, &path, metadata, &mut visit)?
        == WalkControl::Break
      {
        break;
      }
    }
    Ok(())
  }

  pub(super) fn walk_paths_streaming_parallel<F>(
    &self,
    ctx: &AppContext,
    paths: &[String],
    parallelism: usize,
    mut visit: F,
  ) -> io::Result<()>
  where
    F: FnMut(WalkFile) -> io::Result<WalkControl>,
  {
    if parallelism <= 1 {
      return self.walk_paths_streaming(ctx, paths, visit);
    }

    let search_paths =
      if paths.is_empty() { vec![".".to_owned()] } else { paths.to_vec() };

    #[derive(Clone)]
    struct ParallelTask {
      input_path: Arc<str>,
      preserve_dot_prefix: bool,
      shard: shared_walker::ParallelDirShard,
    }

    let search_path_bufs: Vec<PathBuf> =
      search_paths.iter().map(|path| self.cwd().join(path)).collect();
    let stats = self.stat_paths(ctx, &search_path_bufs)?;

    let mut tasks = Vec::new();
    for (path, metadata) in search_paths.into_iter().zip(stats) {
      let absolute_path = self.cwd().join(&path);
      let normalized_input: Arc<str> = normalize_input_path(&path).into();
      let preserve_dot_prefix =
        should_preserve_dot_prefix(normalized_input.as_ref());
      let Some(metadata) = metadata else {
        return Err(io::Error::new(
          io::ErrorKind::NotFound,
          format!("rg: {path}: No such file or directory"),
        ));
      };

      if metadata.is_file() {
        for file in self.walk_explicit_file(&path, &absolute_path)? {
          if visit(file)? == WalkControl::Break {
            return Ok(());
          }
        }
        continue;
      }

      if !metadata.is_dir() {
        continue;
      }

      let relative =
        shared_walker::normalize_display_path(self.cwd(), &absolute_path);
      let plan = self.base.build_parallel_walk_plan(ctx, &relative)?;
      for entry in plan.immediate_entries {
        if entry.file_type != lio::api::FileType::File {
          continue;
        }
        if !self.should_include_path(&entry.display_path) {
          continue;
        }
        let depth = display_path_depth(&entry.display_path);
        let display_path = display_path_for_search_root_with_flags(
          preserve_dot_prefix,
          &entry.display_path,
        );
        if visit(WalkFile { path: entry.path, depth, display_path })?
          == WalkControl::Break
        {
          return Ok(());
        }
      }
      tasks.extend(plan.dir_shards.into_iter().map(|shard| ParallelTask {
        input_path: Arc::clone(&normalized_input),
        preserve_dot_prefix,
        shard,
      }));
    }

    if tasks.is_empty() {
      return Ok(());
    }

    let worker_count = parallelism.min(tasks.len()).max(1);
    let mut worker_tasks = vec![Vec::new(); worker_count];
    for (index, task) in tasks.into_iter().enumerate() {
      worker_tasks[index % worker_count].push(task);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = unbounded::<io::Result<Vec<WalkFile>>>();
    let mut handles = Vec::with_capacity(worker_count);
    for tasks in worker_tasks {
      let walker = self.clone();
      let stop = Arc::clone(&stop);
      let tx = tx.clone();
      handles.push(thread::spawn(move || {
        let ctx = match AppContext::new() {
          Ok(ctx) => ctx,
          Err(err) => {
            let _ = tx.send(Err(err));
            return;
          }
        };

        const TRAVERSAL_CHUNK_SIZE: usize = 256;
        for task in tasks {
          if stop.load(Ordering::Relaxed) {
            break;
          }

          let mut chunk = Vec::with_capacity(TRAVERSAL_CHUNK_SIZE);
          let result =
            walker.base.walk_parallel_dir_shard(&ctx, task.shard, |entry| {
              if stop.load(Ordering::Relaxed) {
                return Ok(WalkControl::Break);
              }
              if entry.file_type != lio::api::FileType::File {
                return Ok(WalkControl::Continue);
              }
              if !walker.should_include_path(&entry.display_path) {
                return Ok(WalkControl::Continue);
              }
              let depth = display_path_depth(&entry.display_path);
              let display_path = display_path_for_search_root_with_flags(
                task.preserve_dot_prefix,
                &entry.display_path,
              );
              chunk.push(WalkFile { path: entry.path, depth, display_path });
              if chunk.len() >= TRAVERSAL_CHUNK_SIZE {
                tx.send(Ok(std::mem::take(&mut chunk))).map_err(|err| {
                  io::Error::other(format!("rg: traversal queue failed: {err}"))
                })?;
              }
              Ok(WalkControl::Continue)
            });

          match result {
            Ok(_) => {
              if !chunk.is_empty() && tx.send(Ok(chunk)).is_err() {
                break;
              }
            }
            Err(err) => {
              let _ = tx.send(Err(err));
              break;
            }
          }
        }
      }));
    }
    drop(tx);

    let mut first_error = None;
    while let Ok(result) = rx.recv() {
      match result {
        Ok(files) => {
          for file in files {
            if visit(file)? == WalkControl::Break {
              stop.store(true, Ordering::Relaxed);
              break;
            }
          }
        }
        Err(err) => {
          stop.store(true, Ordering::Relaxed);
          if first_error.is_none() {
            first_error = Some(err);
          }
          break;
        }
      }
    }

    stop.store(true, Ordering::Relaxed);
    for handle in handles {
      let _ = handle.join();
    }

    if let Some(err) = first_error {
      return Err(err);
    }
    Ok(())
  }

  pub(super) fn build_parallel_walk_work(
    &self,
    ctx: &AppContext,
    paths: &[String],
  ) -> io::Result<ParallelWalkWork> {
    let search_paths =
      if paths.is_empty() { vec![".".to_owned()] } else { paths.to_vec() };

    let search_path_bufs: Vec<PathBuf> =
      search_paths.iter().map(|path| self.cwd().join(path)).collect();
    let stats = self.stat_paths(ctx, &search_path_bufs)?;

    let mut work = ParallelWalkWork::default();
    for (path, metadata) in search_paths.into_iter().zip(stats) {
      let absolute_path = self.cwd().join(&path);
      let normalized_input: Arc<str> = normalize_input_path(&path).into();
      let preserve_dot_prefix =
        should_preserve_dot_prefix(normalized_input.as_ref());
      let Some(metadata) = metadata else {
        return Err(io::Error::new(
          io::ErrorKind::NotFound,
          format!("rg: {path}: No such file or directory"),
        ));
      };

      if metadata.is_file() {
        work
          .immediate_files
          .extend(self.walk_explicit_file(&path, &absolute_path)?);
        continue;
      }

      if !metadata.is_dir() {
        continue;
      }

      let relative =
        shared_walker::normalize_display_path(self.cwd(), &absolute_path);
      let plan = self.base.build_parallel_walk_plan(ctx, &relative)?;
      work.immediate_files.reserve(plan.immediate_entries.len());
      work.shard_tasks.reserve(plan.dir_shards.len());
      for entry in plan.immediate_entries {
        if entry.file_type != lio::api::FileType::File {
          continue;
        }
        if !self.should_include_path(&entry.display_path) {
          continue;
        }
        let depth = display_path_depth(&entry.display_path);
        let display_path = display_path_for_search_root_owned_with_flags(
          preserve_dot_prefix,
          entry.display_path,
        );
        work.immediate_files.push(WalkFile {
          path: entry.path,
          depth,
          display_path,
        });
      }
      work.shard_tasks.extend(plan.dir_shards.into_iter().map(|shard| {
        ParallelWalkTask {
          input_path: Arc::clone(&normalized_input),
          preserve_dot_prefix,
          shard,
        }
      }));
    }

    Ok(work)
  }

  pub(super) fn walk_parallel_task_streaming(
    &self,
    ctx: &AppContext,
    task: ParallelWalkTask,
    mut visit: impl FnMut(WalkFile) -> io::Result<WalkControl>,
  ) -> io::Result<WalkControl> {
    self.base.walk_parallel_dir_shard(ctx, task.shard, |entry| {
      if entry.file_type != lio::api::FileType::File {
        return Ok(WalkControl::Continue);
      }
      if !self.should_include_path(&entry.display_path) {
        return Ok(WalkControl::Continue);
      }
      let depth = display_path_depth(&entry.display_path);
      let display_path = display_path_for_search_root_with_flags(
        task.preserve_dot_prefix,
        &entry.display_path,
      );
      visit(WalkFile { path: entry.path, depth, display_path })
    })
  }

  pub(super) fn split_parallel_task(
    &self,
    ctx: &AppContext,
    task: ParallelWalkTask,
  ) -> io::Result<(Vec<WalkFile>, Vec<ParallelWalkTask>)> {
    let (entries, shards) =
      self.base.split_parallel_dir_shard(ctx, task.shard)?;
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
      if entry.file_type != lio::api::FileType::File {
        continue;
      }
      if !self.should_include_path(&entry.display_path) {
        continue;
      }
      let depth = display_path_depth(&entry.display_path);
      let display_path = display_path_for_search_root_owned_with_flags(
        task.preserve_dot_prefix,
        entry.display_path,
      );
      files.push(WalkFile { path: entry.path, depth, display_path });
    }

    let mut tasks = Vec::with_capacity(shards.len());
    for shard in shards {
      tasks.push(ParallelWalkTask {
        input_path: Arc::clone(&task.input_path),
        preserve_dot_prefix: task.preserve_dot_prefix,
        shard,
      });
    }

    Ok((files, tasks))
  }

  pub(super) fn walk_path(
    &self,
    ctx: &AppContext,
    path: &str,
  ) -> io::Result<Vec<WalkFile>> {
    let metadata = fs_util::stat_path(ctx, &self.cwd().join(path), false)
      .map_err(|err| self.walk_io_error(path, err))?;
    self.walk_path_with_stat(ctx, path, metadata)
  }

  fn walk_path_with_stat_streaming(
    &self,
    ctx: &AppContext,
    path: &str,
    metadata: Option<FileStat>,
    visit: &mut impl FnMut(WalkFile) -> io::Result<WalkControl>,
  ) -> io::Result<WalkControl> {
    let absolute_path = self.cwd().join(path);
    let Some(metadata) = metadata else {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("rg: {path}: No such file or directory"),
      ));
    };

    if metadata.is_file() {
      for file in self.walk_explicit_file(path, &absolute_path)? {
        if visit(file)? == WalkControl::Break {
          return Ok(WalkControl::Break);
        }
      }
      return Ok(WalkControl::Continue);
    }

    if metadata.is_dir() {
      return self.walk_directory_streaming(ctx, path, &absolute_path, visit);
    }

    Ok(WalkControl::Continue)
  }

  fn walk_path_with_stat(
    &self,
    ctx: &AppContext,
    path: &str,
    metadata: Option<FileStat>,
  ) -> io::Result<Vec<WalkFile>> {
    let absolute_path = self.cwd().join(path);
    let Some(metadata) = metadata else {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("rg: {path}: No such file or directory"),
      ));
    };

    if metadata.is_file() {
      return self.walk_explicit_file(path, &absolute_path);
    }

    if metadata.is_dir() {
      let mut files = self.walk_directory(ctx, path, &absolute_path)?;
      if matches!(self.target_order, TargetOrder::Deterministic)
        && self.sort.kind == super::SortKind::None
      {
        files.sort_by(|left, right| {
          left
            .depth
            .cmp(&right.depth)
            .then_with(|| left.display_path.cmp(&right.display_path))
        });
      }
      return Ok(files);
    }

    Ok(Vec::new())
  }

  pub(super) fn walk_explicit_file(
    &self,
    input_path: &str,
    absolute_path: &Path,
  ) -> io::Result<Vec<WalkFile>> {
    let display_path =
      display_path_for_explicit_path(self.cwd(), input_path, absolute_path);
    Ok(vec![WalkFile {
      depth: display_path_depth(&display_path),
      path: absolute_path.to_path_buf(),
      display_path,
    }])
  }

  pub(super) fn walk_directory(
    &self,
    ctx: &AppContext,
    input_path: &str,
    absolute_path: &Path,
  ) -> io::Result<Vec<WalkFile>> {
    let relative =
      shared_walker::normalize_display_path(self.cwd(), absolute_path);
    let entries = self.base.walk_path(ctx, &relative)?;
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
      if entry.file_type != lio::api::FileType::File {
        continue;
      }
      if !self.should_include_path(&entry.display_path) {
        continue;
      }
      let display_path =
        display_path_for_search_root_owned(input_path, entry.display_path);
      files.push(WalkFile {
        path: entry.path,
        depth: display_path_depth(&display_path),
        display_path,
      });
    }
    Ok(files)
  }

  pub(super) fn walk_directory_streaming(
    &self,
    ctx: &AppContext,
    input_path: &str,
    absolute_path: &Path,
    visit: &mut impl FnMut(WalkFile) -> io::Result<WalkControl>,
  ) -> io::Result<WalkControl> {
    let relative =
      shared_walker::normalize_display_path(self.cwd(), absolute_path);
    let mut buffered = Vec::new();
    let use_buffered_order =
      matches!(self.target_order, TargetOrder::Deterministic)
        && self.sort.kind == super::SortKind::None;

    self.base.walk_path_streaming(ctx, &relative, |entry| {
      if entry.file_type != lio::api::FileType::File {
        return Ok(WalkControl::Continue);
      }

      let display_path = display_path_for_search_root(
        input_path,
        &shared_walker::normalize_display_path(self.cwd(), entry.path),
      );
      if !self.should_include_path(&display_path) {
        return Ok(WalkControl::Continue);
      }
      let file = WalkFile {
        path: entry.path.to_path_buf(),
        depth: display_path_depth(&display_path),
        display_path,
      };

      if use_buffered_order {
        buffered.push(file);
        Ok(WalkControl::Continue)
      } else {
        visit(file)
      }
    })?;

    if use_buffered_order {
      buffered.sort_by(|left, right| {
        left
          .depth
          .cmp(&right.depth)
          .then_with(|| left.display_path.cmp(&right.display_path))
      });
      for file in buffered {
        if visit(file)? == WalkControl::Break {
          return Ok(WalkControl::Break);
        }
      }
    }

    Ok(WalkControl::Continue)
  }

  pub(super) fn should_include_path(&self, display_path: &str) -> bool {
    !matches_excluded_directory(display_path, &self.traversal.exclude_dirs)
  }

  pub(super) fn order_files(&self, files: &mut Vec<WalkFile>) {
    match self.sort.kind {
      super::SortKind::None => {
        if matches!(self.target_order, TargetOrder::Cli) {
          files.reverse();
        }
      }
      super::SortKind::Path => {
        files.sort_by(|left, right| left.display_path.cmp(&right.display_path));
        if self.sort.reverse {
          files.reverse();
        }
      }
    }
  }

  pub(super) fn cwd(&self) -> &Path {
    self.base.cwd()
  }

  pub(super) fn traversal(&self) -> &TraversalSpec {
    &self.traversal
  }

  pub(super) fn globs(&self) -> &[String] {
    &self.globs
  }

  pub(super) fn sort(&self) -> SortSpec {
    self.sort
  }

  pub(super) fn target_order(&self) -> TargetOrder {
    self.target_order
  }

  fn walk_io_error(&self, path: &str, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("rg: {path}: {err}"))
  }

  fn stat_paths(
    &self,
    ctx: &AppContext,
    paths: &[PathBuf],
  ) -> io::Result<Vec<Option<lio::api::FileStat>>> {
    let receivers: io::Result<Vec<_>> = paths
      .iter()
      .map(|path| {
        Ok(
          lio::api::statat(
            &ctx.cwd(),
            crate::util::fs::path_to_cstring(path)?,
            false,
          )
          .with_lio(ctx.lio())
          .send(),
        )
      })
      .collect();

    Ok(
      io_util::run_all(ctx.lio(), receivers?)
        .into_iter()
        .zip(paths.iter())
        .map(|(result, path)| {
          let display_path =
            shared_walker::normalize_display_path(self.cwd(), path);
          Ok(match result {
            Ok(stat) => Some(stat),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(self.walk_io_error(&display_path, err)),
          })
        })
        .collect::<io::Result<Vec<_>>>()?,
    )
  }
}

fn build_overrides(
  traversal: &TraversalSpec,
  globs: &[String],
) -> io::Result<Option<ignore::overrides::Override>> {
  if traversal.include_globs.is_empty()
    && traversal.exclude_globs.is_empty()
    && globs.is_empty()
  {
    return Ok(None);
  }

  let mut builder = OverrideBuilder::new(".");
  for glob in &traversal.include_globs {
    builder.add(glob).map_err(|err| {
      io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
    })?;
  }
  for glob in globs {
    builder.add(glob).map_err(|err| {
      io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
    })?;
  }
  for glob in &traversal.exclude_globs {
    builder.add(&format!("!{glob}")).map_err(|err| {
      io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
    })?;
  }

  builder
    .build()
    .map(Some)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))
}

fn matches_excluded_directory(path: &str, exclude_dirs: &[String]) -> bool {
  if exclude_dirs.is_empty() {
    return false;
  }

  let mut parts = path.strip_prefix("./").unwrap_or(path).split('/').peekable();
  while let Some(part) = parts.next() {
    if parts.peek().is_none() {
      break;
    }
    if exclude_dirs
      .iter()
      .any(|pattern| super::glob::glob_matches(pattern, part))
    {
      return true;
    }
  }
  false
}

fn display_path_for_explicit_path(
  cwd: &Path,
  input_path: &str,
  absolute_path: &Path,
) -> String {
  let normalized_input = normalize_input_path(input_path);
  if should_preserve_dot_prefix(&normalized_input) {
    normalized_input
  } else {
    shared_walker::normalize_display_path(cwd, absolute_path)
  }
}

fn display_path_for_search_root(
  input_path: &str,
  display_path: &str,
) -> String {
  let normalized_input = normalize_input_path(input_path);
  display_path_for_search_root_with_flags(
    should_preserve_dot_prefix(&normalized_input),
    display_path,
  )
}

fn display_path_for_search_root_owned(
  input_path: &str,
  display_path: String,
) -> String {
  let normalized_input = normalize_input_path(input_path);
  display_path_for_search_root_owned_with_flags(
    should_preserve_dot_prefix(&normalized_input),
    display_path,
  )
}

fn display_path_for_search_root_with_flags(
  preserve_dot_prefix: bool,
  display_path: &str,
) -> String {
  if preserve_dot_prefix && !display_path.starts_with("./") {
    format!("./{display_path}")
  } else {
    display_path.to_owned()
  }
}

fn display_path_for_search_root_owned_with_flags(
  preserve_dot_prefix: bool,
  display_path: String,
) -> String {
  if preserve_dot_prefix && !display_path.starts_with("./") {
    format!("./{display_path}")
  } else {
    display_path
  }
}

fn normalize_input_path(path: &str) -> String {
  let mut normalized = path.replace('\\', "/");
  while normalized.len() > 1 && normalized.ends_with('/') {
    normalized.pop();
  }
  if normalized.is_empty() { ".".to_owned() } else { normalized }
}

fn should_preserve_dot_prefix(path: &str) -> bool {
  path == "." || path.starts_with("./")
}

fn display_path_depth(display_path: &str) -> usize {
  display_path.strip_prefix("./").unwrap_or(display_path).split('/').count()
}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
  };

  #[cfg(unix)]
  use std::os::unix::fs::PermissionsExt;

  use ignore::WalkBuilder;

  use super::*;
  use crate::app::AppContext;

  struct TempDir {
    path: PathBuf,
  }

  impl TempDir {
    fn new(prefix: &str) -> Self {
      let unique =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
      let path = std::env::temp_dir()
        .join(format!("busybox-rg-walker-{prefix}-{unique}"));
      fs::create_dir_all(&path).unwrap();
      Self { path }
    }

    fn path(&self) -> &Path {
      &self.path
    }

    fn write(&self, relative: &str, contents: &[u8]) {
      let path = self.path.join(relative);
      if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
      }
      fs::write(path, contents).unwrap();
    }
  }

  impl Drop for TempDir {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.path);
    }
  }

  fn default_walker(cwd: &Path) -> FileWalker {
    FileWalker::new(
      cwd.to_path_buf(),
      TraversalSpec {
        hidden: false,
        no_ignore: false,
        paths: Vec::new(),
        globs: Vec::new(),
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        exclude_dirs: Vec::new(),
      },
      Vec::new(),
      SortSpec { kind: super::super::SortKind::None, reverse: false },
      TargetOrder::Deterministic,
    )
    .unwrap()
  }

  fn hidden_walker(cwd: &Path) -> FileWalker {
    FileWalker::new(
      cwd.to_path_buf(),
      TraversalSpec {
        hidden: true,
        no_ignore: false,
        paths: Vec::new(),
        globs: Vec::new(),
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        exclude_dirs: Vec::new(),
      },
      Vec::new(),
      SortSpec { kind: super::super::SortKind::None, reverse: false },
      TargetOrder::Deterministic,
    )
    .unwrap()
  }

  fn no_ignore_walker(cwd: &Path) -> FileWalker {
    FileWalker::new(
      cwd.to_path_buf(),
      TraversalSpec {
        hidden: false,
        no_ignore: true,
        paths: Vec::new(),
        globs: Vec::new(),
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        exclude_dirs: Vec::new(),
      },
      Vec::new(),
      SortSpec { kind: super::super::SortKind::None, reverse: false },
      TargetOrder::Deterministic,
    )
    .unwrap()
  }

  fn sorted_walker(cwd: &Path, reverse: bool) -> FileWalker {
    FileWalker::new(
      cwd.to_path_buf(),
      TraversalSpec {
        hidden: false,
        no_ignore: false,
        paths: Vec::new(),
        globs: Vec::new(),
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        exclude_dirs: Vec::new(),
      },
      Vec::new(),
      SortSpec { kind: super::super::SortKind::Path, reverse },
      TargetOrder::Cli,
    )
    .unwrap()
  }

  fn globbed_walker(cwd: &Path, globs: &[&str]) -> FileWalker {
    FileWalker::new(
      cwd.to_path_buf(),
      TraversalSpec {
        hidden: false,
        no_ignore: false,
        paths: Vec::new(),
        globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        exclude_dirs: Vec::new(),
      },
      globs.iter().map(|glob| (*glob).to_owned()).collect(),
      SortSpec { kind: super::super::SortKind::None, reverse: false },
      TargetOrder::Deterministic,
    )
    .unwrap()
  }

  fn expected_files(cwd: &Path, files: &[&str]) -> Vec<WalkFile> {
    files.iter().map(|path| WalkFile::for_display(cwd, path)).collect()
  }

  fn reference_walk_paths(
    cwd: &Path,
    traversal: &TraversalSpec,
    globs: &[String],
    sort: SortSpec,
    target_order: TargetOrder,
    paths: &[String],
  ) -> io::Result<Vec<WalkFile>> {
    let search_paths =
      if paths.is_empty() { vec![".".to_owned()] } else { paths.to_vec() };

    let mut files = Vec::new();
    for search_path in search_paths {
      let absolute = cwd.join(&search_path);
      let metadata = fs::symlink_metadata(&absolute).map_err(|err| {
        io::Error::new(err.kind(), format!("rg: {search_path}: {}", err))
      })?;

      if metadata.is_file() {
        files.push(WalkFile::for_display(
          cwd,
          &display_path_for_explicit_path(cwd, &search_path, &absolute),
        ));
        continue;
      }

      if !metadata.is_dir() {
        continue;
      }

      let mut builder = WalkBuilder::new(&absolute);
      builder.hidden(!traversal.hidden);
      builder.ignore(!traversal.no_ignore);
      builder.git_ignore(!traversal.no_ignore);
      builder.git_global(!traversal.no_ignore);
      builder.git_exclude(!traversal.no_ignore);
      builder.require_git(false);

      let mut walked = Vec::new();
      for entry in builder.build() {
        let entry =
          entry.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
        if !entry
          .file_type()
          .map(|file_type| file_type.is_file())
          .unwrap_or(false)
        {
          continue;
        }

        let display_path = display_path_for_search_root(
          &search_path,
          &shared_walker::normalize_display_path(cwd, &entry.into_path()),
        );
        if !super::super::glob::matches_globs(&display_path, globs) {
          continue;
        }
        walked.push(WalkFile::for_display(cwd, &display_path));
      }

      if matches!(target_order, TargetOrder::Deterministic)
        && sort.kind == super::super::SortKind::None
      {
        walked.sort_by(|left, right| {
          left
            .depth
            .cmp(&right.depth)
            .then_with(|| left.display_path.cmp(&right.display_path))
        });
      }

      files.extend(walked);
    }

    match sort.kind {
      super::super::SortKind::None => {
        if matches!(target_order, TargetOrder::Deterministic) {
          files.sort_by(|left, right| {
            left
              .depth
              .cmp(&right.depth)
              .then_with(|| left.display_path.cmp(&right.display_path))
          });
        } else {
          files.reverse();
        }
      }
      super::super::SortKind::Path => {
        files.sort_by(|left, right| left.display_path.cmp(&right.display_path));
        if sort.reverse {
          files.reverse();
        }
      }
    }

    Ok(files)
  }

  fn parallel_streamed_paths(
    ctx: &AppContext,
    walker: &FileWalker,
    paths: &[String],
    parallelism: usize,
  ) -> Vec<String> {
    let mut files = Vec::new();
    walker
      .walk_paths_streaming_parallel(ctx, paths, parallelism, |file| {
        files.push(file.display_path);
        Ok(WalkControl::Continue)
      })
      .unwrap();
    files.sort();
    files
  }

  fn assert_matches_reference(
    walker: &FileWalker,
    ctx: &AppContext,
    paths: &[String],
  ) {
    let expected = reference_walk_paths(
      walker.cwd(),
      walker.traversal(),
      walker.globs(),
      walker.sort(),
      walker.target_order(),
      paths,
    )
    .unwrap();
    let actual = walker.walk_paths(ctx, paths).unwrap();
    assert_eq!(actual, expected);
  }

  #[test]
  fn contract_recursive_walk_returns_regular_files_in_deterministic_order() {
    let dir = TempDir::new("recursive");
    dir.write("root/src/lib.rs", b"lib\n");
    dir.write("root/tests/test.rs", b"test\n");
    dir.write("root/Cargo.toml", b"[package]\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &default_walker(dir.path()),
      &ctx,
      &["root".into()],
    );
  }

  #[test]
  fn contract_dot_search_root_preserves_dot_prefix_in_display_paths() {
    let dir = TempDir::new("dot-prefix");
    dir.write("a.txt", b"x\n");
    dir.write("sub/b.txt", b"y\n");

    let ctx = AppContext::new().unwrap();
    let actual =
      default_walker(dir.path()).walk_paths(&ctx, &[".".into()]).unwrap();

    let display_paths: Vec<_> =
      actual.iter().map(|file| file.display_path.as_str()).collect();
    assert_eq!(display_paths, vec!["./a.txt", "./sub/b.txt"]);
  }

  #[test]
  fn contract_explicit_dot_prefixed_file_preserves_cli_path() {
    let dir = TempDir::new("explicit-dot-file");
    dir.write("a.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    let actual =
      default_walker(dir.path()).walk_paths(&ctx, &["./a.txt".into()]).unwrap();

    assert_eq!(actual, expected_files(dir.path(), &["./a.txt"]));
  }

  #[test]
  fn contract_missing_path_returns_not_found_error_naming_the_operand() {
    let dir = TempDir::new("missing");
    let ctx = AppContext::new().unwrap();

    let err = default_walker(dir.path())
      .walk_paths(&ctx, &["missing.txt".into()])
      .unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(err.to_string().contains("missing.txt"));
  }

  #[test]
  fn contract_multiple_operands_fail_with_the_missing_operand_name() {
    let dir = TempDir::new("missing-among-operands");
    dir.write("present.txt", b"x\n");
    let ctx = AppContext::new().unwrap();

    let err = default_walker(dir.path())
      .walk_paths(&ctx, &["present.txt".into(), "missing.txt".into()])
      .unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(err.to_string().contains("missing.txt"));
  }

  #[cfg(unix)]
  #[test]
  fn contract_permission_denied_names_the_unreadable_operand() {
    let dir = TempDir::new("permission-denied");
    dir.write("secret/file.txt", b"x\n");
    let secret_dir = dir.path().join("secret");
    let original_mode = fs::metadata(&secret_dir).unwrap().permissions().mode();
    fs::set_permissions(&secret_dir, fs::Permissions::from_mode(0o000))
      .unwrap();

    let ctx = AppContext::new().unwrap();
    let err = default_walker(dir.path())
      .walk_paths(&ctx, &["secret".into()])
      .unwrap_err();

    fs::set_permissions(&secret_dir, fs::Permissions::from_mode(original_mode))
      .unwrap();

    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("secret"));
  }

  #[test]
  fn contract_explicit_file_bypasses_hidden_and_gitignore_filters() {
    let dir = TempDir::new("explicit-file-bypass");
    dir.write(".gitignore", b"ignored.txt\n.hidden.txt\nignored-dir/\n");
    dir.write("ignored.txt", b"x\n");
    dir.write(".hidden.txt", b"x\n");
    dir.write("ignored-dir/file.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    let actual = default_walker(dir.path())
      .walk_paths(
        &ctx,
        &[
          "ignored.txt".into(),
          ".hidden.txt".into(),
          "ignored-dir/file.txt".into(),
        ],
      )
      .unwrap();

    assert_eq!(
      actual,
      expected_files(
        dir.path(),
        &["ignored.txt", ".hidden.txt", "ignored-dir/file.txt"],
      )
    );
  }

  #[test]
  fn contract_default_walk_excludes_hidden_paths() {
    let dir = TempDir::new("hidden-default");
    dir.write("visible.txt", b"x\n");
    dir.write(".hidden.txt", b"x\n");
    dir.write(".config/file.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(&default_walker(dir.path()), &ctx, &[".".into()]);
  }

  #[test]
  fn contract_hidden_mode_includes_hidden_files_and_descends_hidden_directories()
   {
    let dir = TempDir::new("hidden-enabled");
    dir.write("visible.txt", b"x\n");
    dir.write(".hidden.txt", b"x\n");
    dir.write(".config/file.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(&hidden_walker(dir.path()), &ctx, &[".".into()]);
  }

  #[test]
  fn contract_default_walk_respects_root_gitignore_patterns_and_directory_pruning()
   {
    let dir = TempDir::new("gitignore-root");
    dir.write(".gitignore", b"ignored.txt\nignored-dir/\n");
    dir.write("tracked.txt", b"x\n");
    dir.write("ignored.txt", b"x\n");
    dir.write("ignored-dir/nested.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(&default_walker(dir.path()), &ctx, &[".".into()]);
  }

  #[test]
  fn contract_default_walk_respects_nested_gitignore_relative_to_its_directory()
  {
    let dir = TempDir::new("gitignore-nested");
    dir.write("root/.gitignore", b"ignored-root.txt\nnested/\n");
    dir.write("root/ignored-root.txt", b"x\n");
    dir.write("root/kept-root.txt", b"x\n");
    dir.write("root/nested/.gitignore", b"ignored-in-nested.txt\n");
    dir.write("root/nested/ignored-in-nested.txt", b"x\n");
    dir.write("root/nested/kept-in-nested.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &default_walker(dir.path()),
      &ctx,
      &["root".into()],
    );
  }

  #[test]
  fn contract_parallel_streaming_matches_serial_results() {
    let dir = TempDir::new("parallel-stream");
    dir.write(".gitignore", b"ignored-root.txt\nvendor/\n");
    dir.write("kept.txt", b"x\n");
    dir.write("ignored-root.txt", b"x\n");
    dir.write("a/keep-a.txt", b"x\n");
    dir.write("a/.gitignore", b"ignored-a.txt\n");
    dir.write("a/ignored-a.txt", b"x\n");
    dir.write("b/keep-b.txt", b"x\n");
    dir.write("vendor/skipped.txt", b"x\n");
    dir.write("nested/deep/keep.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    let walker = FileWalker::new(
      dir.path().to_path_buf(),
      TraversalSpec {
        hidden: false,
        no_ignore: false,
        paths: Vec::new(),
        globs: Vec::new(),
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        exclude_dirs: Vec::new(),
      },
      Vec::new(),
      SortSpec { kind: super::super::SortKind::None, reverse: false },
      TargetOrder::Cli,
    )
    .unwrap();

    let mut serial = walker.walk_paths(&ctx, &[".".into()]).unwrap();
    serial.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    let serial_paths: Vec<_> =
      serial.into_iter().map(|file| file.display_path).collect();

    let parallel_paths =
      parallel_streamed_paths(&ctx, &walker, &[".".into()], 4);

    assert_eq!(parallel_paths, serial_paths);
  }

  #[test]
  fn contract_gitignore_negation_unignores_matching_files_when_parent_directory_is_walked()
   {
    let dir = TempDir::new("gitignore-negation");
    dir.write(
      ".gitignore",
      b"*.txt\n!keep.txt\nsubdir/*.txt\n!subdir/keep.txt\n",
    );
    dir.write("drop.txt", b"x\n");
    dir.write("keep.txt", b"x\n");
    dir.write("subdir/drop.txt", b"x\n");
    dir.write("subdir/keep.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(&default_walker(dir.path()), &ctx, &[".".into()]);
  }

  #[test]
  fn contract_no_ignore_mode_disables_gitignore_filtering_but_not_hidden_filtering()
   {
    let dir = TempDir::new("no-ignore");
    dir.write(".gitignore", b"ignored.txt\n");
    dir.write("tracked.txt", b"x\n");
    dir.write("ignored.txt", b"x\n");
    dir.write(".hidden.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &no_ignore_walker(dir.path()),
      &ctx,
      &[".".into()],
    );
  }

  #[test]
  fn contract_globs_filter_on_normalized_display_paths_after_traversal_rules() {
    let dir = TempDir::new("globs");
    dir.write("src/lib.rs", b"x\n");
    dir.write("src/lib.toml", b"x\n");
    dir.write("tests/test.rs", b"x\n");
    dir.write(".hidden.rs", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &globbed_walker(dir.path(), &["*.rs"]),
      &ctx,
      &[".".into()],
    );
  }

  #[test]
  fn contract_sort_path_orders_results_ascending_by_display_path() {
    let dir = TempDir::new("sort-path");
    dir.write("b.txt", b"x\n");
    dir.write("a.txt", b"x\n");
    dir.write("nested/c.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &sorted_walker(dir.path(), false),
      &ctx,
      &[".".into()],
    );
  }

  #[test]
  fn contract_sortr_path_orders_results_descending_by_display_path() {
    let dir = TempDir::new("sortr-path");
    dir.write("b.txt", b"x\n");
    dir.write("a.txt", b"x\n");
    dir.write("nested/c.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &sorted_walker(dir.path(), true),
      &ctx,
      &[".".into()],
    );
  }

  #[test]
  fn contract_gitignore_anchored_patterns_match_only_from_the_gitignore_directory_root()
   {
    let dir = TempDir::new("gitignore-anchored");
    dir.write(".gitignore", b"/root-only.txt\n");
    dir.write("root-only.txt", b"x\n");
    dir.write("subdir/root-only.txt", b"x\n");
    dir.write("subdir/kept.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(&default_walker(dir.path()), &ctx, &[".".into()]);
  }

  #[test]
  fn contract_gitignore_directory_only_patterns_prune_matching_directories() {
    let dir = TempDir::new("gitignore-dironly");
    dir.write(".gitignore", b"build/\n");
    dir.write("build/output.txt", b"x\n");
    dir.write("src/build.rs", b"x\n");
    dir.write("src/keep.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(&default_walker(dir.path()), &ctx, &[".".into()]);
  }

  #[test]
  fn contract_gitignore_comments_and_escaped_hashes_follow_gitignore_rules() {
    let dir = TempDir::new("gitignore-comments");
    dir.write(".gitignore", b"# comment\n\\#literal.txt\nignored.txt\n");
    dir.write("#literal.txt", b"x\n");
    dir.write("ignored.txt", b"x\n");
    dir.write("kept.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(&default_walker(dir.path()), &ctx, &[".".into()]);
  }

  #[test]
  fn contract_multiple_operands_preserve_operand_grouping_before_global_sorting_rules()
   {
    let dir = TempDir::new("multiple-operands");
    dir.write("a/one.txt", b"x\n");
    dir.write("b/two.txt", b"x\n");
    dir.write("b/three.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &default_walker(dir.path()),
      &ctx,
      &["a".into(), "b".into()],
    );
  }

  #[test]
  fn contract_sort_path_applies_globally_across_multiple_operands() {
    let dir = TempDir::new("multiple-operands-sorted");
    dir.write("b/root.txt", b"x\n");
    dir.write("a/nested/child.txt", b"x\n");
    dir.write("a/root.txt", b"x\n");
    dir.write("b/nested/child.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    assert_matches_reference(
      &sorted_walker(dir.path(), false),
      &ctx,
      &["b".into(), "a".into()],
    );
  }

  #[test]
  fn contract_explicit_file_operands_bypass_glob_filters() {
    let dir = TempDir::new("explicit-glob-bypass");
    dir.write("picked.toml", b"x\n");
    dir.write("dir/seen.rs", b"x\n");

    let ctx = AppContext::new().unwrap();
    let walker = globbed_walker(dir.path(), &["*.rs"]);
    assert_matches_reference(
      &walker,
      &ctx,
      &["picked.toml".into(), "dir".into()],
    );
  }
}
