use std::{
  io,
  io::IsTerminal,
  io::Write,
  path::{Path, PathBuf},
  sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
  },
  thread,
  time::Duration,
};

use crossbeam_deque::{Injector, Steal, Stealer, Worker as DequeWorker};
use lio::api::{self, FileType as LioFileType};
use regex::RegexBuilder;

use crate::{
  app::AppContext,
  command::Command,
  util::{
    io as io_util, process as process_util,
    walker::{self as shared_walker, StreamWalkEntry},
  },
};

const OUTPUT_BATCH_BYTES: usize = 4 * 1024 * 1024;
const FILE_MODE_STAT_BATCH_SIZE: usize = 96;
const RENDER_ENTRY_BATCH_SIZE: usize = 16_384;
const LOCAL_DIR_TASK_THRESHOLD: usize = 32;
const TRAVERSAL_TASK_BURST_SIZE: usize = 4;
const DIR_TASK_BATCH_SIZE: usize = 16;
const FULL_TRAVERSAL_TASK_BURST_SIZE: usize = 24;
const FULL_DIR_TASK_BATCH_SIZE: usize = 64;
const PARALLEL_RESULT_CHUNK_SIZE: usize = 4096;
const FULL_TRAVERSAL_RESULT_CHUNK_SIZE: usize = 16_384;
const IDLE_WAIT_TIMEOUT: Duration = Duration::from_millis(2);
#[derive(Debug, Clone, Default)]
pub struct FdCommand {
  pub pattern: Option<String>,
  pub exclude: Vec<String>,
  pub extensions: Vec<String>,
  pub glob: bool,
  pub file_type: Option<FileType>,
  pub hidden: bool,
  pub case_insensitive: bool,
  pub follow_symlinks: bool,
  pub no_ignore: bool,
  pub no_ignore_vcs: bool,
  pub min_depth: Option<usize>,
  pub max_depth: Option<usize>,
  pub max_results: Option<usize>,
  pub full_path: bool,
  pub absolute_path: bool,
  pub print0: bool,
  pub color_output: Option<bool>,
  pub one_file_system: bool,
  pub sort_results: bool,
  pub exec: Option<Vec<String>>,
  pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
  File,
  Directory,
  Symlink,
}

impl Command for FdCommand {
  fn name() -> &'static str {
    "fd"
  }

  fn aliases() -> &'static [&'static str] {
    &[]
  }

  fn summary() -> &'static str {
    "Find entries in the filesystem."
  }

  fn usage() -> &'static str {
    "fd [options] [pattern] [path...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    parse_fd_command(args)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let color_output = self.color_output.unwrap_or_else(|| stdout_is_tty(ctx));
    let search_paths = if self.paths.is_empty() {
      vec![".".to_string()]
    } else {
      self.paths.clone()
    };
    let stdout = ctx.stdout();

    for path in &search_paths {
      search_path(ctx, Path::new(path), self, color_output, &stdout)?;
    }
    Ok(())
  }
}

fn parse_fd_command(args: &[String]) -> io::Result<FdCommand> {
  let mut cmd = FdCommand::default();
  let mut index = 0;
  let mut parse_options = true;

  while index < args.len() {
    let arg = &args[index];
    if !parse_options {
      push_fd_positional(&mut cmd, arg.clone());
      index += 1;
      continue;
    }

    match arg.as_str() {
      "-H" | "--hidden" => {
        cmd.hidden = true;
        index += 1;
      }
      "-i" | "--ignore-case" => {
        cmd.case_insensitive = true;
        index += 1;
      }
      "-L" | "--follow" => {
        cmd.follow_symlinks = true;
        index += 1;
      }
      "-g" | "--glob" => {
        cmd.glob = true;
        index += 1;
      }
      "-I" | "--no-ignore" => {
        cmd.no_ignore = true;
        index += 1;
      }
      "--no-ignore-vcs" => {
        cmd.no_ignore_vcs = true;
        index += 1;
      }
      "-t" | "--type" => {
        if index + 1 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --type requires an argument",
          ));
        }
        cmd.file_type = Some(match args[index + 1].as_str() {
          "f" | "file" => FileType::File,
          "d" | "dir" | "directory" => FileType::Directory,
          "l" | "symlink" => FileType::Symlink,
          other => {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              format!("fd: invalid file type '{}'", other),
            ));
          }
        });
        index += 2;
      }
      "-d" | "--max-depth" => {
        if index + 1 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --max-depth requires an argument",
          ));
        }
        cmd.max_depth = Some(args[index + 1].parse().map_err(|_| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd: invalid depth '{}'", args[index + 1]),
          )
        })?);
        index += 2;
      }
      "--min-depth" => {
        if index + 1 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --min-depth requires an argument",
          ));
        }
        cmd.min_depth = Some(args[index + 1].parse().map_err(|_| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd: invalid depth '{}'", args[index + 1]),
          )
        })?);
        index += 2;
      }
      "--max-results" => {
        if index + 1 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --max-results requires an argument",
          ));
        }
        cmd.max_results = Some(args[index + 1].parse().map_err(|_| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd: invalid max results '{}'", args[index + 1]),
          )
        })?);
        index += 2;
      }
      "-e" | "--extension" => {
        if index + 1 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --extension requires an argument",
          ));
        }
        let extension = args[index + 1].trim_start_matches('.');
        if extension.is_empty() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --extension requires a non-empty argument",
          ));
        }
        cmd.extensions.push(extension.to_string());
        index += 2;
      }
      "-E" | "--exclude" => {
        if index + 1 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --exclude requires an argument",
          ));
        }
        cmd.exclude.push(args[index + 1].clone());
        index += 2;
      }
      "-p" | "--full-path" => {
        cmd.full_path = true;
        index += 1;
      }
      "-a" | "--absolute-path" => {
        cmd.absolute_path = true;
        index += 1;
      }
      "-0" | "--print0" => {
        cmd.print0 = true;
        index += 1;
      }
      "-C" | "--color" => {
        cmd.color_output = Some(true);
        index += 1;
      }
      "-M" | "--no-color" => {
        cmd.color_output = Some(false);
        index += 1;
      }
      "--one-file-system" => {
        cmd.one_file_system = true;
        index += 1;
      }
      "--sort" => {
        cmd.sort_results = true;
        index += 1;
      }
      "-x" | "--exec" => {
        if index + 1 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --exec requires a command",
          ));
        }
        // Collect all args until we hit ';' or end of args
        let mut exec_cmd = Vec::new();
        index += 1;
        while index < args.len() {
          if args[index] == ";" {
            index += 1;
            break;
          }
          exec_cmd.push(args[index].clone());
          index += 1;
        }
        if exec_cmd.is_empty() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd: --exec requires a command",
          ));
        }
        cmd.exec = Some(exec_cmd);
      }
      "--" => {
        parse_options = false;
        index += 1;
      }
      arg if arg.starts_with('-') => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("fd: unknown option '{}'", arg),
        ));
      }
      _ => {
        push_fd_positional(&mut cmd, arg.clone());
        index += 1;
      }
    }
  }

  Ok(cmd)
}

