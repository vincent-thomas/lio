use std::{
  collections::{HashMap, HashSet},
  ffi::{OsStr, OsString},
  io,
  path::{Component, Path, PathBuf},
  sync::Arc,
};

use ignore::{
  Match,
  gitignore::{Gitignore, GitignoreBuilder},
  overrides::Override,
};
use lio::api::{self, FileStat, FileType};

use crate::app::AppContext;
use crate::util::{fs as fs_util, io as io_util};

const PREFETCH_CHILD_BATCH_MIN: usize = 2;
const PREFETCH_CHILD_BATCH_MAX: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
  pub path: PathBuf,
  pub display_path: String,
  pub depth: usize,
  pub file_type: FileType,
}

#[derive(Debug, Clone, Copy)]
pub struct StreamWalkEntry<'a> {
  pub path: &'a Path,
  pub depth: usize,
  pub file_type: FileType,
}

#[derive(Debug, Clone)]
pub struct WalkOptions {
  pub hidden: bool,
  pub no_ignore: bool,
  pub no_ignore_vcs: bool,
  pub follow_symlinks: bool,
  pub one_file_system: bool,
  pub min_depth: Option<usize>,
  pub max_depth: Option<usize>,
  pub overrides: Option<Override>,
  pub max_filesize: Option<u64>,
  pub sort_entries: bool,
  pub prefetch_children: bool,
}

impl Default for WalkOptions {
  fn default() -> Self {
    Self {
      hidden: false,
      no_ignore: false,
      no_ignore_vcs: false,
      follow_symlinks: false,
      one_file_system: false,
      min_depth: None,
      max_depth: None,
      overrides: None,
      max_filesize: None,
      sort_entries: true,
      prefetch_children: true,
    }
  }
}

#[derive(Debug, Clone)]
pub struct FileWalker {
  cwd: PathBuf,
  options: WalkOptions,
}

#[derive(Debug, Clone)]
pub(crate) struct ParallelDirShard {
  path: PathBuf,
  depth: usize,
  relative_depth: usize,
  inherited_matchers: Arc<Vec<Gitignore>>,
  prefetched_entries: Option<Vec<crate::util::fs::DirEntry>>,
}

#[derive(Debug)]
pub(crate) struct ParallelWalkPlan {
  pub immediate_entries: Vec<WalkEntry>,
  pub dir_shards: Vec<ParallelDirShard>,
}

#[derive(Debug)]
struct DirFrame {
  path: PathBuf,
  depth: usize,
  relative_depth: usize,
  entries: Vec<(OsString, Option<FileType>)>,
  next_index: usize,
  matchers: Vec<Gitignore>,
  prefetched_children: HashMap<usize, Option<Box<DirFrame>>>,
  prefetch_attempted: bool,
  scratch_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkControl {
  Continue,
  Break,
}

#[derive(Debug, Default)]
struct WalkState {
  visited_dirs: HashSet<PathBuf>,
  device_ids: HashMap<PathBuf, u64>,
}

fn dir_entries_from_frame(frame: &DirFrame) -> Vec<crate::util::fs::DirEntry> {
  frame
    .entries
    .iter()
    .map(|(name, file_type)| crate::util::fs::DirEntry {
      name: name.clone(),
      file_type: *file_type,
    })
    .collect()
}

impl FileWalker {
  pub fn new(cwd: PathBuf, options: WalkOptions) -> Self {
    Self { cwd, options }
  }

  pub fn walk_path(
    &self,
    ctx: &AppContext,
    path: &str,
  ) -> io::Result<Vec<WalkEntry>> {
    let mut entries = Vec::new();
    self.walk_path_streaming(ctx, path, |entry| {
      entries.push(WalkEntry {
        path: entry.path.to_path_buf(),
        display_path: normalize_display_path(self.cwd(), entry.path),
        depth: entry.depth,
        file_type: entry.file_type,
      });
      Ok(WalkControl::Continue)
    })?;
    Ok(entries)
  }

  pub fn walk_path_streaming<F>(
    &self,
    ctx: &AppContext,
    path: &str,
    mut visit: F,
  ) -> io::Result<()>
  where
    F: FnMut(StreamWalkEntry<'_>) -> io::Result<WalkControl>,
  {
    let absolute_path = normalize_path(&self.cwd.join(path));
    let metadata = fs_util::stat_path(ctx, &absolute_path, false)
      .map_err(|err| self.walk_io_error(&absolute_path, err))?;
    self.walk_path_with_stat(ctx, path, metadata, &mut visit)
  }

  pub fn cwd(&self) -> &Path {
    &self.cwd
  }

  fn walk_io_error(&self, path: &Path, err: io::Error) -> io::Error {
    io::Error::new(
      err.kind(),
      format!("busybox: {}: {err}", normalize_display_path(self.cwd(), path)),
    )
  }

  fn walk_path_with_stat(
    &self,
    ctx: &AppContext,
    path: &str,
    metadata: Option<FileStat>,
    visit: &mut impl FnMut(StreamWalkEntry<'_>) -> io::Result<WalkControl>,
  ) -> io::Result<()> {
    let absolute_path = normalize_path(&self.cwd.join(path));
    let Some(metadata) = metadata else {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("busybox: {path}: No such file or directory"),
      ));
    };

    let metadata = self.resolve_followed_type(ctx, &absolute_path, metadata)?;

    if metadata.is_dir() {
      let mut matchers = Vec::new();
      let mut state = WalkState::default();
      let root_device = if self.options.one_file_system {
        Some(self.device_id_cached(&mut state, &absolute_path)?)
      } else {
        None
      };
      self.walk_directory_iterative(
        ctx,
        absolute_path,
        0,
        root_device,
        &mut state,
        &mut matchers,
        visit,
      )?;
      return Ok(());
    }

    let depth = display_path_depth(&self.cwd, &absolute_path);
    if !self.relative_depth_included(0) {
      return Ok(());
    }

    let _ = visit(StreamWalkEntry {
      path: &absolute_path,
      depth,
      file_type: metadata.file_type,
    })?;
    Ok(())
  }