fn push_fd_positional(cmd: &mut FdCommand, arg: String) {
  if cmd.pattern.is_none() && !looks_like_explicit_fd_path(&arg) {
    cmd.pattern = Some(arg);
    return;
  }

  cmd.paths.push(arg);
}

fn looks_like_explicit_fd_path(arg: &str) -> bool {
  arg.starts_with('/') || arg.starts_with("./") || arg.starts_with("../")
}

fn search_path(
  ctx: &AppContext,
  path: &Path,
  options: &FdCommand,
  color_output: bool,
  stdout: &lio::api::resource::Resource,
) -> io::Result<()> {
  let matcher = PatternMatcher::new(options)?;
  let walker = shared_walker::FileWalker::new(
    std::env::current_dir()?,
    shared_walker::WalkOptions {
      hidden: options.hidden,
      no_ignore: options.no_ignore,
      no_ignore_vcs: options.no_ignore_vcs,
      follow_symlinks: options.follow_symlinks,
      one_file_system: options.one_file_system,
      min_depth: options.min_depth,
      max_depth: options.max_depth,
      overrides: None,
      max_filesize: None,
      sort_entries: options.sort_results,
      prefetch_children: true,
    },
  );
  if !options.sort_results {
    return search_path_parallel(
      ctx,
      path,
      options,
      color_output,
      stdout,
      &walker,
      &matcher,
    );
  }
  let mut matched = 0usize;
  let mut output_buf = OutputBuffer::with_capacity(OUTPUT_BATCH_BYTES);
  let need_modes = color_output && !options.print0 && options.exec.is_none();
  let mut pending = Vec::with_capacity(FILE_MODE_STAT_BATCH_SIZE);
  let search_root = path.to_string_lossy();
  walker.walk_path_streaming(ctx, &search_root, |entry| {
    if !entry_matches_options(&entry, options, &matcher) {
      return Ok(shared_walker::WalkControl::Continue);
    }

    matched += emit_or_queue_entry(
      ctx,
      options,
      stdout,
      walker.cwd(),
      entry.path.to_path_buf(),
      entry.file_type,
      None,
      need_modes,
      &mut pending,
      &mut output_buf,
    )?;
    if options.max_results.is_some_and(|max| matched >= max) {
      return Ok(shared_walker::WalkControl::Break);
    }

    Ok(shared_walker::WalkControl::Continue)
  })?;

  matched += flush_pending_entries(
    ctx,
    options,
    stdout,
    walker.cwd(),
    need_modes,
    options.max_results.map(|max| max.saturating_sub(matched)),
    &mut pending,
    &mut output_buf,
  )?;
  if options.max_results.is_some_and(|max| matched >= max) {
    flush_output_buffer(ctx, stdout, &mut output_buf)?;
    return Ok(());
  }

  flush_output_buffer(ctx, stdout, &mut output_buf)?;
  Ok(())
}

#[derive(Debug, Clone)]
struct MatchedEntry {
  path: PathBuf,
  file_type: LioFileType,
  mode: Option<u32>,
}

enum ParallelOutput {
  Entries(Vec<MatchedEntry>),
}

#[derive(Clone)]
struct ParallelTask {
  shard: shared_walker::ParallelDirShard,
}

#[derive(Default)]
struct ParallelWakeState {
  epoch: Mutex<u64>,
  condvar: Condvar,
}

enum NextParallelTask {
  Task(ParallelTask),
  Retry,
  Empty,
}

fn pop_next_parallel_task(
  worker_index: usize,
  local_tasks: &DequeWorker<ParallelTask>,
  global_tasks: &Injector<ParallelTask>,
  stealers: &[Stealer<ParallelTask>],
) -> NextParallelTask {
  if let Some(task) = local_tasks.pop() {
    return NextParallelTask::Task(task);
  }

  match global_tasks.steal() {
    Steal::Success(task) => NextParallelTask::Task(task),
    Steal::Retry => NextParallelTask::Retry,
    Steal::Empty => {
      for (index, stealer) in stealers.iter().enumerate() {
        if index == worker_index {
          continue;
        }
        match stealer.steal_batch_and_pop(local_tasks) {
          Steal::Success(task) => return NextParallelTask::Task(task),
          Steal::Retry => return NextParallelTask::Retry,
          Steal::Empty => {}
        }
      }
      NextParallelTask::Empty
    }
  }
}

fn enqueue_parallel_child_tasks(
  local_tasks: &DequeWorker<ParallelTask>,
  global_tasks: &Injector<ParallelTask>,
  outstanding_tasks: &std::sync::atomic::AtomicUsize,
  wake_state: &ParallelWakeState,
  child_tasks: impl IntoIterator<Item = ParallelTask>,
) {
  let child_tasks: Vec<_> = child_tasks.into_iter().collect();
  if child_tasks.is_empty() {
    return;
  }

  outstanding_tasks.fetch_add(child_tasks.len(), Ordering::AcqRel);
  let mut local_kept = 0usize;
  for child in child_tasks {
    if local_tasks.len() < LOCAL_DIR_TASK_THRESHOLD && local_kept < 8 {
      local_tasks.push(child);
      local_kept += 1;
    } else {
      global_tasks.push(child);
    }
  }

  if local_tasks.len() > LOCAL_DIR_TASK_THRESHOLD * 2 {
    let spill = local_tasks.len().saturating_sub(LOCAL_DIR_TASK_THRESHOLD);
    for _ in 0..spill {
      if let Some(task) = local_tasks.pop() {
        global_tasks.push(task);
      }
    }
  }

  if let Ok(mut epoch) = wake_state.epoch.lock() {
    *epoch = epoch.wrapping_add(1);
    wake_state.condvar.notify_one();
  }
}

fn notify_all_parallel_workers(wake_state: &ParallelWakeState) {
  if let Ok(mut epoch) = wake_state.epoch.lock() {
    *epoch = epoch.wrapping_add(1);
    wake_state.condvar.notify_all();
  }
}

fn wait_for_parallel_work(
  wake_state: &ParallelWakeState,
  stop: &AtomicBool,
  outstanding_tasks: &std::sync::atomic::AtomicUsize,
) {
  let Ok(epoch) = wake_state.epoch.lock() else {
    return;
  };
  let observed = *epoch;
  if stop.load(Ordering::Acquire)
    || outstanding_tasks.load(Ordering::Acquire) == 0
  {
    return;
  }
  let _ =
    wake_state.condvar.wait_timeout_while(epoch, IDLE_WAIT_TIMEOUT, |epoch| {
      !stop.load(Ordering::Acquire)
        && outstanding_tasks.load(Ordering::Acquire) > 0
        && *epoch == observed
    });
}

fn collect_matched_entry_modes(
  ctx: &AppContext,
  need_modes: bool,
  entries: &mut [MatchedEntry],
) -> io::Result<()> {
  if !need_modes {
    return Ok(());
  }

  let file_indices: Vec<_> = entries
    .iter()
    .enumerate()
    .filter_map(|(index, entry)| {
      (entry.file_type == LioFileType::File && entry.mode.is_none())
        .then_some(index)
    })
    .collect();
  if file_indices.is_empty() {
    return Ok(());
  }

  let receivers: io::Result<Vec<_>> = file_indices
    .iter()
    .map(|&index| {
      Ok(
        api::statat(
          &ctx.cwd(),
          crate::util::fs::path_to_cstring(&entries[index].path)?,
          false,
        )
        .with_lio(ctx.lio())
        .send(),
      )
    })
    .collect();
  let stats = io_util::run_all(ctx.lio(), receivers?);
  for (index, result) in file_indices.into_iter().zip(stats) {
    entries[index].mode = Some(result?.mode);
  }
  Ok(())
}

fn pop_parallel_task_batch(
  worker_index: usize,
  local_tasks: &DequeWorker<ParallelTask>,
  global_tasks: &Injector<ParallelTask>,
  stealers: &[Stealer<ParallelTask>],
  max_tasks: usize,
) -> (Vec<ParallelTask>, bool) {
  let mut tasks = Vec::with_capacity(max_tasks);
  let mut saw_retry = false;
  while tasks.len() < max_tasks {
    match pop_next_parallel_task(
      worker_index,
      local_tasks,
      global_tasks,
      stealers,
    ) {
      NextParallelTask::Task(task) => tasks.push(task),
      NextParallelTask::Retry => {
        saw_retry = true;
        if !tasks.is_empty() {
          break;
        }
      }
      NextParallelTask::Empty => break,
    }
  }
  (tasks, saw_retry)
}

fn search_path_parallel(
  ctx: &AppContext,
  path: &Path,
  options: &FdCommand,
  color_output: bool,
  stdout: &lio::api::resource::Resource,
  walker: &shared_walker::FileWalker,
  matcher: &PatternMatcher,
) -> io::Result<()> {
  let relative = shared_walker::normalize_display_path(
    walker.cwd(),
    &walker.cwd().join(path),
  );
  let plan = walker.build_parallel_walk_plan(ctx, &relative)?;
  let mut matched = 0usize;
  let mut output_buf = OutputBuffer::with_capacity(OUTPUT_BATCH_BYTES);
  let need_modes = color_output && !options.print0 && options.exec.is_none();
  let mut pending = Vec::with_capacity(FILE_MODE_STAT_BATCH_SIZE);

  for entry in plan.immediate_entries {
    let stream_entry = StreamWalkEntry {
      path: &entry.path,
      depth: entry.depth,
      file_type: entry.file_type,
    };
    if !entry_matches_options(&stream_entry, options, matcher) {
      continue;
    }
    matched += emit_or_queue_entry(
      ctx,
      options,
      stdout,
      walker.cwd(),
      entry.path,
      entry.file_type,
      None,
      need_modes,
      &mut pending,
      &mut output_buf,
    )?;
    if options.max_results.is_some_and(|max| matched >= max) {
      flush_pending_entries(
        ctx,
        options,
        stdout,
        walker.cwd(),
        need_modes,
        Some(0),
        &mut pending,
        &mut output_buf,
      )?;
      flush_output_buffer(ctx, stdout, &mut output_buf)?;
      return Ok(());
    }
  }

  if plan.dir_shards.is_empty() {
    flush_pending_entries(
      ctx,
      options,
      stdout,
      walker.cwd(),
      need_modes,
      options.max_results.map(|max| max.saturating_sub(matched)),
      &mut pending,
      &mut output_buf,
    )?;
    flush_output_buffer(ctx, stdout, &mut output_buf)?;
    return Ok(());
  }

  let tasks: Vec<_> =
    plan.dir_shards.into_iter().map(|shard| ParallelTask { shard }).collect();
  let worker_count = std::thread::available_parallelism()
    .map(|count| count.get())
    .unwrap_or(1)
    .min(12)
    .min(tasks.len())
    .max(1);

  let stop = Arc::new(AtomicBool::new(false));
  let (tx, rx) = mpsc::channel::<io::Result<ParallelOutput>>();
  let walker = walker.clone();
  let matcher = matcher.clone();
  let options = options.clone();
  let remaining_budget = options.max_results.map(|max| {
    Arc::new(std::sync::atomic::AtomicUsize::new(max.saturating_sub(matched)))
  });
  let full_traversal = options.max_results.is_none();
  let outstanding_tasks =
    Arc::new(std::sync::atomic::AtomicUsize::new(tasks.len()));
  let wake_state = Arc::new(ParallelWakeState::default());
  let global_tasks = Arc::new(Injector::<ParallelTask>::new());
  for task in tasks {
    global_tasks.push(task);
  }
  let local_queues: Vec<_> =
    (0..worker_count).map(|_| DequeWorker::new_fifo()).collect();
  let stealers = Arc::new(
    local_queues
      .iter()
      .map(DequeWorker::stealer)
      .collect::<Vec<Stealer<ParallelTask>>>(),
  );
  let mut handles = Vec::with_capacity(worker_count);
  for (worker_index, local_tasks) in local_queues.into_iter().enumerate() {
    let tx = tx.clone();
    let walker = walker.clone();
    let matcher = matcher.clone();
    let options = options.clone();
    let stop = Arc::clone(&stop);
    let remaining_budget = remaining_budget.as_ref().map(Arc::clone);
    let outstanding_tasks = Arc::clone(&outstanding_tasks);
    let wake_state = Arc::clone(&wake_state);
    let global_tasks = Arc::clone(&global_tasks);
    let stealers = Arc::clone(&stealers);
    handles.push(thread::spawn(move || {
      let ctx = match AppContext::new() {
        Ok(ctx) => ctx,
        Err(err) => {
          let _ = tx.send(Err(err));
          return;
        }
      };
      loop {
        if stop.load(Ordering::Relaxed) {
          break;
        }

        let mut traversal_budget = if full_traversal {
          FULL_TRAVERSAL_TASK_BURST_SIZE
        } else if local_tasks.len() < FILE_MODE_STAT_BATCH_SIZE {
          LOCAL_DIR_TASK_THRESHOLD * 2
        } else {
          TRAVERSAL_TASK_BURST_SIZE
        };
        let mut progressed = false;

        while traversal_budget > 0 {
          traversal_budget -= 1;
          let (tasks, saw_retry) = pop_parallel_task_batch(
            worker_index,
            &local_tasks,
            global_tasks.as_ref(),
            stealers.as_ref(),
            if full_traversal {
              FULL_DIR_TASK_BATCH_SIZE
            } else {
              DIR_TASK_BATCH_SIZE
            },
          );
          if tasks.is_empty() {
            if saw_retry {
              continue;
            }
            break;
          }

          progressed = true;
          let task_count = tasks.len();
          let result_chunk_size = if full_traversal {
            FULL_TRAVERSAL_RESULT_CHUNK_SIZE
          } else {
            PARALLEL_RESULT_CHUNK_SIZE
          };
          let mut chunk = Vec::with_capacity(result_chunk_size);
          let mut completed_tasks = 0usize;
          let result = (|| -> io::Result<()> {
            let split = walker.split_parallel_dir_shards(
              &ctx,
              tasks.into_iter().map(|task| task.shard).collect(),
            )?;

            for (entries, child_tasks) in split {
              enqueue_parallel_child_tasks(
                &local_tasks,
                global_tasks.as_ref(),
                &outstanding_tasks,
                wake_state.as_ref(),
                child_tasks.into_iter().map(|shard| ParallelTask { shard }),
              );
              outstanding_tasks.fetch_sub(1, Ordering::AcqRel);
              completed_tasks += 1;

              for entry in entries {
                if stop.load(Ordering::Relaxed) {
                  break;
                }
                let stream_entry = StreamWalkEntry {
                  path: &entry.path,
                  depth: entry.depth,
                  file_type: entry.file_type,
                };
                if !entry_matches_options(&stream_entry, &options, &matcher) {
                  continue;
                }
                if let Some(remaining_budget) = remaining_budget.as_ref() {
                  if remaining_budget
                    .try_update(
                      Ordering::AcqRel,
                      Ordering::Acquire,
                      |remaining| remaining.checked_sub(1),
                    )
                    .is_err()
                  {
                    stop.store(true, Ordering::Relaxed);
                    break;
                  }
                  if remaining_budget.load(Ordering::Acquire) == 0 {
                    stop.store(true, Ordering::Relaxed);
                  }
                }
                chunk.push(MatchedEntry {
                  path: entry.path,
                  file_type: entry.file_type,
                  mode: None,
                });
                if chunk.len() >= result_chunk_size {
                  collect_matched_entry_modes(&ctx, need_modes, &mut chunk)?;
                  tx.send(Ok(ParallelOutput::Entries(std::mem::take(
                    &mut chunk,
                  ))))
                  .map_err(|err| {
                    io::Error::other(format!(
                      "fd: traversal queue failed: {err}"
                    ))
                  })?;
                }
              }
            }
            Ok(())
          })();

          match result {
            Ok(()) => {
              if !chunk.is_empty() {
                if let Err(err) =
                  collect_matched_entry_modes(&ctx, need_modes, &mut chunk)
                {
                  let _ = tx.send(Err(err));
                  return;
                }
              }
              if !chunk.is_empty()
                && tx.send(Ok(ParallelOutput::Entries(chunk))).is_err()
              {
                break;
              }
            }
            Err(err) => {
              let remaining = task_count.saturating_sub(completed_tasks);
              if remaining > 0 {
                outstanding_tasks.fetch_sub(remaining, Ordering::AcqRel);
              }
              notify_all_parallel_workers(wake_state.as_ref());
              let _ = tx.send(Err(err));
              return;
            }
          }
        }

        if progressed {
          continue;
        }

        if outstanding_tasks.load(Ordering::Acquire) == 0 {
          break;
        }

        wait_for_parallel_work(
          wake_state.as_ref(),
          stop.as_ref(),
          outstanding_tasks.as_ref(),
        );
      }
    }));
  }
  drop(tx);

  let mut first_error = None;
  while let Ok(result) = rx.recv() {
    match result {
      Ok(ParallelOutput::Entries(entries)) => {
        for entry in entries {
          matched += emit_or_queue_entry(
            ctx,
            &options,
            stdout,
            walker.cwd(),
            entry.path,
            entry.file_type,
            entry.mode,
            need_modes,
            &mut pending,
            &mut output_buf,
          )?;
          if options.max_results.is_some_and(|max| matched >= max) {
            stop.store(true, Ordering::Relaxed);
            notify_all_parallel_workers(wake_state.as_ref());
            break;
          }
        }
        if stop.load(Ordering::Relaxed) {
          break;
        }
      }
      Err(err) => {
        stop.store(true, Ordering::Relaxed);
        notify_all_parallel_workers(wake_state.as_ref());
        first_error = Some(err);
        break;
      }
    }
  }

  stop.store(true, Ordering::Relaxed);
  notify_all_parallel_workers(wake_state.as_ref());
  for handle in handles {
    let _ = handle.join();
  }

  if let Some(err) = first_error {
    return Err(err);
  }

  flush_pending_entries(
    ctx,
    &options,
    stdout,
    walker.cwd(),
    need_modes,
    options.max_results.map(|max| max.saturating_sub(matched)),
    &mut pending,
    &mut output_buf,
  )?;
  flush_output_buffer(ctx, stdout, &mut output_buf)?;
  Ok(())
}