  fn walk_directory_iterative(
    &self,
    ctx: &AppContext,
    absolute_path: PathBuf,
    starting_relative_depth: usize,
    root_device: Option<u64>,
    state: &mut WalkState,
    matchers: &mut Vec<Gitignore>,
    visit: &mut impl FnMut(StreamWalkEntry<'_>) -> io::Result<WalkControl>,
  ) -> io::Result<WalkControl> {
    let root_depth = display_path_depth(&self.cwd, &absolute_path);
    let Some(root_frame) = self.prepare_directory_frame(
      ctx,
      absolute_path,
      root_depth,
      starting_relative_depth,
      state,
    )?
    else {
      return Ok(WalkControl::Continue);
    };

    let mut stack = vec![root_frame];
    while let Some(frame) = stack.last_mut() {
      if frame.next_index == 0 && !frame.matchers.is_empty() {
        matchers.extend(frame.matchers.iter().cloned());
      }
      if self.options.prefetch_children && !frame.prefetch_attempted {
        frame.prefetch_attempted = true;
        frame.prefetched_children = self.prefetch_child_frames(
          ctx,
          frame.path.as_path(),
          frame.depth,
          frame.relative_depth,
          &frame.entries,
          state,
          matchers,
          root_device,
        )?;
      }

      if frame.next_index >= frame.entries.len() {
        for _ in 0..frame.matchers.len() {
          matchers.pop();
        }
        stack.pop();
        continue;
      }

      let (name, file_type) = &frame.entries[frame.next_index];
      let file_type = *file_type;
      frame.scratch_path.clone_from(&frame.path);
      frame.scratch_path.push(name);
      frame.next_index += 1;

      let Some(file_type) = file_type else {
        continue;
      };
      let is_dir = matches!(file_type, FileType::Directory);
      let override_match =
        self.override_match(frame.scratch_path.as_path(), is_dir);
      if matches!(override_match, Some(false)) {
        continue;
      }

      if !matches!(override_match, Some(true))
        && self.is_ignored(matchers, frame.scratch_path.as_path(), is_dir)
      {
        continue;
      }

      if self.should_skip_hidden(name) {
        continue;
      }

      if self.should_skip_filesize(
        ctx,
        frame.scratch_path.as_path(),
        file_type,
      )? {
        continue;
      }

      let child_relative_depth = frame.relative_depth + 1;
      if self.relative_depth_included(child_relative_depth) {
        let control = visit(StreamWalkEntry {
          path: frame.scratch_path.as_path(),
          depth: frame.depth + 1,
          file_type,
        })?;
        if control == WalkControl::Break {
          return Ok(WalkControl::Break);
        }
      }

      if is_dir {
        if root_device.is_some_and(|root_device| {
          self.device_id_cached(state, frame.scratch_path.as_path()).ok()
            != Some(root_device)
        }) {
          continue;
        }

        let child_frame = if let Some(prefetched) =
          frame.prefetched_children.remove(&(frame.next_index - 1))
        {
          prefetched.map(|frame| *frame)
        } else {
          self.prepare_directory_frame(
            ctx,
            frame.scratch_path.clone(),
            frame.depth + 1,
            child_relative_depth,
            state,
          )?
        };

        if let Some(child_frame) = child_frame {
          stack.push(child_frame);
        }
      }
    }

    Ok(WalkControl::Continue)
  }

  pub(crate) fn build_parallel_walk_plan(
    &self,
    ctx: &AppContext,
    path: &str,
  ) -> io::Result<ParallelWalkPlan> {
    let absolute_path = normalize_path(&self.cwd.join(path));
    let metadata = fs_util::stat_path(ctx, &absolute_path, false)
      .map_err(|err| self.walk_io_error(&absolute_path, err))?;
    let Some(metadata) = metadata else {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("busybox: {path}: No such file or directory"),
      ));
    };
    let metadata = self.resolve_followed_type(ctx, &absolute_path, metadata)?;