fn emit_or_queue_entry(
  ctx: &AppContext,
  options: &FdCommand,
  stdout: &lio::api::resource::Resource,
  cwd: &Path,
  entry_path: PathBuf,
  file_type: LioFileType,
  mode: Option<u32>,
  need_modes: bool,
  pending: &mut Vec<PendingEntry>,
  output_buf: &mut OutputBuffer,
) -> io::Result<usize> {
  pending.push(PendingEntry {
    stat_path: (need_modes && file_type == LioFileType::File && mode.is_none())
      .then_some(entry_path.clone()),
    entry_path,
    file_type,
    mode,
  });

  let batch_size = pending_entry_batch_size(options, need_modes);
  if pending.len() >= batch_size {
    return flush_pending_entries(
      ctx, options, stdout, cwd, need_modes, None, pending, output_buf,
    );
  }

  Ok(0)
}

#[derive(Debug)]
struct PendingEntry {
  stat_path: Option<PathBuf>,
  entry_path: PathBuf,
  file_type: LioFileType,
  mode: Option<u32>,
}

fn pending_entry_batch_size(options: &FdCommand, need_modes: bool) -> usize {
  if options.exec.is_some() {
    1
  } else if need_modes {
    FILE_MODE_STAT_BATCH_SIZE
  } else {
    RENDER_ENTRY_BATCH_SIZE
  }
}

fn flush_pending_entries(
  ctx: &AppContext,
  options: &FdCommand,
  stdout: &lio::api::resource::Resource,
  cwd: &Path,
  need_modes: bool,
  remaining_limit: Option<usize>,
  pending: &mut Vec<PendingEntry>,
  output_buf: &mut OutputBuffer,
) -> io::Result<usize> {
  if pending.is_empty() {
    return Ok(0);
  }

  if let Some(limit) = remaining_limit {
    if limit == 0 {
      pending.clear();
      return Ok(0);
    }
    if pending.len() > limit {
      pending.truncate(limit);
    }
  }

  collect_file_modes(ctx, need_modes, pending)?;
  let mut emitted = 0usize;

  for entry in pending.drain(..) {
    emitted += emit_entry(
      ctx,
      options,
      stdout,
      cwd,
      &entry.entry_path,
      entry.file_type,
      need_modes,
      entry.mode,
      output_buf,
    )?;
    if remaining_limit.is_some_and(|limit| emitted >= limit) {
      break;
    }
  }

  Ok(emitted)
}

fn emit_entry(
  ctx: &AppContext,
  options: &FdCommand,
  stdout: &lio::api::resource::Resource,
  cwd: &Path,
  entry_path: &Path,
  file_type: LioFileType,
  color_output: bool,
  mode: Option<u32>,
  output_buf: &mut OutputBuffer,
) -> io::Result<usize> {
  if let Some(exec_cmd) = &options.exec {
    let output_path = output_entry_path(cwd, entry_path, file_type, options);
    flush_output_buffer(ctx, stdout, output_buf)?;
    execute_command(ctx, &output_path, exec_cmd)?;
  } else {
    if color_output {
      append_colorized_output_path_bytes(
        output_buf,
        cwd,
        entry_path,
        file_type,
        options.absolute_path,
        mode,
      );
    } else {
      append_output_path_bytes(
        output_buf,
        cwd,
        entry_path,
        file_type,
        options.absolute_path,
      );
    }
    output_buf.push_byte(if options.print0 { b'\0' } else { b'\n' });
    flush_output_buffer_if_needed(ctx, stdout, output_buf)?;
  }

  Ok(1)
}

fn output_entry_path(
  cwd: &Path,
  entry_path: &Path,
  file_type: LioFileType,
  options: &FdCommand,
) -> String {
  let display_path = if options.absolute_path {
    entry_path
  } else {
    entry_path.strip_prefix(cwd).unwrap_or(entry_path)
  };
  let mut path = String::with_capacity(
    display_path.as_os_str().len()
      + usize::from(file_type == LioFileType::Directory),
  );
  append_normalized_path_string(&mut path, display_path);
  if file_type == LioFileType::Directory {
    path.push('/');
  }
  path
}

fn append_output_path_bytes(
  output_buf: &mut OutputBuffer,
  cwd: &Path,
  entry_path: &Path,
  file_type: LioFileType,
  absolute_path: bool,
) {
  output_buf.set_style(None);
  let display_path = if absolute_path {
    entry_path
  } else {
    entry_path.strip_prefix(cwd).unwrap_or(entry_path)
  };
  append_normalized_path_bytes(output_buf, display_path);
  if file_type == LioFileType::Directory {
    output_buf.push_byte(b'/');
  }
}

fn append_colorized_output_path_bytes(
  output: &mut OutputBuffer,
  cwd: &Path,
  entry_path: &Path,
  file_type: LioFileType,
  absolute_path: bool,
  mode: Option<u32>,
) {
  let display_path = if absolute_path {
    entry_path
  } else {
    entry_path.strip_prefix(cwd).unwrap_or(entry_path)
  };
  append_colorized_path_bytes_lio(
    output,
    &display_path.to_string_lossy(),
    file_type,
    mode,
  );
}

fn append_normalized_path_string(output: &mut String, path: &Path) {
  append_normalized_str_string(output, &path.to_string_lossy());
}

fn append_normalized_path_bytes(output: &mut OutputBuffer, path: &Path) {
  append_normalized_str_bytes(output, &path.to_string_lossy());
}

fn append_normalized_str_string(output: &mut String, value: &str) {
  if !value.contains('\\') {
    output.push_str(value);
    return;
  }

  for ch in value.chars() {
    output.push(if ch == '\\' { '/' } else { ch });
  }
}

fn append_normalized_str_bytes(output: &mut OutputBuffer, value: &str) {
  if !value.contains('\\') {
    output.push_raw_bytes(value.as_bytes());
    return;
  }

  for ch in value.chars() {
    let mut buf = [0; 4];
    let rendered = if ch == '\\' { "/" } else { ch.encode_utf8(&mut buf) };
    output.push_raw_bytes(rendered.as_bytes());
  }
}

fn entry_matches_options(
  entry: &StreamWalkEntry<'_>,
  options: &FdCommand,
  matcher: &PatternMatcher,
) -> bool {
  if let Some(filter_type) = options.file_type {
    let matches = match filter_type {
      FileType::File => entry.file_type == LioFileType::File,
      FileType::Directory => entry.file_type == LioFileType::Directory,
      FileType::Symlink => entry.file_type == LioFileType::Symlink,
    };
    if !matches {
      return false;
    }
  }

  if !options.extensions.is_empty() {
    if entry.file_type == LioFileType::Directory {
      return false;
    }

    let Some(entry_ext) = entry.path.extension().and_then(|e| e.to_str())
    else {
      return false;
    };

    let matches_extension = if options.case_insensitive {
      options.extensions.iter().any(|ext| entry_ext.eq_ignore_ascii_case(ext))
    } else {
      options.extensions.iter().any(|ext| entry_ext == ext)
    };

    if !matches_extension {
      return false;
    }
  }

  let basename = entry.path.file_name().map(|n| n.to_string_lossy());
  let full_path = options.full_path.then(|| entry.path.to_string_lossy());

  if matcher
    .is_excluded_parts(basename.as_deref().unwrap_or(""), full_path.as_deref())
  {
    return false;
  }

  matcher.matches_parts(basename.as_deref().unwrap_or(""), full_path.as_deref())
}

const COLOR_RESET: &str = "\x1b[0m";
const COLOR_DIRECTORY: &str = "\x1b[38;5;81m";
const COLOR_SYMLINK: &str = "\x1b[38;5;203m";
const COLOR_EXECUTABLE: &str = "\x1b[1;38;5;210m";

fn stdout_is_tty(ctx: &AppContext) -> bool {
  let _ = ctx;
  std::io::stdout().is_terminal()
}

#[cfg(test)]
fn colorize_entry_path(
  path: &str,
  entry_path: &Path,
  file_type: std::fs::FileType,
) -> String {
  let separator = std::path::MAIN_SEPARATOR;
  let trimmed = path.strip_suffix(separator).unwrap_or(path);
  let trailing_separator = path.ends_with(separator);
  if trimmed.is_empty() {
    return path.to_string();
  }

  let mut rendered = String::new();
  if file_type.is_dir() {
    rendered.push_str(COLOR_DIRECTORY);
    rendered.push_str(trimmed);
    if trailing_separator {
      rendered.push(separator);
    }
    rendered.push_str(COLOR_RESET);
    return rendered;
  }

  let (parent, basename) = match trimmed.rfind(separator) {
    Some(index) => {
      let split = index + separator.len_utf8();
      (&trimmed[..split], &trimmed[split..])
    }
    None => ("", trimmed),
  };

  if !parent.is_empty() {
    rendered.push_str(COLOR_DIRECTORY);
    rendered.push_str(parent);
    rendered.push_str(COLOR_RESET);
  }

  if let Some(color) = classify_file_color(entry_path, file_type) {
    rendered.push_str(color);
    rendered.push_str(basename);
    rendered.push_str(COLOR_RESET);
  } else {
    rendered.push_str(basename);
  }

  rendered
}
fn append_colorized_path_bytes_lio(
  output: &mut OutputBuffer,
  path: &str,
  file_type: LioFileType,
  mode: Option<u32>,
) {
  if path.is_empty() {
    return;
  }

  if file_type == LioFileType::Directory {
    output.set_style(Some(COLOR_DIRECTORY));
    append_normalized_str_bytes(output, path);
    output.push_byte(b'/');
    return;
  }

  let split = path
    .rmatch_indices(['/', '\\'])
    .next()
    .map(|(index, sep)| index + sep.len());
  let (parent, basename) = match split {
    Some(index) => (&path[..index], &path[index..]),
    None => ("", path),
  };

  if !parent.is_empty() {
    output.set_style(Some(COLOR_DIRECTORY));
    append_normalized_str_bytes(output, parent);
  }

  if let Some(color) = classify_file_color_lio(file_type, mode) {
    output.set_style(Some(color));
    append_normalized_str_bytes(output, basename);
  } else {
    output.set_style(None);
    append_normalized_str_bytes(output, basename);
  }
}