    if !metadata.is_dir() {
      let depth = display_path_depth(&self.cwd, &absolute_path);
      let mut immediate_entries = Vec::new();
      if self.relative_depth_included(0) {
        immediate_entries.push(WalkEntry {
          path: absolute_path,
          display_path: normalize_display_path(
            self.cwd(),
            &self.cwd.join(path),
          ),
          depth,
          file_type: metadata.file_type,
        });
      }
      return Ok(ParallelWalkPlan {
        immediate_entries,
        dir_shards: Vec::new(),
      });
    }

    let mut state = WalkState::default();
    let root_device = if self.options.one_file_system {
      Some(self.device_id_cached(&mut state, &absolute_path)?)
    } else {
      None
    };
    let root_depth = display_path_depth(&self.cwd, &absolute_path);
    let Some(root_frame) = self.prepare_directory_frame(
      ctx,
      absolute_path.clone(),
      root_depth,
      0,
      &mut state,
    )?
    else {
      return Ok(ParallelWalkPlan {
        immediate_entries: Vec::new(),
        dir_shards: Vec::new(),
      });
    };

    let mut immediate_entries = Vec::new();
    let mut dir_shards = Vec::new();
    let inherited_matchers = Arc::new(root_frame.matchers.clone());
    let mut prefetched_children = self.prefetch_child_frames(
      ctx,
      root_frame.path.as_path(),
      root_frame.depth,
      root_frame.relative_depth,
      &root_frame.entries,
      &mut state,
      inherited_matchers.as_ref(),
      root_device,
    )?;
    let mut scratch_path = root_frame.path.clone();
    for (index, (name, file_type)) in root_frame.entries.iter().enumerate() {
      let Some(file_type) = *file_type else {
        continue;
      };

      scratch_path.clone_from(&root_frame.path);
      scratch_path.push(name);
      let is_dir = matches!(file_type, FileType::Directory);
      let override_match = self.override_match(scratch_path.as_path(), is_dir);
      if matches!(override_match, Some(false)) {
        continue;
      }
      if !matches!(override_match, Some(true))
        && self.is_ignored(
          inherited_matchers.as_ref(),
          scratch_path.as_path(),
          is_dir,
        )
      {
        continue;
      }
      if self.should_skip_hidden(name.as_os_str()) {
        continue;
      }
      if self.should_skip_filesize(ctx, scratch_path.as_path(), file_type)? {
        continue;
      }
      if is_dir
        && root_device.is_some_and(|root_device| {
          self.device_id_cached(&mut state, scratch_path.as_path()).ok()
            != Some(root_device)
        })
      {
        continue;
      }

      let child_relative_depth = 1;
      let child_depth = root_frame.depth + 1;
      if self.relative_depth_included(child_relative_depth) {
        immediate_entries.push(WalkEntry {
          display_path: normalize_display_path(
            self.cwd(),
            scratch_path.as_path(),
          ),
          path: scratch_path.clone(),
          depth: child_depth,
          file_type,
        });
      }

      if is_dir {
        dir_shards.push(ParallelDirShard {
          path: scratch_path.clone(),
          depth: child_depth,
          relative_depth: child_relative_depth,
          inherited_matchers: Arc::clone(&inherited_matchers),
          prefetched_entries: prefetched_children.remove(&index).and_then(
            |frame| frame.map(|frame| dir_entries_from_frame(&frame)),
          ),
        });
      }
    }

    Ok(ParallelWalkPlan { immediate_entries, dir_shards })
  }

  pub(crate) fn walk_parallel_dir_shard<F>(
    &self,
    ctx: &AppContext,
    shard: ParallelDirShard,
    mut visit: F,
  ) -> io::Result<WalkControl>
  where
    F: FnMut(WalkEntry) -> io::Result<WalkControl>,
  {
    let mut matchers = shard.inherited_matchers.as_ref().clone();
    let mut state = WalkState::default();
    let root_device = if self.options.one_file_system {
      Some(self.device_id_cached(&mut state, &shard.path)?)
    } else {
      None
    };

    self.walk_directory_iterative(
      ctx,
      shard.path,
      shard.relative_depth,
      root_device,
      &mut state,
      &mut matchers,
      &mut |entry| {
        visit(WalkEntry {
          display_path: normalize_display_path(self.cwd(), entry.path),
          path: entry.path.to_path_buf(),
          depth: entry.depth,
          file_type: entry.file_type,
        })
      },
    )
  }

  pub(crate) fn split_parallel_dir_shard(
    &self,
    ctx: &AppContext,
    shard: ParallelDirShard,
  ) -> io::Result<(Vec<WalkEntry>, Vec<ParallelDirShard>)> {
    let ParallelDirShard {
      path,
      depth,
      relative_depth,
      inherited_matchers,
      prefetched_entries,
    } = shard;
    let mut state = WalkState::default();
    let root_device = if self.options.one_file_system {
      Some(self.device_id_cached(&mut state, &path)?)
    } else {
      None
    };

    let frame = if let Some(prefetched_entries) = prefetched_entries {
      self.prepare_directory_frame_from_entries(
        ctx,
        path.clone(),
        depth,
        relative_depth,
        &mut state,
        prefetched_entries,
      )?
    } else {
      self.prepare_directory_frame(
        ctx,
        path.clone(),
        depth,
        relative_depth,
        &mut state,
      )?
    };
    let Some(frame) = frame else {
      return Ok((Vec::new(), Vec::new()));
    };

    let combined_matchers = if frame.matchers.is_empty() {
      Arc::clone(&inherited_matchers)
    } else {
      let mut combined_matchers = inherited_matchers.as_ref().clone();
      combined_matchers.extend(frame.matchers.iter().cloned());
      Arc::new(combined_matchers)
    };

    let mut prefetched_children = self.prefetch_child_frames(
      ctx,
      frame.path.as_path(),
      frame.depth,
      frame.relative_depth,
      &frame.entries,
      &mut state,
      &combined_matchers,
      root_device,
    )?;
    let mut immediate_entries = Vec::new();
    let mut dir_shards = Vec::new();
    let mut scratch_path = frame.path.clone();
    for (index, (name, file_type)) in frame.entries.iter().enumerate() {
      let Some(file_type) = *file_type else {
        continue;
      };

      scratch_path.clone_from(&frame.path);
      scratch_path.push(name);
      let is_dir = matches!(file_type, FileType::Directory);
      let override_match = self.override_match(scratch_path.as_path(), is_dir);
      if matches!(override_match, Some(false)) {
        continue;
      }
      if !matches!(override_match, Some(true))
        && self.is_ignored(&combined_matchers, scratch_path.as_path(), is_dir)
      {
        continue;
      }
      if self.should_skip_hidden(name.as_os_str()) {
        continue;
      }
      if self.should_skip_filesize(ctx, scratch_path.as_path(), file_type)? {
        continue;
      }
      if is_dir
        && root_device.is_some_and(|root_device| {
          self.device_id_cached(&mut state, scratch_path.as_path()).ok()
            != Some(root_device)
        })
      {
        continue;
      }

      let child_relative_depth = frame.relative_depth + 1;
      let child_depth = frame.depth + 1;
      if self.relative_depth_included(child_relative_depth) {
        immediate_entries.push(WalkEntry {
          display_path: normalize_display_path(
            self.cwd(),
            scratch_path.as_path(),
          ),
          path: scratch_path.clone(),
          depth: child_depth,
          file_type,
        });
      }

      if is_dir {
        dir_shards.push(ParallelDirShard {
          path: scratch_path.clone(),
          depth: child_depth,
          relative_depth: child_relative_depth,
          inherited_matchers: Arc::clone(&combined_matchers),
          prefetched_entries: prefetched_children.remove(&index).and_then(
            |frame| frame.map(|frame| dir_entries_from_frame(&frame)),
          ),
        });
      }
    }

    Ok((immediate_entries, dir_shards))
  }

  pub(crate) fn split_parallel_dir_shards(
    &self,
    ctx: &AppContext,
    shards: Vec<ParallelDirShard>,
  ) -> io::Result<Vec<(Vec<WalkEntry>, Vec<ParallelDirShard>)>> {
    if shards.is_empty() {
      return Ok(Vec::new());
    }

    let mut prefetched_entries: Vec<Option<Vec<crate::util::fs::DirEntry>>> =
      shards.iter().map(|shard| shard.prefetched_entries.clone()).collect();
    let read_indices: Vec<_> = shards
      .iter()
      .enumerate()
      .filter_map(|(index, shard)| {
        shard.prefetched_entries.is_none().then_some(index)
      })
      .collect();
    if !read_indices.is_empty() {
      let paths: Vec<_> =
        read_indices.iter().map(|&index| shards[index].path.clone()).collect();
      let read_results = fs_util::read_dir_paths(ctx, &paths)?;
      for (index, entries) in
        read_indices.into_iter().zip(read_results.into_iter())
      {
        prefetched_entries[index] = Some(entries);
      }
    }
    let mut results = Vec::with_capacity(shards.len());

    for (shard, dir_entries) in
      shards.into_iter().zip(prefetched_entries.into_iter())
    {
      let mut state = WalkState::default();
      let root_device = if self.options.one_file_system {
        Some(self.device_id_cached(&mut state, &shard.path)?)
      } else {
        None
      };

      let Some(frame) = self.prepare_directory_frame_from_entries(
        ctx,
        shard.path.clone(),
        shard.depth,
        shard.relative_depth,
        &mut state,
        dir_entries.expect("missing prefetched directory entries"),
      )?
      else {
        results.push((Vec::new(), Vec::new()));
        continue;
      };

      let combined_matchers = if frame.matchers.is_empty() {
        Arc::clone(&shard.inherited_matchers)
      } else {
        let mut combined_matchers = shard.inherited_matchers.as_ref().clone();
        combined_matchers.extend(frame.matchers.iter().cloned());
        Arc::new(combined_matchers)
      };

      let mut prefetched_children = self.prefetch_child_frames(
        ctx,
        frame.path.as_path(),
        frame.depth,
        frame.relative_depth,
        &frame.entries,
        &mut state,
        &combined_matchers,
        root_device,
      )?;
      let mut immediate_entries = Vec::new();
      let mut child_shards = Vec::new();
      let mut scratch_path = frame.path.clone();
      for (index, (name, file_type)) in frame.entries.iter().enumerate() {
        let Some(file_type) = *file_type else {
          continue;
        };

        scratch_path.clone_from(&frame.path);
        scratch_path.push(name);
        let is_dir = matches!(file_type, FileType::Directory);
        let override_match =
          self.override_match(scratch_path.as_path(), is_dir);
        if matches!(override_match, Some(false)) {
          continue;
        }
        if !matches!(override_match, Some(true))
          && self.is_ignored(&combined_matchers, scratch_path.as_path(), is_dir)
        {
          continue;
        }
        if self.should_skip_hidden(name.as_os_str()) {
          continue;
        }
        if self.should_skip_filesize(ctx, scratch_path.as_path(), file_type)? {
          continue;
        }
        if is_dir
          && root_device.is_some_and(|root_device| {
            self.device_id_cached(&mut state, scratch_path.as_path()).ok()
              != Some(root_device)
          })
        {
          continue;
        }

        let child_relative_depth = frame.relative_depth + 1;
        let child_depth = frame.depth + 1;
        if self.relative_depth_included(child_relative_depth) {
          immediate_entries.push(WalkEntry {
            display_path: normalize_display_path(
              self.cwd(),
              scratch_path.as_path(),
            ),
            path: scratch_path.clone(),
            depth: child_depth,
            file_type,
          });
        }

        if is_dir {
          child_shards.push(ParallelDirShard {
            path: scratch_path.clone(),
            depth: child_depth,
            relative_depth: child_relative_depth,
            inherited_matchers: Arc::clone(&combined_matchers),
            prefetched_entries: prefetched_children.remove(&index).and_then(
              |frame| frame.map(|frame| dir_entries_from_frame(&frame)),
            ),
          });
        }
      }

      results.push((immediate_entries, child_shards));
    }

    Ok(results)
  }