#[cfg(test)]
fn classify_file_color(
  entry_path: &Path,
  file_type: std::fs::FileType,
) -> Option<&'static str> {
  if file_type.is_symlink() {
    return Some(COLOR_SYMLINK);
  }

  if is_executable_file(entry_path, file_type) {
    return Some(COLOR_EXECUTABLE);
  }

  if file_type.is_file() {
    return None;
  }

  None
}

fn classify_file_color_lio(
  file_type: LioFileType,
  mode: Option<u32>,
) -> Option<&'static str> {
  if file_type == LioFileType::Symlink {
    return Some(COLOR_SYMLINK);
  }

  if mode.is_some_and(is_executable_mode) {
    return Some(COLOR_EXECUTABLE);
  }

  if file_type == LioFileType::File {
    return None;
  }

  None
}

#[cfg(test)]
fn is_executable_file(entry_path: &Path, file_type: std::fs::FileType) -> bool {
  is_executable_path(entry_path, file_type.is_file())
}

fn is_executable_mode(mode: u32) -> bool {
  mode & 0o111 != 0
}

#[cfg(test)]
fn is_executable_path(entry_path: &Path, is_file: bool) -> bool {
  if !is_file {
    return false;
  }

  use std::ffi::CString;

  let path = entry_path.to_string_lossy();
  let Ok(path) = CString::new(path.as_bytes()) else {
    return false;
  };

  unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

fn collect_file_modes(
  ctx: &AppContext,
  need_modes: bool,
  entries: &mut [PendingEntry],
) -> io::Result<()> {
  if !need_modes {
    return Ok(());
  }

  let stat_indices: Vec<_> = entries
    .iter()
    .enumerate()
    .filter_map(|(index, entry)| entry.stat_path.as_ref().map(|_| index))
    .collect();
  if stat_indices.is_empty() {
    return Ok(());
  }

  let receivers: io::Result<Vec<_>> = stat_indices
    .iter()
    .map(|&index| &entries[index])
    .map(|entry| {
      Ok(
        api::statat(
          &ctx.cwd(),
          crate::util::fs::path_to_cstring(
            entry.stat_path.as_ref().expect("missing stat path"),
          )?,
          false,
        )
        .with_lio(ctx.lio())
        .send(),
      )
    })
    .collect();
  let stats = io_util::run_all(ctx.lio(), receivers?);
  for (index, result) in stat_indices.into_iter().zip(stats) {
    entries[index].mode = Some(result?.mode);
  }
  Ok(())
}

fn flush_output_buffer(
  ctx: &AppContext,
  stdout: &lio::api::resource::Resource,
  output_buf: &mut OutputBuffer,
) -> io::Result<()> {
  output_buf.flush(ctx, stdout, true)
}

fn flush_output_buffer_if_needed(
  ctx: &AppContext,
  stdout: &lio::api::resource::Resource,
  output_buf: &mut OutputBuffer,
) -> io::Result<()> {
  if output_buf.len() < OUTPUT_BATCH_BYTES {
    return Ok(());
  }
  output_buf.flush(ctx, stdout, false)
}

struct OutputBuffer {
  bytes: Vec<u8>,
  active_style: Option<&'static str>,
}

impl OutputBuffer {
  fn with_capacity(capacity: usize) -> Self {
    Self { bytes: Vec::with_capacity(capacity), active_style: None }
  }

  fn len(&self) -> usize {
    self.bytes.len()
  }

  fn is_empty(&self) -> bool {
    self.bytes.is_empty()
  }

  fn set_style(&mut self, style: Option<&'static str>) {
    if self.active_style == style {
      return;
    }
    if self.active_style.is_some() {
      self.bytes.extend_from_slice(COLOR_RESET.as_bytes());
    }
    if let Some(style) = style {
      self.bytes.extend_from_slice(style.as_bytes());
    }
    self.active_style = style;
  }

  fn push_raw_bytes(&mut self, bytes: &[u8]) {
    self.bytes.extend_from_slice(bytes);
  }

  fn push_byte(&mut self, byte: u8) {
    self.bytes.push(byte);
  }

  fn flush(
    &mut self,
    _ctx: &AppContext,
    _stdout: &lio::api::resource::Resource,
    reset_style: bool,
  ) -> io::Result<()> {
    if reset_style {
      self.set_style(None);
    }
    if self.is_empty() {
      return Ok(());
    }

    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&self.bytes)?;
    self.bytes.clear();
    Ok(())
  }
}

#[derive(Debug, Clone)]
struct PatternMatcher {
  regex: Option<regex::Regex>,
  excludes: Vec<regex::Regex>,
}

impl PatternMatcher {
  fn new(options: &FdCommand) -> io::Result<Self> {
    let excludes = options
      .exclude
      .iter()
      .map(|pattern| build_glob_regex(pattern, options.case_insensitive))
      .collect::<io::Result<Vec<_>>>()?;

    let Some(pattern) = options.pattern.as_deref() else {
      return Ok(Self { regex: None, excludes });
    };

    let regex = if options.glob {
      build_glob_regex(pattern, options.case_insensitive).map_err(|err| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("fd: invalid glob '{pattern}': {err}"),
        )
      })?
    } else {
      RegexBuilder::new(pattern)
        .case_insensitive(options.case_insensitive)
        .build()
        .map_err(|err| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd: invalid regex '{pattern}': {err}"),
          )
        })?
    };

    Ok(Self { regex: Some(regex), excludes })
  }

  #[cfg(test)]
  fn matches(&self, entry_path: &Path, full_path: bool) -> bool {
    let basename = entry_path.file_name().map(|n| n.to_string_lossy());
    let full_path = full_path.then(|| entry_path.to_string_lossy());
    self.matches_parts(basename.as_deref().unwrap_or(""), full_path.as_deref())
  }

  fn matches_parts(&self, basename: &str, full_path: Option<&str>) -> bool {
    let Some(regex) = &self.regex else {
      return true;
    };

    regex.is_match(full_path.unwrap_or(basename))
  }

  #[cfg(test)]
  fn is_excluded(&self, entry_path: &Path, full_path: bool) -> bool {
    let basename = entry_path.file_name().map(|n| n.to_string_lossy());
    let full_path = full_path.then(|| entry_path.to_string_lossy());
    self.is_excluded_parts(
      basename.as_deref().unwrap_or(""),
      full_path.as_deref(),
    )
  }

  fn is_excluded_parts(&self, basename: &str, full_path: Option<&str>) -> bool {
    self.excludes.iter().any(|regex| {
      regex.is_match(basename)
        || full_path.is_some_and(|full_path| regex.is_match(full_path))
    })
  }
}