  fn prepare_directory_frame(
    &self,
    ctx: &AppContext,
    absolute_path: PathBuf,
    depth: usize,
    relative_depth: usize,
    state: &mut WalkState,
  ) -> io::Result<Option<DirFrame>> {
    if self.options.follow_symlinks {
      let canonical = absolute_path
        .as_path()
        .canonicalize()
        .unwrap_or_else(|_| absolute_path.clone());
      if !state.visited_dirs.insert(canonical) {
        return Ok(None);
      }
    }

    if self
      .options
      .max_depth
      .is_some_and(|max_depth| relative_depth >= max_depth)
    {
      return Ok(None);
    }

    let dir_entries = fs_util::read_dir_path(ctx, absolute_path.as_path())
      .map_err(|err| self.walk_io_error(&absolute_path, err))?;
    self.prepare_directory_frame_from_entries(
      ctx,
      absolute_path,
      depth,
      relative_depth,
      state,
      dir_entries,
    )
  }

  fn prepare_directory_frame_from_entries(
    &self,
    ctx: &AppContext,
    absolute_path: PathBuf,
    depth: usize,
    relative_depth: usize,
    _state: &mut WalkState,
    dir_entries: Vec<crate::util::fs::DirEntry>,
  ) -> io::Result<Option<DirFrame>> {
    let mut dir_entries: Vec<_> = dir_entries
      .into_iter()
      .map(|entry| (entry.name, entry.file_type))
      .collect();
    if self.options.sort_entries {
      dir_entries.sort_by(|left, right| left.0.cmp(&right.0));
    }

    let mut unknown_paths = Vec::new();
    let mut symlink_paths = Vec::new();
    let mut has_gitignore = false;
    let mut has_ignore = false;
    let mut has_git_dir = false;
    let mut scratch_path = absolute_path.clone();
    for (name, file_type) in &dir_entries {
      match *file_type {
        Some(file_type) if file_type != FileType::Unknown => {
          if matches!(file_type, FileType::File) {
            has_gitignore |=
              !self.options.no_ignore_vcs && name == OsStr::new(".gitignore");
            has_ignore |= name == OsStr::new(".ignore");
          } else if matches!(file_type, FileType::Directory) {
            has_git_dir |= name == OsStr::new(".git");
          }
        }
        _ => {
          scratch_path.clone_from(&absolute_path);
          scratch_path.push(name);
          unknown_paths.push(scratch_path.clone());
        }
      }
    }

    let mut unknown_stats = if unknown_paths.is_empty() {
      Vec::new().into_iter()
    } else {
      self.stat_paths(ctx, &unknown_paths, false)?.into_iter()
    };
    let mut resolved_entries = Vec::with_capacity(dir_entries.len());
    for (name, file_type) in dir_entries {
      let file_type = match file_type {
        Some(file_type) if file_type != FileType::Unknown => Some(file_type),
        _ => unknown_stats.next().flatten().map(|stat| stat.file_type),
      };
      if matches!(file_type, Some(FileType::File)) {
        has_gitignore |=
          !self.options.no_ignore_vcs && name == OsStr::new(".gitignore");
        has_ignore |= name == OsStr::new(".ignore");
      } else if matches!(file_type, Some(FileType::Directory)) {
        has_git_dir |= name == OsStr::new(".git");
      } else if self.options.follow_symlinks
        && file_type == Some(FileType::Symlink)
      {
        scratch_path.clone_from(&absolute_path);
        scratch_path.push(&name);
        symlink_paths.push(scratch_path.clone());
      }
      resolved_entries.push((name, file_type));
    }

    if self.options.follow_symlinks {
      let mut symlink_stats = if symlink_paths.is_empty() {
        Vec::new().into_iter()
      } else {
        self.stat_paths(ctx, &symlink_paths, true)?.into_iter()
      };
      for (_name, file_type) in &mut resolved_entries {
        if *file_type == Some(FileType::Symlink) {
          if let Some(stat) = symlink_stats.next().flatten() {
            *file_type = Some(stat.file_type);
          }
        }
      }
    }

    let matchers = self.load_matchers_from_presence(
      ctx,
      absolute_path.as_path(),
      has_gitignore,
      has_ignore,
      has_git_dir,
    )?;
    Ok(Some(DirFrame {
      scratch_path: absolute_path.clone(),
      path: absolute_path,
      depth,
      relative_depth,
      entries: resolved_entries,
      next_index: 0,
      matchers,
      prefetched_children: HashMap::new(),
      prefetch_attempted: false,
    }))
  }

  fn prefetch_child_frames(
    &self,
    ctx: &AppContext,
    parent_path: &Path,
    depth: usize,
    relative_depth: usize,
    entries: &[(OsString, Option<FileType>)],
    state: &mut WalkState,
    matchers: &[Gitignore],
    root_device: Option<u64>,
  ) -> io::Result<HashMap<usize, Option<Box<DirFrame>>>> {
    if self
      .options
      .max_depth
      .is_some_and(|max_depth| relative_depth + 1 >= max_depth)
    {
      return Ok(HashMap::new());
    }

    let mut child_indices = Vec::new();
    let mut child_paths = Vec::new();

    for (idx, (name, file_type)) in entries.iter().enumerate() {
      if *file_type != Some(FileType::Directory) {
        continue;
      }
      let child_path = parent_path.join(name);
      let override_match = self.override_match(child_path.as_path(), true);
      if matches!(override_match, Some(false)) {
        continue;
      }
      if !matches!(override_match, Some(true))
        && self.is_ignored(matchers, child_path.as_path(), true)
      {
        continue;
      }
      if self.should_skip_hidden(name.as_os_str()) {
        continue;
      }
      if root_device.is_some_and(|root_device| {
        self.device_id_cached(state, child_path.as_path()).ok()
          != Some(root_device)
      }) {
        continue;
      }
      child_indices.push(idx);
      child_paths.push(child_path);
    }

    if child_paths.is_empty() {
      return Ok(HashMap::new());
    }

    if child_paths.len() < PREFETCH_CHILD_BATCH_MIN {
      return Ok(HashMap::new());
    }
    if child_paths.len() > PREFETCH_CHILD_BATCH_MAX {
      child_indices.truncate(PREFETCH_CHILD_BATCH_MAX);
      child_paths.truncate(PREFETCH_CHILD_BATCH_MAX);
    }

    let child_entries =
      fs_util::read_dir_paths(ctx, &child_paths).map_err(|err| {
        let failed_path = child_paths.iter().find_map(|path| {
          fs_util::read_dir_path(ctx, path).err().map(|_| path.as_path())
        });
        self.walk_io_error(failed_path.unwrap_or(parent_path), err)
      })?;
    let mut prefetched = HashMap::with_capacity(child_indices.len());
    for ((idx, child_path), dir_entries) in child_indices
      .into_iter()
      .zip(child_paths.into_iter())
      .zip(child_entries.into_iter())
    {
      let frame = self
        .prepare_directory_frame_from_entries(
          ctx,
          child_path,
          depth + 1,
          relative_depth + 1,
          state,
          dir_entries,
        )?
        .map(Box::new);
      prefetched.insert(idx, frame);
    }
    Ok(prefetched)
  }

  fn should_skip_hidden(&self, name: &OsStr) -> bool {
    !self.options.hidden && is_hidden_name(name)
  }

  fn override_match(&self, path: &Path, is_dir: bool) -> Option<bool> {
    let Some(overrides) = &self.options.overrides else {
      return None;
    };
    let override_path = path.strip_prefix(self.cwd()).unwrap_or(path);
    match overrides.matched(override_path, is_dir) {
      Match::Ignore(_) => Some(false),
      Match::Whitelist(_) => Some(true),
      Match::None => None,
    }
  }

  fn is_ignored(
    &self,
    matchers: &[Gitignore],
    path: &Path,
    is_dir: bool,
  ) -> bool {
    if self.options.no_ignore {
      return false;
    }

    for matcher in matchers.iter().rev() {
      match matcher.matched(path, is_dir) {
        Match::Ignore(_) => return true,
        Match::Whitelist(_) => return false,
        Match::None => {}
      }
    }
    false
  }

  fn should_skip_filesize(
    &self,
    ctx: &AppContext,
    path: &Path,
    file_type: FileType,
  ) -> io::Result<bool> {
    let Some(max_filesize) = self.options.max_filesize else {
      return Ok(false);
    };
    if matches!(file_type, FileType::Directory) {
      return Ok(false);
    }
    let Some(stat) = fs_util::stat_path(ctx, path, false)
      .map_err(|err| self.walk_io_error(path, err))?
    else {
      return Ok(false);
    };
    Ok(stat.size > max_filesize)
  }