fn build_glob_regex(
  pattern: &str,
  case_insensitive: bool,
) -> io::Result<regex::Regex> {
  let mut regex = String::from("^");
  for ch in pattern.chars() {
    match ch {
      '*' => regex.push_str(".*"),
      '?' => regex.push('.'),
      _ => regex.push_str(&regex::escape(&ch.to_string())),
    }
  }
  regex.push('$');

  RegexBuilder::new(&regex).case_insensitive(case_insensitive).build().map_err(
    |err| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("fd: invalid exclude pattern '{pattern}': {err}"),
      )
    },
  )
}

fn execute_command(
  ctx: &AppContext,
  path: &str,
  cmd_template: &[String],
) -> io::Result<()> {
  if cmd_template.is_empty() {
    return Ok(());
  }

  // Replace {} placeholder with the actual path
  let cmd_args: Vec<String> = cmd_template
    .iter()
    .map(
      |arg| {
        if arg == "{}" { path.to_string() } else { arg.replace("{}", path) }
      },
    )
    .collect();

  let pid = process_util::spawn_command(ctx, &cmd_args[0], &cmd_args[1..])?;
  let status = process_util::wait_for_child(pid)?;

  if !status.success() {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      format!(
        "fd: command '{}' failed with exit code: {}",
        cmd_args.join(" "),
        match status {
          process_util::ChildStatus::Exited(code) => code,
          process_util::ChildStatus::Signaled(signal) => 128 + signal,
          process_util::ChildStatus::Other(raw) => raw,
        }
      ),
    ));
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_fd_command_with_pattern() {
    let parsed = FdCommand::parse(&["test".into()]).unwrap();
    assert_eq!(parsed.pattern, Some("test".to_string()));
    assert!(parsed.exclude.is_empty());
    assert!(!parsed.print0);
    assert_eq!(parsed.paths.len(), 0);
  }

  #[test]
  fn parse_fd_command_with_pattern_and_path() {
    let parsed = FdCommand::parse(&["test".into(), "/tmp".into()]).unwrap();
    assert_eq!(parsed.pattern, Some("test".to_string()));
    assert_eq!(parsed.paths, vec!["/tmp"]);
  }

  #[test]
  fn parse_fd_command_accepts_extension_after_pattern() {
    let parsed =
      FdCommand::parse(&["test".into(), "--extension".into(), "rs".into()])
        .unwrap();
    assert_eq!(parsed.pattern, Some("test".to_string()));
    assert_eq!(parsed.extensions, vec!["rs"]);
    assert!(parsed.paths.is_empty());
  }

  #[test]
  fn parse_fd_command_accepts_extension_after_explicit_path() {
    let parsed =
      FdCommand::parse(&["./src".into(), "--extension".into(), "rs".into()])
        .unwrap();
    assert_eq!(parsed.pattern, None);
    assert_eq!(parsed.paths, vec!["./src"]);
    assert_eq!(parsed.extensions, vec!["rs"]);
  }

  #[test]
  fn parse_fd_command_with_type_filter() {
    let parsed =
      FdCommand::parse(&["-t".into(), "f".into(), "test".into()]).unwrap();
    assert_eq!(parsed.file_type, Some(FileType::File));
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_max_depth() {
    let parsed =
      FdCommand::parse(&["-d".into(), "3".into(), "test".into()]).unwrap();
    assert_eq!(parsed.max_depth, Some(3));
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_min_depth() {
    let parsed =
      FdCommand::parse(&["--min-depth".into(), "2".into(), "test".into()])
        .unwrap();
    assert_eq!(parsed.min_depth, Some(2));
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_min_and_max_depth() {
    let parsed = FdCommand::parse(&[
      "--min-depth".into(),
      "1".into(),
      "-d".into(),
      "3".into(),
      "test".into(),
    ])
    .unwrap();
    assert_eq!(parsed.min_depth, Some(1));
    assert_eq!(parsed.max_depth, Some(3));
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_hidden_and_case_insensitive() {
    let parsed =
      FdCommand::parse(&["-H".into(), "-i".into(), "test".into()]).unwrap();
    assert!(parsed.hidden);
    assert!(parsed.case_insensitive);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_no_ignore() {
    let parsed = FdCommand::parse(&["-I".into(), "test".into()]).unwrap();
    assert!(parsed.no_ignore);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_no_ignore_vcs() {
    let parsed =
      FdCommand::parse(&["--no-ignore-vcs".into(), "test".into()]).unwrap();
    assert!(parsed.no_ignore_vcs);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_glob() {
    let parsed = FdCommand::parse(&["--glob".into(), "*.rs".into()]).unwrap();
    assert!(parsed.glob);
    assert_eq!(parsed.pattern, Some("*.rs".to_string()));
  }

  #[test]
  fn parse_fd_command_with_max_results() {
    let parsed =
      FdCommand::parse(&["--max-results".into(), "5".into(), "test".into()])
        .unwrap();
    assert_eq!(parsed.max_results, Some(5));
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_rejects_invalid_type() {
    let err = FdCommand::parse(&["-t".into(), "invalid".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_fd_command_rejects_unknown_flag() {
    let err = FdCommand::parse(&["-z".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_fd_command_with_extension() {
    let parsed =
      FdCommand::parse(&["-e".into(), "rs".into(), "test".into()]).unwrap();
    assert_eq!(parsed.extensions, vec!["rs"]);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_multiple_extensions_and_dot_prefix() {
    let parsed = FdCommand::parse(&[
      "-e".into(),
      ".rs".into(),
      "--extension".into(),
      "toml".into(),
      "test".into(),
    ])
    .unwrap();
    assert_eq!(parsed.extensions, vec!["rs", "toml"]);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_full_path() {
    let parsed = FdCommand::parse(&["-p".into(), "test".into()]).unwrap();
    assert!(parsed.full_path);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_absolute_path() {
    let parsed = FdCommand::parse(&["-a".into(), "test".into()]).unwrap();
    assert!(parsed.absolute_path);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_exec() {
    let parsed = FdCommand::parse(&[
      "-x".into(),
      "echo".into(),
      "{}".into(),
      ";".into(),
      "test".into(),
    ])
    .unwrap();
    assert_eq!(parsed.exec, Some(vec!["echo".to_string(), "{}".to_string()]));
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_exec_multiple_args() {
    let parsed = FdCommand::parse(&[
      "-x".into(),
      "wc".into(),
      "-l".into(),
      "{}".into(),
      ";".into(),
    ])
    .unwrap();
    assert_eq!(
      parsed.exec,
      Some(vec!["wc".to_string(), "-l".to_string(), "{}".to_string()])
    );
  }

  #[test]
  fn parse_fd_command_with_print0_and_excludes() {
    let parsed = FdCommand::parse(&[
      "-0".into(),
      "--exclude".into(),
      "*.tmp".into(),
      "--exclude".into(),
      "node_modules".into(),
      "test".into(),
    ])
    .unwrap();
    assert!(parsed.print0);
    assert_eq!(parsed.exclude, vec!["*.tmp", "node_modules"]);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn parse_fd_command_with_color_flags() {
    let colored = FdCommand::parse(&["-C".into(), "test".into()]).unwrap();
    assert_eq!(colored.color_output, Some(true));

    let monochrome = FdCommand::parse(&["-M".into(), "test".into()]).unwrap();
    assert_eq!(monochrome.color_output, Some(false));
  }

  #[test]
  fn parse_fd_command_with_sort() {
    let parsed = FdCommand::parse(&["--sort".into(), "test".into()]).unwrap();
    assert!(parsed.sort_results);
  }

  #[test]
  fn parse_fd_command_with_exclude_alias_and_one_file_system() {
    let parsed = FdCommand::parse(&[
      "-E".into(),
      "*.log".into(),
      "--one-file-system".into(),
      "test".into(),
    ])
    .unwrap();
    assert_eq!(parsed.exclude, vec!["*.log"]);
    assert!(parsed.one_file_system);
    assert_eq!(parsed.pattern, Some("test".to_string()));
  }

  #[test]
  fn pattern_matcher_uses_regex_by_default() {
    let cmd =
      FdCommand { pattern: Some("^foo\\d+$".into()), ..FdCommand::default() };
    let matcher = PatternMatcher::new(&cmd).unwrap();
    assert!(matcher.matches(Path::new("foo123"), false));
    assert!(!matcher.matches(Path::new("fooabc"), false));
  }

  #[test]
  fn pattern_matcher_supports_ignore_case() {
    let cmd = FdCommand {
      pattern: Some("^foo$".into()),
      case_insensitive: true,
      ..FdCommand::default()
    };
    let matcher = PatternMatcher::new(&cmd).unwrap();
    assert!(matcher.matches(Path::new("FOO"), false));
  }

  #[test]
  fn pattern_matcher_supports_glob_mode() {
    let cmd = FdCommand {
      pattern: Some("foo*.rs".into()),
      glob: true,
      ..FdCommand::default()
    };
    let matcher = PatternMatcher::new(&cmd).unwrap();
    assert!(matcher.matches(Path::new("foobar.rs"), false));
    assert!(!matcher.matches(Path::new("foobar.ts"), false));
  }

  #[test]
  fn pattern_matcher_rejects_invalid_regex() {
    let cmd = FdCommand { pattern: Some("(".into()), ..FdCommand::default() };
    let err = PatternMatcher::new(&cmd).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn exclude_matcher_supports_glob_patterns() {
    let cmd = FdCommand {
      exclude: vec!["*.tmp".into(), "node_modules".into()],
      ..FdCommand::default()
    };
    let matcher = PatternMatcher::new(&cmd).unwrap();
    assert!(matcher.is_excluded(Path::new("foo.tmp"), false));
    assert!(matcher.is_excluded(Path::new("node_modules"), false));
    assert!(!matcher.is_excluded(Path::new("foo.rs"), false));
  }

  #[test]
  fn entry_matches_options_applies_filters_before_mode_collection() {
    let cmd = FdCommand {
      pattern: Some("doc".into()),
      exclude: vec!["*.tmp".into()],
      extensions: vec!["rs".into()],
      ..FdCommand::default()
    };
    let matcher = PatternMatcher::new(&cmd).unwrap();
    let entries = [
      StreamWalkEntry {
        path: Path::new("src/doc.rs"),
        depth: 2,
        file_type: LioFileType::File,
      },
      StreamWalkEntry {
        path: Path::new("src/doc.tmp"),
        depth: 2,
        file_type: LioFileType::File,
      },
      StreamWalkEntry {
        path: Path::new("src/other.rs"),
        depth: 2,
        file_type: LioFileType::File,
      },
    ];

    assert!(entry_matches_options(&entries[0], &cmd, &matcher));
    assert!(!entry_matches_options(&entries[1], &cmd, &matcher));
    assert!(!entry_matches_options(&entries[2], &cmd, &matcher));
  }

  #[test]
  fn entry_matches_options_supports_case_insensitive_extensions() {
    let cmd = FdCommand {
      extensions: vec!["rs".into()],
      case_insensitive: true,
      ..FdCommand::default()
    };
    let matcher = PatternMatcher::new(&cmd).unwrap();
    let file = StreamWalkEntry {
      path: Path::new("src/lib.RS"),
      depth: 2,
      file_type: LioFileType::File,
    };

    assert!(entry_matches_options(&file, &cmd, &matcher));
  }

  #[test]
  fn entry_matches_options_extension_filter_rejects_directories() {
    let cmd =
      FdCommand { extensions: vec!["rs".into()], ..FdCommand::default() };
    let matcher = PatternMatcher::new(&cmd).unwrap();
    let directory = StreamWalkEntry {
      path: Path::new("src.rs"),
      depth: 1,
      file_type: LioFileType::Directory,
    };

    assert!(!entry_matches_options(&directory, &cmd, &matcher));
  }

  #[cfg(unix)]
  #[test]
  fn colorize_entry_path_wraps_directories_and_symlinks() {
    let dir = std::fs::File::open(".").unwrap();
    let dir_ty = dir.metadata().unwrap().file_type();
    assert_eq!(
      colorize_entry_path("src/", Path::new("src"), dir_ty),
      format!("{COLOR_DIRECTORY}src/{COLOR_RESET}")
    );

    let base =
      std::env::temp_dir().join(format!("busybox-fd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let file = base.join("file.txt");
    std::fs::write(&file, b"x").unwrap();
    let file_ty = std::fs::metadata(&file).unwrap().file_type();
    assert_eq!(
      colorize_entry_path("src/bin/file.txt", &file, file_ty),
      format!("{COLOR_DIRECTORY}src/bin/{COLOR_RESET}file.txt")
    );

    let executable = base.join("run.sh");
    std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
    let executable_c =
      std::ffi::CString::new(executable.to_string_lossy().as_bytes()).unwrap();
    let chmod_result = unsafe { libc::chmod(executable_c.as_ptr(), 0o755) };
    assert_eq!(chmod_result, 0);
    let executable_ty = std::fs::metadata(&executable).unwrap().file_type();
    assert_eq!(
      colorize_entry_path("bin/run.sh", &executable, executable_ty),
      format!(
        "{COLOR_DIRECTORY}bin/{COLOR_RESET}{COLOR_EXECUTABLE}run.sh{COLOR_RESET}"
      )
    );

    let target = base.join("target");
    std::fs::write(&target, b"x").unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let link_ty = std::fs::symlink_metadata(&link).unwrap().file_type();
    assert_eq!(
      colorize_entry_path("link", &link, link_ty),
      format!("{COLOR_SYMLINK}link{COLOR_RESET}")
    );
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&executable);
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_dir(&base);
  }
}