  fn relative_depth_included(&self, depth: usize) -> bool {
    self.options.min_depth.is_none_or(|min_depth| depth >= min_depth)
  }

  fn load_ignore_file(
    &self,
    ctx: &AppContext,
    dir: &Path,
    ignore_path: &Path,
  ) -> io::Result<Gitignore> {
    let cpath = fs_util::path_to_cstring(ignore_path)?;
    let fd = io_util::run(
      ctx.lio(),
      lio::api::openat(&ctx.cwd(), cpath, libc::O_RDONLY, 0)
        .with_lio(ctx.lio())
        .send(),
    )
    .map_err(|err| self.walk_io_error(ignore_path, err))?;
    let bytes = io_util::read_to_bytes_fd(ctx.lio(), &fd)
      .map_err(|err| self.walk_io_error(ignore_path, err))?;
    let mut builder = GitignoreBuilder::new(dir);

    for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
      let line =
        if index == 0 { line.trim_start_matches('\u{feff}') } else { line };
      builder.add_line(Some(ignore_path.to_path_buf()), line).map_err(
        |err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()),
      )?;
    }

    builder.build().map_err(|err| {
      io::Error::new(io::ErrorKind::InvalidData, err.to_string())
    })
  }

  fn load_matchers_from_presence(
    &self,
    ctx: &AppContext,
    dir: &Path,
    has_gitignore: bool,
    has_ignore: bool,
    has_git_dir: bool,
  ) -> io::Result<Vec<Gitignore>> {
    if self.options.no_ignore {
      return Ok(Vec::new());
    }

    let mut matchers = Vec::new();

    if !self.options.no_ignore_vcs && has_git_dir {
      let exclude_path = dir.join(".git").join("info").join("exclude");
      if exclude_path.is_file() {
        matchers.push(self.load_ignore_file(ctx, dir, &exclude_path)?);
      }
    }

    if has_gitignore {
      matchers.push(self.load_ignore_file(
        ctx,
        dir,
        &dir.join(".gitignore"),
      )?);
    }

    if has_ignore {
      matchers.push(self.load_ignore_file(ctx, dir, &dir.join(".ignore"))?);
    }

    Ok(matchers)
  }

  fn resolve_followed_type(
    &self,
    ctx: &AppContext,
    path: &Path,
    metadata: FileStat,
  ) -> io::Result<FileStat> {
    if !self.options.follow_symlinks || !metadata.is_symlink() {
      return Ok(metadata);
    }

    Ok(
      fs_util::stat_path(ctx, path, true)
        .map_err(|err| self.walk_io_error(path, err))?
        .unwrap_or(metadata),
    )
  }

  fn stat_paths(
    &self,
    ctx: &AppContext,
    paths: &[PathBuf],
    follow_symlinks: bool,
  ) -> io::Result<Vec<Option<FileStat>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut inflight = 0usize;
    let mut remaining = paths.len();
    let mut results: Vec<Option<io::Result<Option<FileStat>>>> =
      Vec::with_capacity(paths.len());
    results.resize_with(paths.len(), || None);

    for (idx, path) in paths.iter().enumerate() {
      let sender = tx.clone();
      api::statat(&ctx.cwd(), fs_util::path_to_cstring(path)?, follow_symlinks)
        .with_lio(ctx.lio())
        .when_done(move |result| {
          let _ = sender.send((idx, result));
        });
      inflight += 1;
    }

    while remaining > 0 {
      let mut progressed = false;
      while let Ok((idx, result)) = rx.try_recv() {
        progressed = true;
        inflight = inflight.saturating_sub(1);
        results[idx] = Some(match result {
          Ok(stat) => Ok(Some(stat)),
          Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
          Err(err) => Err(self.walk_io_error(&paths[idx], err)),
        });
        remaining -= 1;
      }

      if remaining == 0 {
        break;
      }

      if !progressed {
        if inflight == 0 {
          return Err(io::Error::other(
            "stat_paths stalled without in-flight operations",
          ));
        }
        if ctx.lio().try_run()? == 0 {
          ctx.lio().run()?;
        }
      }
    }

    results
      .into_iter()
      .map(|result| result.expect("missing stat result"))
      .collect()
  }

  fn device_id_cached(
    &self,
    state: &mut WalkState,
    path: &Path,
  ) -> io::Result<u64> {
    if let Some(device_id) = state.device_ids.get(path) {
      return Ok(*device_id);
    }

    let device_id = device_id(path)?;
    state.device_ids.insert(path.to_path_buf(), device_id);
    Ok(device_id)
  }
}

fn display_path_depth(cwd: &Path, path: &Path) -> usize {
  path.strip_prefix(cwd).unwrap_or(path).components().count()
}

fn device_id(path: &Path) -> io::Result<u64> {
  let cpath = fs_util::path_to_cstring(path)?;
  let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
  let result = unsafe { libc::stat(cpath.as_ptr(), stat.as_mut_ptr()) };
  if result != 0 {
    return Err(io::Error::last_os_error());
  }

  let stat = unsafe { stat.assume_init() };
  Ok(stat.st_dev as u64)
}

#[cfg(unix)]
fn is_hidden_name(name: &OsStr) -> bool {
  use std::os::unix::ffi::OsStrExt;

  name.as_bytes().first() == Some(&b'.')
}

#[cfg(not(unix))]
fn is_hidden_name(name: &OsStr) -> bool {
  name.to_string_lossy().as_ref().starts_with('.')
}

pub fn normalize_display_path(cwd: &Path, path: &Path) -> String {
  path.strip_prefix(cwd).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

pub fn normalize_path(path: &Path) -> PathBuf {
  path
    .components()
    .filter(|component| !matches!(component, Component::CurDir))
    .collect()
}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
  };

  use super::*;
  use crate::app::AppContext;

  struct TempDir {
    path: PathBuf,
  }

  impl TempDir {
    fn new(prefix: &str) -> Self {
      Self::new_under(std::env::temp_dir(), "busybox-walker", prefix)
    }

    fn new_short(prefix: &str) -> Self {
      Self::new_under(PathBuf::from("/tmp"), "bbw", prefix)
    }

    fn new_under(base: PathBuf, namespace: &str, prefix: &str) -> Self {
      let unique =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
      let path = base.join(format!("{namespace}-{prefix}-{unique}"));
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

  fn walker(cwd: &Path, options: WalkOptions) -> FileWalker {
    FileWalker::new(cwd.to_path_buf(), options)
  }

  fn paths(entries: Vec<WalkEntry>) -> Vec<String> {
    entries.into_iter().map(|entry| entry.display_path).collect()
  }

  #[test]
  fn contract_no_ignore_vcs_still_applies_dot_ignore() {
    let dir = TempDir::new("no-ignore-vcs");
    dir.write(".gitignore", b"gitignored.txt\n");
    dir.write(".ignore", b"ignored.txt\n");
    dir.write("gitignored.txt", b"x\n");
    dir.write("ignored.txt", b"x\n");
    dir.write("kept.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    let entries = walker(
      dir.path(),
      WalkOptions {
        hidden: false,
        no_ignore: false,
        no_ignore_vcs: true,
        follow_symlinks: false,
        one_file_system: false,
        min_depth: None,
        max_depth: None,
        overrides: None,
        max_filesize: None,
        sort_entries: true,
        prefetch_children: true,
      },
    )
    .walk_path(&ctx, ".")
    .unwrap();

    assert_eq!(paths(entries), vec!["gitignored.txt", "kept.txt"]);
  }

  #[test]
  fn contract_max_depth_one_still_descends_into_same_filesystem_children() {
    let dir = TempDir::new("same-filesystem");
    dir.write("root/child/file.txt", b"x\n");
    dir.write("root/top.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    let entries = walker(
      dir.path(),
      WalkOptions {
        hidden: false,
        no_ignore: false,
        no_ignore_vcs: false,
        follow_symlinks: false,
        one_file_system: true,
        min_depth: None,
        max_depth: Some(1),
        overrides: None,
        max_filesize: None,
        sort_entries: true,
        prefetch_children: true,
      },
    )
    .walk_path(&ctx, "root")
    .unwrap();

    assert_eq!(paths(entries), vec!["root/child", "root/top.txt"]);
  }

  #[test]
  fn contract_streaming_walk_can_stop_early() {
    let dir = TempDir::new("stream-break");
    dir.write("root/a.txt", b"x\n");
    dir.write("root/subdir/b.txt", b"x\n");
    dir.write("root/subdir/c.txt", b"x\n");

    let ctx = AppContext::new().unwrap();
    let mut seen = Vec::new();
    walker(dir.path(), WalkOptions::default())
      .walk_path_streaming(&ctx, "root", |entry| {
        seen.push(normalize_display_path(dir.path(), entry.path));
        Ok(WalkControl::Break)
      })
      .unwrap();

    assert_eq!(seen, vec!["root/a.txt"]);
  }

  #[test]
  fn contract_streaming_walk_handles_deep_directory_trees_without_recursion() {
    let dir = TempDir::new_short("deep");
    let depth = 300usize;
    let mut relative = String::from("root");
    for _ in 0..depth {
      relative.push_str("/a");
    }
    dir.write(&format!("{relative}/file.txt"), b"x\n");

    let ctx = AppContext::new().unwrap();
    let mut last_seen = String::new();
    walker(dir.path(), WalkOptions::default())
      .walk_path_streaming(&ctx, "root", |entry| {
        if entry.file_type == FileType::File {
          last_seen = normalize_display_path(dir.path(), entry.path);
        }
        Ok(WalkControl::Continue)
      })
      .unwrap();

    assert_eq!(last_seen, format!("{relative}/file.txt"));
  }

  #[test]
  fn normalize_path_strips_curdir_components() {
    assert_eq!(
      normalize_path(Path::new("/tmp/example/./nested/file.txt")),
      PathBuf::from("/tmp/example/nested/file.txt")
    );
  }
}
