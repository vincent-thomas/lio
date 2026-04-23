use std::{
  cell::RefCell,
  collections::{HashMap, VecDeque},
  env, io,
  path::PathBuf,
  rc::Rc,
  sync::{Arc, atomic::AtomicUsize},
  thread,
  time::{Duration, Instant},
};

use crossbeam_deque::{Injector, Stealer, Worker as DequeWorker};
use kanal::unbounded;
use lio::{Lio, api, api::resource::Resource};
use memchr::{memchr, memchr_iter, memrchr};

use super::render::{AppByteSink, ByteSink, StreamingRenderer};
use super::walker::FileWalker;
use super::worker::{
  Worker, WorkerAssignment, WorkerLoopProfile, WriterMessage,
};
use super::*;
use crate::app::AppContext;

pub(super) const LIO_READ_BATCH_SIZE: usize = 2;
const LIO_MIN_READ_BUF: usize = 8 * 1024;
const LIO_MAX_READ_BUF: usize = 512 * 1024;
const MAX_PARALLEL_SEARCH_WORKERS: usize = 12;
const MIN_FILES_PER_PARALLEL_WORKER: usize = 16;
pub(super) const LOCAL_DIR_TASK_THRESHOLD: usize = 32;
pub(super) const TRAVERSAL_TASK_BURST_SIZE: usize = 4;
pub(super) const WRITER_CHANNEL_FLUSH_THRESHOLD: usize = 256 * 1024;

#[derive(Debug)]
pub(super) enum SearchInput {
  Stdin(Vec<u8>),
  File { display_path: String, bytes: Vec<u8> },
}

#[derive(Debug, Clone)]
pub(super) struct SearchFile {
  path: PathBuf,
  display_path: String,
}

#[derive(Debug)]
struct PendingStreamFileRead {
  display_path: String,
  fd: lio::api::resource::Resource,
  buf: Vec<u8>,
  carry: Vec<u8>,
  before_context: VecDeque<BufferedContextLine>,
  bytes_searched: usize,
  line_number: usize,
  next_offset: usize,
  matches: usize,
  matched_lines: usize,
  emitted_match_lines: usize,
  has_match: bool,
  after_remaining: usize,
  gap_since_group_end: usize,
  has_emitted_group: bool,
  stop_after_context: bool,
  done: bool,
  file_match_emitted: bool,
}

#[derive(Debug)]
struct PendingOpeningFile {
  display_path: String,
  buf: Vec<u8>,
}

#[derive(Debug)]
enum PendingWorkerFile {
  Opening(PendingOpeningFile),
  Reading(PendingStreamFileRead),
}

#[derive(Debug)]
struct BufferedContextLine {
  line_number: usize,
  absolute_offset: usize,
  line: Vec<u8>,
}

enum StreamFileEvent {
  Open { id: usize, result: io::Result<lio::api::resource::Resource> },
  Read { id: usize, result: io::Result<i32>, returned_buf: Vec<u8> },
}

#[derive(Debug)]
struct PendingBatchFileRead {
  display_path: String,
  fd: lio::api::resource::Resource,
  buf: Vec<u8>,
  bytes: Vec<u8>,
  skipped: bool,
}

enum BatchFileEvent {
  Open { index: usize, result: io::Result<lio::api::resource::Resource> },
  Read { index: usize, result: io::Result<i32>, returned_buf: Vec<u8> },
}

pub(super) struct WorkerFilePipeline {
  events: Rc<RefCell<VecDeque<StreamFileEvent>>>,
  next_id: usize,
  inflight: usize,
  active: HashMap<usize, PendingWorkerFile>,
  free_buffers: Vec<Vec<u8>>,
}

impl WorkerFilePipeline {
  pub(super) fn new() -> Self {
    Self {
      events: Rc::new(RefCell::new(VecDeque::new())),
      next_id: 0,
      inflight: 0,
      active: HashMap::new(),
      free_buffers: Vec::new(),
    }
  }

  pub(super) fn file_count(&self) -> usize {
    self.active.len()
  }

  pub(super) fn has_capacity(&self) -> bool {
    self.file_count() < LIO_READ_BATCH_SIZE
  }

  pub(super) fn has_pending_work(&self) -> bool {
    self.inflight > 0 || !self.active.is_empty()
  }

  pub(super) fn submit_file(
    &mut self,
    lio: &Lio,
    cwd: &Resource,
    file: SearchFile,
  ) -> io::Result<()> {
    let id = self.next_id;
    self.next_id += 1;
    let cpath = std::ffi::CString::new(file.path.to_string_lossy().as_bytes())?;
    let buf = self.checkout_buffer();
    self.active.insert(
      id,
      PendingWorkerFile::Opening(PendingOpeningFile {
        display_path: file.display_path,
        buf,
      }),
    );
    let events = Rc::clone(&self.events);
    api::openat(cwd, cpath, libc::O_RDONLY, 0).with_lio(lio).when_done(
      move |result| {
        events.borrow_mut().push_back(StreamFileEvent::Open { id, result });
      },
    );
    self.inflight += 1;
    Ok(())
  }

  pub(super) fn drive_wait(&mut self, lio: &Lio) -> io::Result<()> {
    if self.inflight == 0 {
      return Ok(());
    }
    lio.run()?;
    Ok(())
  }

  fn checkout_buffer(&mut self) -> Vec<u8> {
    self
      .free_buffers
      .pop()
      .unwrap_or_else(|| vec![0u8; initial_read_buffer_len()])
  }

  fn recycle_buffer(&mut self, mut buf: Vec<u8>) {
    if buf.is_empty() {
      buf.resize(initial_read_buffer_len(), 0);
    }
    if self.free_buffers.len() < LIO_READ_BATCH_SIZE {
      self.free_buffers.push(buf);
    }
  }
}

#[derive(Debug)]
struct FileSearchResult {
  index: usize,
  outcomes: Vec<SearchOutcome>,
  stats: SearchStats,
}

fn schedule_batch_file_open(
  lio: &Lio,
  cwd: &Resource,
  tx: &kanal::Sender<BatchFileEvent>,
  index: usize,
  file: &SearchFile,
) -> io::Result<()> {
  let cpath = std::ffi::CString::new(file.path.to_string_lossy().as_bytes())?;
  let sender = tx.clone();
  api::openat(cwd, cpath, libc::O_RDONLY, 0).with_lio(lio).when_done(
    move |result| {
      let _ = sender.send(BatchFileEvent::Open { index, result });
    },
  );
  Ok(())
}

fn schedule_batch_file_read(
  lio: &Lio,
  tx: &kanal::Sender<BatchFileEvent>,
  index: usize,
  fd: &lio::api::resource::Resource,
  buf: Vec<u8>,
) {
  let sender = tx.clone();
  api::read(fd, buf).with_lio(lio).when_done(move |(result, returned_buf)| {
    let _ = sender.send(BatchFileEvent::Read { index, result, returned_buf });
  });
}

fn finish_batch_file(
  pending: &mut [Option<PendingBatchFileRead>],
  outputs: &mut [Option<SearchInput>],
  index: usize,
) {
  if let Some(file) = pending[index].take() {
    outputs[index] = (!file.skipped).then_some(SearchInput::File {
      display_path: file.display_path,
      bytes: file.bytes,
    });
  }
}

fn handle_batch_file_event(
  lio: &Lio,
  tx: &kanal::Sender<BatchFileEvent>,
  files: &[SearchFile],
  pending: &mut [Option<PendingBatchFileRead>],
  outputs: &mut [Option<SearchInput>],
  binary_mode: SearchBinaryMode,
  event: BatchFileEvent,
) -> io::Result<bool> {
  match event {
    BatchFileEvent::Open { index, result } => {
      let fd = match result {
        Ok(fd) => fd,
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
          outputs[index] = None;
          return Ok(true);
        }
        Err(err) => return Err(files[index].io_error(err)),
      };
      pending[index] = Some(PendingBatchFileRead {
        display_path: files[index].display_path.clone(),
        fd,
        buf: vec![0u8; initial_read_buffer_len()],
        bytes: Vec::new(),
        skipped: false,
      });
      let file = pending[index].as_mut().expect("missing pending file");
      let fd = file.fd.clone();
      let buf = std::mem::take(&mut file.buf);
      schedule_batch_file_read(lio, tx, index, &fd, buf);
      Ok(false)
    }
    BatchFileEvent::Read { index, result, returned_buf } => {
      let mut resubmit = None;
      let mut finished = false;
      {
        let file = pending[index].as_mut().expect("missing pending read state");
        file.buf = returned_buf;

        let n = match result {
          Ok(n) => n as usize,
          Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            file.bytes.clear();
            file.skipped = true;
            finished = true;
            0
          }
          Err(err) => return Err(files[index].io_error(err)),
        };

        if n == 0 {
          finished = true;
        } else if matches!(binary_mode, SearchBinaryMode::Skip)
          && file.buf[..n].contains(&0)
        {
          file.bytes.clear();
          file.skipped = true;
          finished = true;
        } else {
          file.bytes.extend_from_slice(&file.buf[..n]);
          let fd = file.fd.clone();
          let buf = std::mem::take(&mut file.buf);
          resubmit = Some((fd, buf));
        }
      }

      if finished {
        finish_batch_file(pending, outputs, index);
        return Ok(true);
      }

      if let Some((fd, buf)) = resubmit {
        schedule_batch_file_read(lio, tx, index, &fd, buf);
      }
      Ok(false)
    }
  }
}

impl SearchFile {
  fn io_error(&self, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("rg: {}: {err}", self.display_path))
  }

  pub(super) fn from_walk_file(file: super::walker::WalkFile) -> Self {
    Self { path: file.path, display_path: file.display_path }
  }

  fn maybe_push_input(
    self,
    lio: &Lio,
    inputs: &mut Vec<SearchInput>,
    binary_mode: SearchBinaryMode,
  ) -> io::Result<()> {
    if let Some(bytes) =
      super::util::read_searchable_file(lio, &self.path, binary_mode)
        .map_err(|err| self.io_error(err))?
    {
      inputs.push(SearchInput::File { display_path: self.display_path, bytes });
    }
    Ok(())
  }

  fn read_batch_lio(
    ctx: &AppContext,
    files: &[Self],
    binary_mode: SearchBinaryMode,
  ) -> io::Result<Vec<Option<SearchInput>>> {
    Self::read_batch_with_driver(ctx.lio(), &ctx.cwd(), files, binary_mode)
  }

  fn read_batch_with_driver(
    lio: &Lio,
    cwd: &Resource,
    files: &[Self],
    binary_mode: SearchBinaryMode,
  ) -> io::Result<Vec<Option<SearchInput>>> {
    if files.is_empty() {
      return Ok(Vec::new());
    }

    let (tx, rx) = unbounded();
    let mut inflight = 0usize;
    for (index, file) in files.iter().enumerate() {
      schedule_batch_file_open(lio, cwd, &tx, index, file)?;
      inflight += 1;
    }
    drop(tx.clone());

    let mut pending: Vec<Option<PendingBatchFileRead>> =
      std::iter::repeat_with(|| None).take(files.len()).collect();
    let mut outputs: Vec<Option<SearchInput>> =
      std::iter::repeat_with(|| None).take(files.len()).collect();
    let mut remaining_files = files.len();

    while remaining_files > 0 {
      let mut progressed = false;
      while let Ok(Some(event)) = rx.try_recv() {
        progressed = true;
        inflight = inflight.saturating_sub(1);
        let finished = handle_batch_file_event(
          lio,
          &tx,
          files,
          &mut pending,
          &mut outputs,
          binary_mode,
          event,
        )?;
        if finished {
          remaining_files -= 1;
        } else {
          inflight += 1;
        }
      }
      if remaining_files == 0 {
        break;
      }
      if !progressed {
        if inflight == 0 {
          return Err(io::Error::other(
            "rg: file batch stalled without in-flight operations",
          ));
        }
        if lio.try_run()? == 0 {
          lio.run()?;
        }
      }
    }

    Ok(outputs)
  }
}

fn initial_read_buffer_len() -> usize {
  (LIO_MIN_READ_BUF * 16).min(LIO_MAX_READ_BUF)
}

fn maybe_grow_read_buffer(buf: &mut Vec<u8>, bytes_read: usize) {
  if bytes_read < buf.len() || buf.len() >= LIO_MAX_READ_BUF {
    return;
  }

  let new_len = (buf.len() * 2).min(LIO_MAX_READ_BUF);
  buf.resize(new_len, 0);
}

fn next_record_end(bytes: &[u8], start: usize, delimiter: u8) -> usize {
  bytes[start..]
    .iter()
    .position(|&byte| byte == delimiter)
    .map(|offset| start + offset)
    .unwrap_or(bytes.len())
}

fn capped_parallel_worker_count(
  requested: Option<usize>,
  total_files: Option<usize>,
) -> usize {
  let available = std::thread::available_parallelism()
    .map(|count| count.get())
    .unwrap_or(1)
    .max(1)
    .min(MAX_PARALLEL_SEARCH_WORKERS);
  let capped_requested = requested.unwrap_or(available).max(1).min(available);

  match total_files {
    Some(total_files) if total_files > 0 => {
      capped_requested.min(total_files).max(1)
    }
    _ => capped_requested,
  }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum TargetOrder {
  Deterministic,
  Cli,
}

impl SearchEngine {
  fn effective_binary_mode(plan: &SearchPlan) -> SearchBinaryMode {
    if plan.config.search.text || plan.config.search.null_data {
      SearchBinaryMode::Text
    } else {
      plan.config.search.binary_mode
    }
  }

  pub fn search(
    &self,
    command: &RgCommand,
    runtime: &SearchRuntime,
  ) -> io::Result<Vec<SearchOutcome>> {
    self.search_plan(&command.plan_for_runtime(runtime), runtime)
  }

  pub fn search_plan(
    &self,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
  ) -> io::Result<Vec<SearchOutcome>> {
    self.search_with_target_order(plan, runtime, TargetOrder::Deterministic)
  }

  fn search_with_target_order(
    &self,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    target_order: TargetOrder,
  ) -> io::Result<Vec<SearchOutcome>> {
    if plan.config.search.files_mode {
      return self.collect_files_mode_outcomes(plan, runtime, target_order);
    }
    let matcher =
      super::matcher::CompiledMatcher::new(&plan.config.pattern_spec)?;
    let include_path = plan.include_path_in_results(runtime);
    let targets = self.resolve_targets(plan, runtime, target_order)?;
    let (outcomes, _) = self.finalize_search_outcomes(
      plan,
      runtime,
      include_path,
      targets,
      &matcher,
    )?;
    Ok(outcomes)
  }

  #[cfg(test)]
  pub(super) fn search_cli(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
  ) -> io::Result<(Vec<SearchOutcome>, SearchStats)> {
    let mut emitter = SearchResultEmitter::new();
    let stats =
      self.search_cli_with_sink(ctx, plan, runtime, true, &mut emitter)?;
    let mut outcomes = emitter.into_outcomes();
    if plan.should_suppress_auto_filename(runtime, &outcomes) {
      super::outcome::suppress_paths(&mut outcomes);
    }
    Ok((outcomes, stats))
  }

  pub(super) fn search_cli_stream(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<SearchStats> {
    self.search_cli_with_sink(ctx, plan, runtime, true, sink)
  }

  pub(super) fn execute_cli_pipeline(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
  ) -> io::Result<SearchStats> {
    let presentation = plan.presentation(runtime)?;
    if !plan.supports_core_unordered_execute()
      || plan.config.search.files_mode
      || (matches!(plan.targets.as_slice(), [SearchTarget::Stdin])
        && !runtime.stdin_is_tty)
      || plan.config.search.quiet
    {
      let mut renderer =
        StreamingRenderer::new(presentation, plan, AppByteSink::new(ctx));
      let stats = self.search_cli_stream(ctx, plan, runtime, &mut renderer)?;
      renderer.finish(stats, Duration::ZERO, Duration::ZERO)?;
      return Ok(stats);
    }
    self.search_cli_core_unordered_rendered(ctx, plan, runtime)
  }

  fn search_cli_with_plain_renderer(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
  ) -> io::Result<SearchStats> {
    let presentation = plan.presentation(runtime)?;
    let mut renderer = StreamingRenderer::new(
      presentation,
      plan,
      super::render::AppByteSink::new(ctx),
    );
    self.search_cli_with_sink(
      ctx,
      plan,
      runtime,
      presentation.color_enabled,
      &mut renderer,
    )
  }

  fn build_worker_assignments(
    &self,
    immediate_files: Vec<super::walker::WalkFile>,
    shard_tasks: Vec<super::walker::ParallelWalkTask>,
    desired_workers: usize,
  ) -> Vec<WorkerAssignment> {
    let mut assignments = (0..desired_workers)
      .map(|_| WorkerAssignment::default())
      .collect::<Vec<_>>();
    for (index, file) in immediate_files.into_iter().enumerate() {
      assignments[index % desired_workers]
        .immediate_files
        .push(SearchFile::from_walk_file(file));
    }
    for (index, shard) in shard_tasks.into_iter().enumerate() {
      assignments[index % desired_workers].shards.push(shard);
    }
    assignments.retain(|assignment| {
      !assignment.immediate_files.is_empty() || !assignment.shards.is_empty()
    });
    assignments
  }

  fn build_worker_queues(
    &self,
    assignments: &mut [WorkerAssignment],
  ) -> (
    Arc<AtomicUsize>,
    Arc<Injector<super::walker::ParallelWalkTask>>,
    Vec<DequeWorker<super::walker::ParallelWalkTask>>,
    Arc<Vec<Stealer<super::walker::ParallelWalkTask>>>,
  ) {
    let outstanding_tasks = Arc::new(AtomicUsize::new(
      assignments.iter().map(|assignment| assignment.shards.len()).sum(),
    ));
    let global_tasks =
      Arc::new(Injector::<super::walker::ParallelWalkTask>::new());
    let local_queues = (0..assignments.len())
      .map(|_| DequeWorker::<super::walker::ParallelWalkTask>::new_lifo())
      .collect::<Vec<_>>();
    for (index, assignment) in assignments.iter_mut().enumerate() {
      for shard in assignment.shards.drain(..) {
        local_queues[index].push(shard);
      }
    }
    let stealers = Arc::new(
      local_queues
        .iter()
        .map(DequeWorker::stealer)
        .collect::<Vec<Stealer<super::walker::ParallelWalkTask>>>(),
    );
    (outstanding_tasks, global_tasks, local_queues, stealers)
  }

  fn run_writer_loop(
    &self,
    ctx: &AppContext,
    worker_count: usize,
    writer_rx: kanal::Receiver<WriterMessage>,
  ) -> io::Result<SearchStats> {
    let mut stdout = AppByteSink::new(ctx);
    let mut stats = SearchStats::default();
    let mut done_workers = 0usize;
    let mut first_error = None;

    while done_workers < worker_count {
      match writer_rx.recv() {
        Ok(WriterMessage::Chunk(bytes)) => stdout.write_chunk(bytes)?,
        Ok(WriterMessage::WorkerDone(worker_stats)) => {
          done_workers += 1;
          stats.matches += worker_stats.matches;
          stats.matched_lines += worker_stats.matched_lines;
          stats.files_with_matches += worker_stats.files_with_matches;
          stats.files_searched += worker_stats.files_searched;
          stats.bytes_searched += worker_stats.bytes_searched;
        }
        Ok(WriterMessage::Error(err)) => {
          if first_error.is_none() {
            first_error = Some(err);
          }
        }
        Err(err) => {
          if first_error.is_none() {
            first_error = Some(io::Error::other(format!(
              "rg: writer channel failed: {err}"
            )));
          }
          break;
        }
      }
    }

    if let Some(err) = first_error {
      return Err(err);
    }
    Ok(stats)
  }

  pub(super) fn search_cli_core_unordered_rendered(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
  ) -> io::Result<SearchStats> {
    if !plan.supports_core_unordered_execute() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "rg: unsupported flag combination for the current execution engine",
      ));
    }

    if matches!(plan.targets.as_slice(), [SearchTarget::Stdin])
      && !runtime.stdin_is_tty
    {
      return self.search_cli_with_plain_renderer(ctx, plan, runtime);
    }

    let matcher =
      super::matcher::CompiledMatcher::new(&plan.config.pattern_spec)?;
    let include_path = plan.include_path_in_results(runtime);
    let effective_match_mode = plan.effective_match_mode();
    let target_paths = plan
      .targets
      .iter()
      .filter_map(|target| match target {
        SearchTarget::Stdin => None,
        SearchTarget::File(path) => Some(path.clone()),
      })
      .collect::<Vec<_>>();
    let walker = FileWalker::new(
      runtime.cwd.clone(),
      plan.config.traversal.clone(),
      plan.config.traversal.globs.clone(),
      plan.config.sort,
      TargetOrder::Cli,
    )?;
    let work = walker.build_parallel_walk_work(ctx, &target_paths)?;

    let desired_workers = capped_parallel_worker_count(
      plan.config.search.threads,
      Some(work.immediate_files.len() + work.shard_tasks.len()),
    );
    if desired_workers <= 1 {
      return self.search_cli_with_plain_renderer(ctx, plan, runtime);
    }

    let mut assignments = self.build_worker_assignments(
      work.immediate_files,
      work.shard_tasks,
      desired_workers,
    );
    if assignments.is_empty() {
      return Ok(SearchStats::default());
    }

    let (outstanding_tasks, global_tasks, local_queues, stealers) =
      self.build_worker_queues(&mut assignments);
    let presentation = plan.presentation(runtime)?;
    let worker_count = assignments.len();
    let (writer_tx, writer_rx) = unbounded::<WriterMessage>();
    let profiling_enabled = env::var_os("BUSYBOX_RG_PROFILE").is_some();
    let mut handles = Vec::with_capacity(assignments.len());
    for (worker_index, (assignment, local_queue)) in
      assignments.into_iter().zip(local_queues.into_iter()).enumerate()
    {
      let outstanding_tasks = Arc::clone(&outstanding_tasks);
      let global_tasks = Arc::clone(&global_tasks);
      let stealers = Arc::clone(&stealers);
      let plan = Arc::new(plan.clone());
      let matcher = super::matcher::WorkerMatcher::new(matcher.clone());
      let worker_walker = walker.clone();
      let worker_writer_tx = writer_tx.clone();
      handles.push(thread::spawn(move || {
        Worker::new(
          worker_index,
          assignment,
          local_queue,
          plan,
          matcher,
          presentation,
          worker_writer_tx,
          include_path,
          effective_match_mode,
          presentation.color_enabled,
          outstanding_tasks,
          global_tasks,
          stealers,
          worker_walker,
        )?
        .run()
      }));
    }
    drop(writer_tx);

    let writer_result = self.run_writer_loop(ctx, worker_count, writer_rx);

    let mut first_error = None;
    let mut worker_profiles = Vec::new();
    for handle in handles {
      match handle.join() {
        Ok(Ok((_worker_stats, profile))) => {
          worker_profiles.push(profile);
        }
        Ok(Err(err)) => {
          if first_error.is_none() {
            first_error = Some(err);
          }
        }
        Err(_) => {
          if first_error.is_none() {
            first_error = Some(io::Error::other("rg: worker thread panicked"));
          }
        }
      }
    }

    if let Some(err) = first_error {
      return Err(err);
    }
    if profiling_enabled {
      let total_profile = worker_profiles
        .iter()
        .copied()
        .fold(WorkerLoopProfile::default(), |acc, p| acc.merge(p));
      eprintln!();
      for (index, profile) in worker_profiles.iter().enumerate() {
        eprintln!(
          "rg-profile worker={index} iterations={} traversal_tasks={} files_admitted={} file_events_drained={} open_events={} read_events={} try_run_calls={} try_run_completions={} try_run_empty_calls={} drive_wait_calls={} traversal_ms={:.3} traversal_split_ms={:.3} traversal_filter_ms={:.3} drain_ms={:.3} open_event_ms={:.3} read_event_ms={:.3} match_render_ms={:.3} try_run_ms={:.3} drive_wait_ms={:.3} between_runtime_non_search_ms={:.3} idle_ms={:.3}",
          profile.iterations,
          profile.traversal_tasks,
          profile.files_admitted,
          profile.file_events_drained,
          profile.open_events,
          profile.read_events,
          profile.try_run_calls,
          profile.try_run_completions,
          profile.try_run_empty_calls,
          profile.drive_wait_calls,
          profile.traversal_time.as_secs_f64() * 1000.0,
          profile.traversal_split_time.as_secs_f64() * 1000.0,
          profile.traversal_filter_time.as_secs_f64() * 1000.0,
          profile.drain_time.as_secs_f64() * 1000.0,
          profile.open_event_time.as_secs_f64() * 1000.0,
          profile.read_event_time.as_secs_f64() * 1000.0,
          profile.match_render_time.as_secs_f64() * 1000.0,
          profile.try_run_time.as_secs_f64() * 1000.0,
          profile.drive_wait_time.as_secs_f64() * 1000.0,
          profile.between_runtime_non_search_time.as_secs_f64() * 1000.0,
          profile.idle_time.as_secs_f64() * 1000.0,
        );
      }
      eprintln!(
        "rg-profile total iterations={} traversal_tasks={} files_admitted={} file_events_drained={} open_events={} read_events={} try_run_calls={} try_run_completions={} try_run_empty_calls={} drive_wait_calls={} traversal_ms={:.3} traversal_split_ms={:.3} traversal_filter_ms={:.3} drain_ms={:.3} open_event_ms={:.3} read_event_ms={:.3} match_render_ms={:.3} try_run_ms={:.3} drive_wait_ms={:.3} between_runtime_non_search_ms={:.3} idle_ms={:.3}",
        total_profile.iterations,
        total_profile.traversal_tasks,
        total_profile.files_admitted,
        total_profile.file_events_drained,
        total_profile.open_events,
        total_profile.read_events,
        total_profile.try_run_calls,
        total_profile.try_run_completions,
        total_profile.try_run_empty_calls,
        total_profile.drive_wait_calls,
        total_profile.traversal_time.as_secs_f64() * 1000.0,
        total_profile.traversal_split_time.as_secs_f64() * 1000.0,
        total_profile.traversal_filter_time.as_secs_f64() * 1000.0,
        total_profile.drain_time.as_secs_f64() * 1000.0,
        total_profile.open_event_time.as_secs_f64() * 1000.0,
        total_profile.read_event_time.as_secs_f64() * 1000.0,
        total_profile.match_render_time.as_secs_f64() * 1000.0,
        total_profile.try_run_time.as_secs_f64() * 1000.0,
        total_profile.drive_wait_time.as_secs_f64() * 1000.0,
        total_profile.between_runtime_non_search_time.as_secs_f64() * 1000.0,
        total_profile.idle_time.as_secs_f64() * 1000.0,
      );
    }
    writer_result
  }

  fn search_cli_with_sink(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<SearchStats> {
    if plan.config.search.files_mode {
      let outcomes = self.collect_files_mode_outcomes_lio(
        ctx,
        plan,
        runtime,
        TargetOrder::Cli,
      )?;
      let files_searched = outcomes.len();
      for outcome in outcomes {
        sink.emit_outcome(outcome)?;
      }
      return Ok(SearchStats { files_searched, ..SearchStats::default() });
    }

    let matcher =
      super::matcher::CompiledMatcher::new(&plan.config.pattern_spec)?;
    let include_path = plan.include_path_in_results(runtime);
    let effective_match_mode = plan.effective_match_mode();
    let mut stats = SearchStats::default();

    if matches!(plan.targets.as_slice(), [SearchTarget::Stdin])
      && !runtime.stdin_is_tty
    {
      self.process_search_input(
        SearchInput::Stdin(runtime.stdin.clone().unwrap_or_default()),
        plan,
        include_path,
        effective_match_mode,
        &matcher,
        capture_spans,
        sink,
        &mut stats,
      )?;
    } else {
      let files =
        self.collect_search_files_lio(ctx, plan, runtime, TargetOrder::Cli)?;
      if self.should_parallelize_cli_file_search(plan, files.len()) {
        let (mut outcomes, parallel_stats) = self.search_files_parallel(
          files,
          plan,
          include_path,
          effective_match_mode,
          &matcher,
        )?;
        if plan.should_suppress_auto_filename(runtime, &outcomes) {
          super::outcome::suppress_paths(&mut outcomes);
        }
        for outcome in outcomes {
          sink.emit_outcome(outcome)?;
        }
        return Ok(parallel_stats);
      }
      'file_batches: for file_batch in files.chunks(LIO_READ_BATCH_SIZE) {
        if self.can_stream_search_files(plan) {
          if self.stream_search_file_batch_lio(
            ctx,
            file_batch,
            plan,
            include_path,
            effective_match_mode,
            &matcher,
            capture_spans,
            sink,
            &mut stats,
          )? {
            break 'file_batches;
          }
        } else {
          for input in SearchFile::read_batch_lio(
            ctx,
            file_batch,
            Self::effective_binary_mode(plan),
          )? {
            let Some(input) = input else {
              continue;
            };
            self.process_search_input(
              input,
              plan,
              include_path,
              effective_match_mode,
              &matcher,
              capture_spans,
              sink,
              &mut stats,
            )?;
            if plan.config.search.quiet && stats.matched_lines > 0 {
              break 'file_batches;
            }
          }
        }
      }
    }

    Ok(stats)
  }

  fn search_file_task(
    &self,
    lio: &Lio,
    index: usize,
    file: SearchFile,
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &super::matcher::CompiledMatcher,
  ) -> io::Result<FileSearchResult> {
    let Some(bytes) = super::util::read_searchable_file(
      lio,
      &file.path,
      Self::effective_binary_mode(plan),
    )
    .map_err(|err| file.io_error(err))?
    else {
      return Ok(FileSearchResult {
        index,
        outcomes: Vec::new(),
        stats: SearchStats::default(),
      });
    };

    let mut emitter = SearchResultEmitter::new();
    let mut stats = SearchStats::default();
    self.process_search_input(
      SearchInput::File { display_path: file.display_path, bytes },
      plan,
      include_path,
      effective_match_mode,
      matcher,
      true,
      &mut emitter,
      &mut stats,
    )?;

    Ok(FileSearchResult { index, outcomes: emitter.into_outcomes(), stats })
  }

  fn can_stream_search_files(&self, plan: &SearchPlan) -> bool {
    !plan.config.search.null_data
      && !plan.config.search.passthru
      && !matches!(plan.config.search.binary_mode, SearchBinaryMode::Report)
  }

  fn should_parallelize_cli_file_search(
    &self,
    plan: &SearchPlan,
    files_len: usize,
  ) -> bool {
    let worker_count =
      capped_parallel_worker_count(plan.config.search.threads, Some(files_len));
    !plan.config.search.quiet
      && worker_count > 1
      && files_len >= worker_count * MIN_FILES_PER_PARALLEL_WORKER
  }

  fn search_files_parallel(
    &self,
    files: Vec<SearchFile>,
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &super::matcher::CompiledMatcher,
  ) -> io::Result<(Vec<SearchOutcome>, SearchStats)> {
    let total_files = files.len();
    let worker_count = capped_parallel_worker_count(
      plan.config.search.threads,
      Some(total_files),
    );
    let (tx, rx) = unbounded();
    let mut handles = Vec::with_capacity(worker_count);
    let mut worker_batches = vec![Vec::new(); worker_count];
    for (index, file) in files.into_iter().enumerate() {
      worker_batches[index % worker_count].push((index, file));
    }

    for worker_batch in worker_batches {
      let tx = tx.clone();
      let plan = plan.clone();
      let matcher = matcher.clone();
      handles.push(thread::spawn(move || {
        let engine = SearchEngine::default();
        let lio = match Lio::new(256) {
          Ok(lio) => lio,
          Err(err) => {
            let _ = tx.send(Err(err));
            return;
          }
        };
        for (index, file) in worker_batch {
          let result = engine.search_file_task(
            &lio,
            index,
            file,
            &plan,
            include_path,
            effective_match_mode,
            &matcher,
          );
          if tx.send(result).is_err() {
            break;
          }
        }
      }));
    }
    drop(tx);

    let mut first_error = None;
    let mut results = (0..total_files).map(|_| None).collect::<Vec<_>>();
    let mut received = 0usize;

    while received < total_files {
      match rx.recv() {
        Ok(Ok(file_result)) => {
          let index = file_result.index;
          results[index] = Some(file_result);
          received += 1;
        }
        Ok(Err(err)) => {
          first_error = Some(err);
          received += 1;
        }
        Err(_) => {
          first_error = Some(io::Error::other("rg: worker channel failed"));
          break;
        }
      }
    }

    for handle in handles {
      let _ = handle.join();
    }

    if let Some(err) = first_error {
      return Err(err);
    }

    let mut outcomes = Vec::new();
    let mut stats = SearchStats::default();
    for result in results.into_iter().flatten() {
      outcomes.extend(result.outcomes);
      stats.matches += result.stats.matches;
      stats.matched_lines += result.stats.matched_lines;
      stats.files_with_matches += result.stats.files_with_matches;
      stats.files_searched += result.stats.files_searched;
      stats.bytes_searched += result.stats.bytes_searched;
    }

    Ok((outcomes, stats))
  }

  fn stream_search_file_batch_lio(
    &self,
    ctx: &AppContext,
    files: &[SearchFile],
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &super::matcher::CompiledMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
    stats: &mut SearchStats,
  ) -> io::Result<bool> {
    self.stream_search_file_batch_with_driver(
      ctx.lio(),
      &ctx.cwd(),
      files,
      plan,
      include_path,
      effective_match_mode,
      matcher,
      capture_spans,
      sink,
      stats,
    )
  }

  fn stream_search_file_batch_with_driver(
    &self,
    lio: &Lio,
    cwd: &Resource,
    files: &[SearchFile],
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &super::matcher::CompiledMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
    stats: &mut SearchStats,
  ) -> io::Result<bool> {
    enum FileEvent {
      Open { index: usize, result: io::Result<lio::api::resource::Resource> },
      Read { index: usize, result: io::Result<i32>, returned_buf: Vec<u8> },
    }

    if files.is_empty() {
      return Ok(false);
    }

    let mut pending: Vec<Option<PendingStreamFileRead>> =
      std::iter::repeat_with(|| None).take(files.len()).collect();
    let mut worker_matcher =
      super::matcher::WorkerMatcher::new(matcher.clone());
    let (tx, rx) = unbounded();
    let mut inflight = 0usize;
    for (index, file) in files.iter().enumerate() {
      let cpath =
        std::ffi::CString::new(file.path.to_string_lossy().as_bytes())?;
      let sender = tx.clone();
      api::openat(cwd, cpath, libc::O_RDONLY, 0).with_lio(lio).when_done(
        move |result| {
          let _ = sender.send(FileEvent::Open { index, result });
        },
      );
      inflight += 1;
    }
    drop(tx.clone());

    let mut remaining_files = files.len();
    while remaining_files > 0 {
      let mut progressed = false;
      while let Ok(Some(event)) = rx.try_recv() {
        progressed = true;
        inflight = inflight.saturating_sub(1);
        match event {
          FileEvent::Open { index, result } => {
            let fd = match result {
              Ok(fd) => fd,
              Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                remaining_files -= 1;
                continue;
              }
              Err(err) => return Err(files[index].io_error(err)),
            };
            pending[index] = Some(PendingStreamFileRead {
              display_path: files[index].display_path.clone(),
              fd,
              buf: vec![0u8; initial_read_buffer_len()],
              carry: Vec::new(),
              before_context: VecDeque::new(),
              bytes_searched: 0,
              line_number: 1,
              next_offset: 0,
              matches: 0,
              matched_lines: 0,
              emitted_match_lines: 0,
              has_match: false,
              after_remaining: 0,
              gap_since_group_end: 0,
              has_emitted_group: false,
              stop_after_context: false,
              done: false,
              file_match_emitted: false,
            });
            let file = pending[index].as_mut().expect("missing pending file");
            let fd = file.fd.clone();
            let buf = std::mem::take(&mut file.buf);
            let sender = tx.clone();
            api::read(&fd, buf).with_lio(lio).when_done(
              move |(result, returned_buf)| {
                let _ =
                  sender.send(FileEvent::Read { index, result, returned_buf });
              },
            );
            inflight += 1;
          }
          FileEvent::Read { index, result, returned_buf } => {
            let mut resubmit = None;
            let mut finished = false;
            {
              let file =
                pending[index].as_mut().expect("missing pending read state");
              file.buf = returned_buf;

              let n = match result {
                Ok(n) => n as usize,
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                  file.done = true;
                  finished = true;
                  0
                }
                Err(err) => return Err(files[index].io_error(err)),
              };
              if n == 0 {
                self.finish_stream_file(
                  file,
                  plan,
                  include_path,
                  effective_match_mode,
                  &mut worker_matcher,
                  capture_spans,
                  sink,
                  stats,
                )?;
                if plan.config.search.quiet && stats.matched_lines > 0 {
                  return Ok(true);
                }
                file.done = true;
                finished = true;
              } else {
                let contains_nul = file.buf[..n].contains(&0);
                if let Some(should_stop) = self.handle_stream_binary_chunk(
                  &file.display_path,
                  file.bytes_searched,
                  contains_nul,
                  &file.buf[..n],
                  plan,
                  include_path,
                  effective_match_mode,
                  &mut worker_matcher,
                  sink,
                  stats,
                )? {
                  if should_stop {
                    return Ok(true);
                  }
                  file.done = true;
                  finished = true;
                } else {
                  file.bytes_searched += n;
                  maybe_grow_read_buffer(&mut file.buf, n);
                  file.carry.extend_from_slice(&file.buf[..n]);
                  self.process_stream_chunk(
                    file,
                    plan,
                    include_path,
                    effective_match_mode,
                    &mut worker_matcher,
                    capture_spans,
                    sink,
                  )?;

                  if file.done {
                    self.finish_stream_file(
                      file,
                      plan,
                      include_path,
                      effective_match_mode,
                      &mut worker_matcher,
                      capture_spans,
                      sink,
                      stats,
                    )?;
                    if plan.config.search.quiet && stats.matched_lines > 0 {
                      return Ok(true);
                    }
                    finished = true;
                  } else {
                    let fd = file.fd.clone();
                    let buf = std::mem::take(&mut file.buf);
                    resubmit = Some((fd, buf));
                  }
                }
              }
            }
            if finished {
              pending[index] = None;
              remaining_files -= 1;
              continue;
            }
            if let Some((fd, buf)) = resubmit {
              let sender = tx.clone();
              api::read(&fd, buf).with_lio(lio).when_done(
                move |(result, returned_buf)| {
                  let _ = sender.send(FileEvent::Read {
                    index,
                    result,
                    returned_buf,
                  });
                },
              );
              inflight += 1;
            }
          }
        }
      }

      if remaining_files == 0 {
        break;
      }
      if !progressed {
        if inflight == 0 {
          return Err(io::Error::other(
            "rg: streaming file batch stalled without in-flight operations",
          ));
        }
        if lio.try_run()? == 0 {
          lio.run()?;
        }
      }
    }

    Ok(false)
  }

  pub(super) fn drain_worker_file_events(
    &self,
    pipeline: &mut WorkerFilePipeline,
    lio: &Lio,
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &mut super::matcher::WorkerMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
    stats: &mut SearchStats,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<(usize, bool, bool)> {
    let mut drained = 0usize;
    loop {
      let event = pipeline.events.borrow_mut().pop_front();
      let Some(event) = event else {
        break;
      };
      drained += 1;
      pipeline.inflight = pipeline.inflight.saturating_sub(1);
      match event {
        StreamFileEvent::Open { id, result } => {
          let open_started = Instant::now();
          profile.open_events += 1;
          let file =
            pipeline.active.remove(&id).expect("missing active file state");
          let PendingWorkerFile::Opening(PendingOpeningFile {
            display_path,
            mut buf,
          }) = file
          else {
            panic!("expected opening file state");
          };
          let fd = match result {
            Ok(fd) => fd,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
              pipeline.recycle_buffer(buf);
              profile.open_event_time += open_started.elapsed();
              continue;
            }
            Err(err) => {
              return Err(io::Error::new(
                err.kind(),
                format!("rg: {}: {err}", display_path),
              ));
            }
          };
          let file = PendingStreamFileRead {
            display_path,
            fd,
            buf: Vec::new(),
            carry: Vec::new(),
            before_context: VecDeque::new(),
            bytes_searched: 0,
            line_number: 1,
            next_offset: 0,
            matches: 0,
            matched_lines: 0,
            emitted_match_lines: 0,
            has_match: false,
            after_remaining: 0,
            gap_since_group_end: 0,
            has_emitted_group: false,
            stop_after_context: false,
            done: false,
            file_match_emitted: false,
          };
          let fd = file.fd.clone();
          let mut file = file;
          std::mem::swap(&mut file.buf, &mut buf);
          let buf = std::mem::take(&mut file.buf);
          pipeline.active.insert(id, PendingWorkerFile::Reading(file));
          let events = Rc::clone(&pipeline.events);
          api::read(&fd, buf).with_lio(lio).when_done(
            move |(result, returned_buf)| {
              events.borrow_mut().push_back(StreamFileEvent::Read {
                id,
                result,
                returned_buf,
              });
            },
          );
          pipeline.inflight += 1;
          profile.open_event_time += open_started.elapsed();
        }
        StreamFileEvent::Read { id, result, returned_buf } => {
          let read_started = Instant::now();
          profile.read_events += 1;
          let mut finished = false;
          let mut should_stop = false;
          let mut resubmit = None;
          {
            let file =
              pipeline.active.get_mut(&id).expect("missing active file state");
            let PendingWorkerFile::Reading(file) = file else {
              panic!("expected reading file state");
            };
            file.buf = returned_buf;

            let n = match result {
              Ok(n) => n as usize,
              Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                file.done = true;
                finished = true;
                0
              }
              Err(err) => {
                return Err(io::Error::new(
                  err.kind(),
                  format!("rg: {}: {err}", file.display_path),
                ));
              }
            };
            if n > 0 {
              let contains_nul = file.buf[..n].contains(&0);
              if let Some(stop) = self.handle_stream_binary_chunk(
                &file.display_path,
                file.bytes_searched,
                contains_nul,
                &file.buf[..n],
                plan,
                include_path,
                effective_match_mode,
                matcher,
                sink,
                stats,
              )? {
                finished = true;
                should_stop = stop;
              } else {
                let mut buf = std::mem::take(&mut file.buf);
                file.bytes_searched += n;
                maybe_grow_read_buffer(&mut buf, n);
                let match_started = Instant::now();
                self.process_stream_bytes(
                  file,
                  &buf[..n],
                  plan,
                  include_path,
                  effective_match_mode,
                  matcher,
                  capture_spans,
                  sink,
                )?;
                profile.match_render_time += match_started.elapsed();

                if file.done {
                  file.buf = buf;
                  self.finish_stream_file(
                    file,
                    plan,
                    include_path,
                    effective_match_mode,
                    matcher,
                    capture_spans,
                    sink,
                    stats,
                  )?;
                  sink.flush_file()?;
                  finished = true;
                  should_stop =
                    plan.config.search.quiet && stats.matched_lines > 0;
                } else {
                  let fd = file.fd.clone();
                  file.buf = buf;
                  let buf = std::mem::take(&mut file.buf);
                  resubmit = Some((fd, buf));
                }
              }
            } else {
              self.finish_stream_file(
                file,
                plan,
                include_path,
                effective_match_mode,
                matcher,
                capture_spans,
                sink,
                stats,
              )?;
              sink.flush_file()?;
              finished = true;
              should_stop = plan.config.search.quiet && stats.matched_lines > 0;
            }
          }
          if finished {
            let file =
              pipeline.active.remove(&id).expect("missing finished file state");
            let PendingWorkerFile::Reading(file) = file else {
              panic!("expected reading file state");
            };
            pipeline.recycle_buffer(file.buf);
            if should_stop {
              profile.read_event_time += read_started.elapsed();
              return Ok((drained, true, false));
            }
            profile.read_event_time += read_started.elapsed();
            continue;
          }
          if let Some((fd, buf)) = resubmit {
            let events = Rc::clone(&pipeline.events);
            api::read(&fd, buf).with_lio(lio).when_done(
              move |(result, returned_buf)| {
                events.borrow_mut().push_back(StreamFileEvent::Read {
                  id,
                  result,
                  returned_buf,
                });
              },
            );
            pipeline.inflight += 1;
          }
          profile.read_event_time += read_started.elapsed();
        }
      }
    }

    Ok((drained, false, false))
  }

  fn process_stream_bytes(
    &self,
    file: &mut PendingStreamFileRead,
    bytes: &[u8],
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &mut super::matcher::WorkerMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    if file.carry.is_empty() {
      return self.process_stream_bytes_no_carry(
        file,
        bytes,
        plan,
        include_path,
        effective_match_mode,
        matcher,
        capture_spans,
        sink,
      );
    }

    file.carry.extend_from_slice(bytes);
    self.process_stream_chunk(
      file,
      plan,
      include_path,
      effective_match_mode,
      matcher,
      capture_spans,
      sink,
    )
  }

  fn handle_stream_binary_chunk(
    &self,
    display_path: &str,
    bytes_searched: usize,
    contains_nul: bool,
    bytes: &[u8],
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &mut super::matcher::WorkerMatcher,
    sink: &mut dyn SearchOutcomeSink,
    stats: &mut SearchStats,
  ) -> io::Result<Option<bool>> {
    if plan.config.search.text || plan.config.search.null_data || !contains_nul
    {
      return Ok(None);
    }

    match plan.config.search.binary_mode {
      SearchBinaryMode::Skip => {
        sink.flush_file()?;
        return Ok(Some(false));
      }
      SearchBinaryMode::Text => return Ok(None),
      SearchBinaryMode::Report => {}
    }

    let matched = if plan.config.search.invert_match {
      false
    } else {
      matcher.is_match(bytes)
    };

    match effective_match_mode {
      MatchMode::Standard if matched => {
        sink
          .emit_binary_match(include_path.then_some(display_path.to_owned()))?;
      }
      MatchMode::Count | MatchMode::CountMatches => {
        sink.emit_count(
          include_path.then_some(display_path.to_owned()),
          usize::from(matched),
        )?;
      }
      MatchMode::FilesWithMatches if matched => {
        if include_path {
          sink.emit_file_match(display_path.to_owned())?;
        }
      }
      MatchMode::FilesWithoutMatch if !matched => {
        sink.emit_file_without_match(display_path.to_owned())?;
      }
      _ => {}
    }

    stats.files_searched += 1;
    stats.bytes_searched += bytes_searched + bytes.len();
    stats.matches += usize::from(matched);
    stats.matched_lines += usize::from(matched);
    stats.files_with_matches += usize::from(matched);

    sink.flush_file()?;
    Ok(Some(plan.config.search.quiet && matched))
  }

  fn process_stream_chunk(
    &self,
    file: &mut PendingStreamFileRead,
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &mut super::matcher::WorkerMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    if plan.config.output.json && file.next_offset == 0 && file.line_number == 1
    {
      sink.emit_json_begin(Some(file.display_path.clone()))?;
    }

    if self.can_use_stream_candidate_fast_path(
      plan,
      effective_match_mode,
      capture_spans,
    ) && matcher.has_candidate_line_search()
    {
      let mut carry = std::mem::take(&mut file.carry);
      let Some(last_newline) = memrchr(b'\n', &carry) else {
        file.carry = carry;
        return Ok(());
      };
      let remainder = carry.split_off(last_newline + 1);
      self.process_stream_chunk_candidate_fast(
        file,
        &carry,
        plan,
        include_path,
        matcher,
        capture_spans,
        sink,
      )?;
      if file.done {
        file.carry = remainder;
        return Ok(());
      }
      file.carry = remainder;
      return Ok(());
    }

    let mut carry = std::mem::take(&mut file.carry);
    let mut consumed = 0usize;
    while let Some(relative_pos) = memchr(b'\n', &carry[consumed..]) {
      let pos = consumed + relative_pos;
      let line = &carry[consumed..pos];
      consumed = pos + 1;
      self.process_stream_line(
        file,
        line,
        plan,
        include_path,
        effective_match_mode,
        matcher,
        capture_spans,
        sink,
      )?;
      if file.done {
        file.carry = carry.split_off(consumed);
        return Ok(());
      }
    }

    file.carry = carry.split_off(consumed);

    Ok(())
  }

  fn process_stream_bytes_no_carry(
    &self,
    file: &mut PendingStreamFileRead,
    bytes: &[u8],
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &mut super::matcher::WorkerMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    if plan.config.output.json && file.next_offset == 0 && file.line_number == 1
    {
      sink.emit_json_begin(Some(file.display_path.clone()))?;
    }

    if self.can_use_stream_candidate_fast_path(
      plan,
      effective_match_mode,
      capture_spans,
    ) && matcher.has_candidate_line_search()
    {
      let Some(last_newline) = memrchr(b'\n', bytes) else {
        file.carry.extend_from_slice(bytes);
        return Ok(());
      };
      self.process_stream_chunk_candidate_fast(
        file,
        &bytes[..=last_newline],
        plan,
        include_path,
        matcher,
        capture_spans,
        sink,
      )?;
      if !file.done && last_newline + 1 < bytes.len() {
        file.carry.extend_from_slice(&bytes[last_newline + 1..]);
      }
      return Ok(());
    }

    let mut consumed = 0usize;
    while let Some(relative_pos) = memchr(b'\n', &bytes[consumed..]) {
      let pos = consumed + relative_pos;
      let line = &bytes[consumed..pos];
      consumed = pos + 1;
      self.process_stream_line(
        file,
        line,
        plan,
        include_path,
        effective_match_mode,
        matcher,
        capture_spans,
        sink,
      )?;
      if file.done {
        if consumed < bytes.len() {
          file.carry.extend_from_slice(&bytes[consumed..]);
        }
        return Ok(());
      }
    }

    if consumed < bytes.len() {
      file.carry.extend_from_slice(&bytes[consumed..]);
    }

    Ok(())
  }

  fn can_use_stream_candidate_fast_path(
    &self,
    plan: &SearchPlan,
    effective_match_mode: MatchMode,
    capture_spans: bool,
  ) -> bool {
    let _ = capture_spans;
    effective_match_mode == MatchMode::Standard
      && !plan.config.search.invert_match
      && !plan.config.search.passthru
      && plan.config.context.before == 0
      && plan.config.context.after == 0
      && !plan.config.search.null_data
  }

  fn process_stream_chunk_candidate_fast(
    &self,
    file: &mut PendingStreamFileRead,
    complete: &[u8],
    plan: &SearchPlan,
    include_path: bool,
    matcher: &mut super::matcher::WorkerMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    let mut cursor = 0usize;
    while cursor < complete.len() {
      let Some(candidate) = matcher.find_candidate_line(&complete[cursor..])
      else {
        self.advance_stream_nonmatch_prefix(file, &complete[cursor..]);
        break;
      };
      let candidate_offset = match candidate {
        super::matcher::CandidateLineMatch::Confirmed(offset)
        | super::matcher::CandidateLineMatch::Candidate(offset) => {
          cursor + offset
        }
      };
      let line_start = match memrchr(b'\n', &complete[cursor..candidate_offset])
      {
        Some(offset) => cursor + offset + 1,
        None => cursor,
      };
      self.advance_stream_nonmatch_prefix(file, &complete[cursor..line_start]);
      let line_end = line_start
        + memchr(b'\n', &complete[line_start..])
          .expect("complete line chunk must contain newline");
      let line = &complete[line_start..line_end];
      let absolute_offset = file.next_offset;
      let line_number = file.line_number;

      let raw_match = match candidate {
        super::matcher::CandidateLineMatch::Confirmed(_) => true,
        super::matcher::CandidateLineMatch::Candidate(_) => {
          matcher.is_match(line)
        }
      };
      let spans = if !capture_spans || !raw_match {
        &[][..]
      } else {
        matcher.line_spans(line)?
      };
      let is_match = if capture_spans { !spans.is_empty() } else { raw_match };

      file.line_number += 1;
      file.next_offset += line.len() + 1;

      if is_match {
        file.matched_lines += 1;
        file.matches += if capture_spans { spans.len() } else { 1 };
        file.has_match = true;
        self.emit_stream_standard_match(
          include_path.then_some(file.display_path.as_str()),
          line_number,
          absolute_offset,
          line,
          spans,
          plan,
          sink,
        )?;
        if plan
          .config
          .search
          .max_count
          .is_some_and(|max_count| file.matched_lines >= max_count)
        {
          file.done = true;
          return Ok(());
        }
      }

      cursor = line_end + 1;
    }
    Ok(())
  }

  fn advance_stream_nonmatch_prefix(
    &self,
    file: &mut PendingStreamFileRead,
    bytes: &[u8],
  ) {
    if bytes.is_empty() {
      return;
    }
    let newline_count = memchr_iter(b'\n', bytes).count();
    file.line_number += newline_count;
    file.next_offset += bytes.len();
  }

  fn process_stream_line(
    &self,
    file: &mut PendingStreamFileRead,
    line: &[u8],
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &mut super::matcher::WorkerMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    let line_number = file.line_number;
    let absolute_offset = file.next_offset;

    let raw_match = matcher.is_match(line);
    let spans =
      if plan.config.search.invert_match || !capture_spans || !raw_match {
        &[][..]
      } else {
        matcher.line_spans(line)?
      };
    let is_match = if plan.config.search.invert_match {
      !raw_match
    } else if capture_spans {
      !spans.is_empty()
    } else {
      raw_match
    };

    file.line_number += 1;
    file.next_offset += line.len() + 1;

    if is_match {
      file.matched_lines += 1;
      file.matches += if capture_spans { spans.len() } else { 1 };
      file.has_match = true;
    }

    let has_context =
      plan.config.context.before > 0 || plan.config.context.after > 0;
    if effective_match_mode == MatchMode::Standard && !has_context {
      if is_match {
        self.emit_stream_standard_match(
          include_path.then_some(file.display_path.as_str()),
          line_number,
          absolute_offset,
          line,
          spans,
          plan,
          sink,
        )?;
        file.emitted_match_lines += 1;
        if plan
          .config
          .search
          .max_count
          .is_some_and(|max_count| file.emitted_match_lines >= max_count)
        {
          file.done = true;
        }
      }
      return Ok(());
    }

    match effective_match_mode {
      MatchMode::Standard => {
        if is_match {
          if file.stop_after_context && file.after_remaining > 0 {
            let path = include_path.then_some(file.display_path.clone());
            self.buffer_or_emit_stream_context_line(
              file,
              path.as_deref(),
              line_number,
              absolute_offset,
              line.to_vec(),
              plan,
              sink,
            )?;
          } else {
            let path = include_path.then_some(file.display_path.clone());
            self.emit_stream_context_match(
              file,
              path.as_deref(),
              line_number,
              absolute_offset,
              line.to_vec(),
              spans.to_vec(),
              plan,
              sink,
            )?;
          }
        } else if has_context {
          let path = include_path.then_some(file.display_path.clone());
          self.buffer_or_emit_stream_context_line(
            file,
            path.as_deref(),
            line_number,
            absolute_offset,
            line.to_vec(),
            plan,
            sink,
          )?;
        }
      }
      MatchMode::FilesWithMatches if is_match && !file.file_match_emitted => {
        if include_path {
          sink.emit_file_match(file.display_path.clone())?;
        }
        file.file_match_emitted = true;
        file.done = true;
      }
      MatchMode::FilesWithoutMatch if is_match => {
        file.done = true;
      }
      _ => {}
    }

    Ok(())
  }

  fn emit_stream_context_match(
    &self,
    file: &mut PendingStreamFileRead,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: Vec<u8>,
    spans: Vec<MatchSpan>,
    plan: &SearchPlan,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    if file.has_emitted_group
      && file.gap_since_group_end > plan.config.context.before
    {
      sink.emit_context_separator()?;
    }

    while let Some(record) = file.before_context.pop_front() {
      sink.emit_context_line(
        path,
        record.line_number,
        record.absolute_offset,
        &record.line,
      )?;
    }

    sink.emit_match_line(
      path,
      line_number,
      absolute_offset,
      &line,
      if plan.config.search.invert_match { &[] } else { &spans },
    )?;

    file.has_emitted_group = true;
    file.gap_since_group_end = 0;
    file.after_remaining = plan.config.context.after;
    file.emitted_match_lines += 1;
    if plan
      .config
      .search
      .max_count
      .is_some_and(|max_count| file.emitted_match_lines >= max_count)
    {
      file.stop_after_context = true;
      if file.after_remaining == 0 {
        file.done = true;
      }
    }

    Ok(())
  }

  fn buffer_or_emit_stream_context_line(
    &self,
    file: &mut PendingStreamFileRead,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: Vec<u8>,
    plan: &SearchPlan,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    if file.after_remaining > 0 {
      sink.emit_context_line(path, line_number, absolute_offset, &line)?;
      file.after_remaining -= 1;
      if file.after_remaining == 0 {
        file.gap_since_group_end = 0;
        if file.stop_after_context {
          file.done = true;
        }
      }
      return Ok(());
    }

    if file.has_emitted_group {
      file.gap_since_group_end += 1;
    }

    if plan.config.context.before > 0 {
      file.before_context.push_back(BufferedContextLine {
        line_number,
        absolute_offset,
        line,
      });
      while file.before_context.len() > plan.config.context.before {
        file.before_context.pop_front();
      }
    }

    Ok(())
  }

  fn emit_stream_standard_match(
    &self,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: &[u8],
    spans: &[MatchSpan],
    plan: &SearchPlan,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    if plan.config.output.vimgrep && !plan.config.search.invert_match {
      for span in spans {
        sink.emit_match_line(
          path,
          line_number,
          absolute_offset,
          line,
          std::slice::from_ref(span),
        )?;
      }
    } else if plan.config.output.only_matching
      && !plan.config.search.invert_match
    {
      for span in spans.iter().cloned() {
        let exact_span = [MatchSpan::new(0, span.end - span.start)?];
        sink.emit_match_line(
          path,
          line_number,
          absolute_offset + span.start,
          &line[span.start..span.end],
          &exact_span,
        )?;
      }
    } else if spans.is_empty() {
      sink.emit_plain_match(path, line_number, absolute_offset, line)?;
    } else {
      sink.emit_match_line(
        path,
        line_number,
        absolute_offset,
        line,
        if plan.config.search.invert_match { &[] } else { spans },
      )?;
    }

    Ok(())
  }

  fn finish_stream_file(
    &self,
    file: &mut PendingStreamFileRead,
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &mut super::matcher::WorkerMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
    stats: &mut SearchStats,
  ) -> io::Result<()> {
    if file.line_number == 1
      && file.bytes_searched == 0
      && file.carry.is_empty()
    {
      stats.files_searched += 1;
      if matches!(
        effective_match_mode,
        MatchMode::Count | MatchMode::CountMatches
      ) && plan.config.output.include_zero
      {
        sink
          .emit_count(include_path.then_some(file.display_path.clone()), 0)?;
      } else if matches!(effective_match_mode, MatchMode::FilesWithoutMatch) {
        sink.emit_file_without_match(file.display_path.clone())?;
      }
      if plan.config.output.json {
        sink.emit_json_begin(Some(file.display_path.clone()))?;
        sink.emit_json_end(
          Some(file.display_path.clone()),
          0,
          0,
          0,
          false,
          std::time::Duration::ZERO,
        )?;
      }
      file.done = true;
      return Ok(());
    }

    if !file.carry.is_empty() && !file.done {
      let trailing = std::mem::take(&mut file.carry);
      self.process_stream_line(
        file,
        &trailing,
        plan,
        include_path,
        effective_match_mode,
        matcher,
        capture_spans,
        sink,
      )?;
    }

    match effective_match_mode {
      MatchMode::Count
        if file.matched_lines > 0 || plan.config.output.include_zero =>
      {
        sink.emit_count(
          include_path.then_some(file.display_path.clone()),
          file.matched_lines,
        )?;
      }
      MatchMode::CountMatches
        if file.matches > 0 || plan.config.output.include_zero =>
      {
        sink.emit_count(
          include_path.then_some(file.display_path.clone()),
          file.matches,
        )?;
      }
      MatchMode::FilesWithoutMatch if !file.has_match => {
        sink.emit_file_without_match(file.display_path.clone())?;
      }
      _ => {}
    }

    stats.files_searched += 1;
    stats.bytes_searched += file.bytes_searched;
    stats.matches += file.matches;
    stats.matched_lines += file.matched_lines;
    stats.files_with_matches += usize::from(file.has_match);

    if plan.config.output.json {
      sink.emit_json_end(
        Some(file.display_path.clone()),
        file.bytes_searched,
        file.matches,
        file.matched_lines,
        file.has_match,
        std::time::Duration::ZERO,
      )?;
    }

    file.done = true;
    Ok(())
  }

  fn resolve_targets(
    &self,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    target_order: TargetOrder,
  ) -> io::Result<Vec<SearchInput>> {
    if matches!(plan.targets.as_slice(), [SearchTarget::Stdin])
      && !runtime.stdin_is_tty
    {
      return Ok(vec![SearchInput::Stdin(
        runtime.stdin.clone().unwrap_or_default(),
      )]);
    }
    let files = self.collect_search_files(plan, runtime, target_order)?;
    let lio = Lio::new(256)?;

    let mut inputs = Vec::new();
    for file in files {
      match file.maybe_push_input(
        &lio,
        &mut inputs,
        Self::effective_binary_mode(plan),
      ) {
        Ok(()) => {}
        Err(_err) if plan.config.search.suppress_errors => {}
        Err(err) => return Err(err),
      }
    }
    Ok(inputs)
  }

  fn collect_search_files(
    &self,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    target_order: TargetOrder,
  ) -> io::Result<Vec<SearchFile>> {
    let walker = FileWalker::new(
      runtime.cwd.clone(),
      plan.config.traversal.clone(),
      plan.config.traversal.globs.clone(),
      plan.config.sort,
      target_order,
    )?;
    let target_paths = plan
      .targets
      .iter()
      .filter_map(|target| match target {
        SearchTarget::Stdin => None,
        SearchTarget::File(path) => Some(path.clone()),
      })
      .collect::<Vec<_>>();
    let ctx = AppContext::new()?;
    if plan.config.search.suppress_errors {
      let mut files = Vec::new();
      for path in &target_paths {
        match walker.walk_path(&ctx, path) {
          Ok(mut walked) => files.append(&mut walked),
          Err(_) => continue,
        }
      }
      walker.order_files(&mut files);
      Ok(files.into_iter().map(SearchFile::from_walk_file).collect())
    } else {
      walker.walk_paths(&ctx, &target_paths).map(|files| {
        files.into_iter().map(SearchFile::from_walk_file).collect()
      })
    }
  }

  fn collect_search_files_lio(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    target_order: TargetOrder,
  ) -> io::Result<Vec<SearchFile>> {
    let _ = (ctx, runtime);
    let walker = FileWalker::new(
      runtime.cwd.clone(),
      plan.config.traversal.clone(),
      plan.config.traversal.globs.clone(),
      plan.config.sort,
      target_order,
    )?;
    let target_paths = plan
      .targets
      .iter()
      .filter_map(|target| match target {
        SearchTarget::Stdin => None,
        SearchTarget::File(path) => Some(path.clone()),
      })
      .collect::<Vec<_>>();
    if plan.config.search.suppress_errors {
      let mut files = Vec::new();
      for path in &target_paths {
        match walker.walk_path(ctx, path) {
          Ok(mut walked) => files.append(&mut walked),
          Err(_) => continue,
        }
      }
      walker.order_files(&mut files);
      Ok(files.into_iter().map(SearchFile::from_walk_file).collect())
    } else {
      walker.walk_paths(ctx, &target_paths).map(|files| {
        files.into_iter().map(SearchFile::from_walk_file).collect()
      })
    }
  }

  fn collect_files_mode_outcomes(
    &self,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    target_order: TargetOrder,
  ) -> io::Result<Vec<SearchOutcome>> {
    Ok(
      self
        .collect_search_files(plan, runtime, target_order)?
        .into_iter()
        .map(|file| SearchOutcome::file_match(file.display_path))
        .collect(),
    )
  }

  fn collect_files_mode_outcomes_lio(
    &self,
    ctx: &AppContext,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    target_order: TargetOrder,
  ) -> io::Result<Vec<SearchOutcome>> {
    Ok(
      self
        .collect_search_files_lio(ctx, plan, runtime, target_order)?
        .into_iter()
        .map(|file| SearchOutcome::file_match(file.display_path))
        .collect(),
    )
  }

  fn finalize_search_outcomes(
    &self,
    plan: &SearchPlan,
    runtime: &SearchRuntime,
    include_path: bool,
    targets: Vec<SearchInput>,
    matcher: &super::matcher::CompiledMatcher,
  ) -> io::Result<(Vec<SearchOutcome>, SearchStats)> {
    let mut emitter = SearchResultEmitter::new();
    let mut stats = SearchStats::default();
    let effective_match_mode = plan.effective_match_mode();

    for target in targets {
      self.process_search_input(
        target,
        plan,
        include_path,
        effective_match_mode,
        matcher,
        true,
        &mut emitter,
        &mut stats,
      )?;
      if plan.config.search.quiet && stats.matched_lines > 0 {
        emitter = SearchResultEmitter::new();
        break;
      }
    }

    let mut outcomes = emitter.into_outcomes();
    if plan.should_suppress_auto_filename(runtime, &outcomes) {
      super::outcome::suppress_paths(&mut outcomes);
    }

    Ok((outcomes, stats))
  }

  fn process_search_input(
    &self,
    target: SearchInput,
    plan: &SearchPlan,
    include_path: bool,
    effective_match_mode: MatchMode,
    matcher: &super::matcher::CompiledMatcher,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
    stats: &mut SearchStats,
  ) -> io::Result<()> {
    match target {
      SearchInput::Stdin(bytes) => {
        if self.process_binary_input(
          None,
          &bytes,
          matcher,
          plan,
          effective_match_mode,
          sink,
        )? {
          stats.files_searched += 1;
          stats.bytes_searched += bytes.len();
          stats.matches += 1;
          stats.matched_lines += 1;
          stats.files_with_matches += 1;
          return Ok(());
        }
        if plan.config.output.json {
          sink.emit_json_begin(None)?;
        }
        stats.files_searched += 1;
        stats.bytes_searched += bytes.len();
        let search_start = std::time::Instant::now();
        let file_stats = self.search_contents(
          None,
          &bytes,
          matcher,
          plan,
          effective_match_mode,
          capture_spans,
          sink,
        )?;
        let elapsed = search_start.elapsed();
        stats.matches += file_stats.matches;
        stats.matched_lines += file_stats.matched_lines;
        stats.files_with_matches += usize::from(file_stats.matched_lines > 0);
        if plan.config.output.json {
          sink.emit_json_end(
            None,
            bytes.len(),
            file_stats.matches,
            file_stats.matched_lines,
            file_stats.matched_lines > 0,
            elapsed,
          )?;
        }
      }
      SearchInput::File { display_path, bytes } => {
        if self.process_binary_input(
          Some(display_path.as_str()),
          &bytes,
          matcher,
          plan,
          effective_match_mode,
          sink,
        )? {
          stats.files_searched += 1;
          stats.bytes_searched += bytes.len();
          stats.matches += 1;
          stats.matched_lines += 1;
          stats.files_with_matches += 1;
          return Ok(());
        }
        if plan.config.output.json {
          sink.emit_json_begin(Some(display_path.clone()))?;
        }
        stats.files_searched += 1;
        stats.bytes_searched += bytes.len();
        let search_start = std::time::Instant::now();
        let file_stats = self.search_contents(
          include_path.then_some(display_path.as_str()),
          &bytes,
          matcher,
          plan,
          effective_match_mode,
          capture_spans,
          sink,
        )?;
        let elapsed = search_start.elapsed();
        stats.matches += file_stats.matches;
        stats.matched_lines += file_stats.matched_lines;
        stats.files_with_matches += usize::from(file_stats.matched_lines > 0);
        if plan.config.output.json {
          sink.emit_json_end(
            Some(display_path),
            bytes.len(),
            file_stats.matches,
            file_stats.matched_lines,
            file_stats.matched_lines > 0,
            elapsed,
          )?;
        }
      }
    }
    Ok(())
  }

  fn process_binary_input(
    &self,
    path: Option<&str>,
    bytes: &[u8],
    matcher: &super::matcher::CompiledMatcher,
    plan: &SearchPlan,
    match_mode: MatchMode,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<bool> {
    if !bytes.contains(&0)
      || plan.config.search.text
      || plan.config.search.null_data
      || !matches!(plan.config.search.binary_mode, SearchBinaryMode::Report)
    {
      return Ok(false);
    }

    let matched = if plan.config.search.invert_match {
      false
    } else {
      matcher.is_match(bytes)
    };

    match match_mode {
      MatchMode::Standard if matched => {
        sink.emit_binary_match(path.map(str::to_owned))?;
      }
      MatchMode::Count | MatchMode::CountMatches => {
        sink.emit_count(path.map(str::to_owned), usize::from(matched))?;
      }
      MatchMode::FilesWithMatches if matched => {
        if let Some(path) = path {
          sink.emit_file_match(path.to_owned())?;
        }
      }
      MatchMode::FilesWithoutMatch if !matched => {
        if let Some(path) = path {
          sink.emit_file_without_match(path.to_owned())?;
        }
      }
      _ => {}
    }

    Ok(true)
  }

  fn search_contents(
    &self,
    path: Option<&str>,
    bytes: &[u8],
    matcher: &super::matcher::CompiledMatcher,
    plan: &SearchPlan,
    match_mode: MatchMode,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<SearchStats> {
    if self.can_use_plain_standard_fast_path(plan, match_mode, capture_spans) {
      return self
        .search_contents_plain_standard_fast(path, bytes, matcher, plan, sink);
    }

    if self.can_use_candidate_standard_fast_path(plan, match_mode)
      && matcher.has_candidate_line_search()
    {
      return self.search_contents_standard_candidate_fast(
        path,
        bytes,
        matcher,
        plan,
        capture_spans,
        sink,
      );
    }

    if plan.config.context.before == 0
      && plan.config.context.after == 0
      && !plan.config.search.passthru
    {
      return self.search_contents_no_context_streaming(
        path,
        bytes,
        matcher,
        plan,
        match_mode,
        capture_spans,
        sink,
      );
    }

    let lines = super::util::split_records_with_numbers(
      bytes,
      if plan.config.search.null_data { b'\0' } else { b'\n' },
    );
    let mut matched_line_count = 0usize;
    let mut total_match_count = 0usize;
    let mut matched_entries = Vec::new();

    for (index, (_, _, line)) in lines.iter().enumerate() {
      let raw_match = matcher.is_match(line);
      let spans = if plan.config.search.invert_match || !capture_spans {
        Vec::new()
      } else if raw_match {
        matcher.line_spans(line)?
      } else {
        Vec::new()
      };
      let is_match = if plan.config.search.invert_match {
        !raw_match
      } else if capture_spans {
        !spans.is_empty()
      } else {
        raw_match
      };
      if !is_match {
        continue;
      }

      let span_count = if capture_spans { spans.len() } else { 1 };
      matched_line_count += 1;
      total_match_count += span_count;
      matched_entries.push((index, spans));

      if match_mode == MatchMode::FilesWithMatches {
        if let Some(path) = path {
          sink.emit_file_match(path.to_owned())?;
        }
        return Ok(SearchStats {
          matches: span_count,
          matched_lines: 1,
          ..SearchStats::default()
        });
      }
    }

    match match_mode {
      MatchMode::Standard => {
        self.emit_standard_lines(path, &lines, &matched_entries, plan, sink)?
      }
      MatchMode::Count
        if matched_line_count > 0 || plan.config.output.include_zero =>
      {
        sink.emit_count(path.map(str::to_owned), matched_line_count)?
      }
      MatchMode::CountMatches
        if total_match_count > 0 || plan.config.output.include_zero =>
      {
        sink.emit_count(path.map(str::to_owned), total_match_count)?
      }
      MatchMode::FilesWithoutMatch if matched_line_count == 0 => {
        if let Some(path) = path {
          sink.emit_file_without_match(path.to_owned())?;
        }
      }
      _ => {}
    }

    Ok(SearchStats {
      matches: total_match_count,
      matched_lines: matched_line_count,
      ..SearchStats::default()
    })
  }

  fn can_use_plain_standard_fast_path(
    &self,
    plan: &SearchPlan,
    match_mode: MatchMode,
    capture_spans: bool,
  ) -> bool {
    match_mode == MatchMode::Standard
      && !capture_spans
      && !plan.config.search.invert_match
      && !plan.config.search.passthru
      && plan.config.context.before == 0
      && plan.config.context.after == 0
      && !plan.config.output.only_matching
      && !plan.config.output.vimgrep
  }

  fn can_use_candidate_standard_fast_path(
    &self,
    plan: &SearchPlan,
    match_mode: MatchMode,
  ) -> bool {
    match_mode == MatchMode::Standard
      && !plan.config.search.invert_match
      && !plan.config.search.passthru
      && plan.config.context.before == 0
      && plan.config.context.after == 0
      && !plan.config.search.null_data
  }

  fn search_contents_plain_standard_fast(
    &self,
    path: Option<&str>,
    bytes: &[u8],
    matcher: &super::matcher::CompiledMatcher,
    plan: &SearchPlan,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<SearchStats> {
    let delimiter = if plan.config.search.null_data { b'\0' } else { b'\n' };
    let max_count = plan.config.search.max_count;
    let mut matched_line_count = 0usize;
    let mut total_match_count = 0usize;
    let mut line_start = 0usize;
    let mut line_end = next_record_end(bytes, line_start, delimiter);
    let mut line_number = 1usize;
    let mut last_emitted_line_start = None;
    let mut emit_err = None;
    let mut limit_reached = false;

    matcher.visit_match_ranges(bytes, |match_start, _| {
      if limit_reached {
        return;
      }
      while line_end < match_start {
        if line_end == bytes.len() {
          return;
        }
        line_start = line_end + 1;
        line_number += 1;
        line_end = next_record_end(bytes, line_start, delimiter);
      }

      if last_emitted_line_start == Some(line_start) {
        return;
      }

      let line = &bytes[line_start..line_end];
      matched_line_count += 1;
      total_match_count += 1;
      if let Err(err) =
        sink.emit_plain_match(path, line_number, line_start, line)
      {
        emit_err = Some(err);
        return;
      }
      last_emitted_line_start = Some(line_start);
      if max_count.is_some_and(|max_count| matched_line_count >= max_count) {
        limit_reached = true;
      }
    });

    if let Some(err) = emit_err {
      return Err(err);
    }

    Ok(SearchStats {
      matches: total_match_count,
      matched_lines: matched_line_count,
      ..SearchStats::default()
    })
  }

  fn search_contents_no_context_streaming(
    &self,
    path: Option<&str>,
    bytes: &[u8],
    matcher: &super::matcher::CompiledMatcher,
    plan: &SearchPlan,
    match_mode: MatchMode,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<SearchStats> {
    let delimiter = if plan.config.search.null_data { b'\0' } else { b'\n' };
    let mut matched_line_count = 0usize;
    let mut total_match_count = 0usize;
    let mut line_start = 0usize;
    let mut line_number = 1usize;

    while line_start < bytes.len() {
      let line_end = next_record_end(bytes, line_start, delimiter);
      let line = &bytes[line_start..line_end];
      let raw_match = matcher.is_match(line);
      let spans = if plan.config.search.invert_match || !capture_spans {
        Vec::new()
      } else if raw_match {
        matcher.line_spans(line)?
      } else {
        Vec::new()
      };
      let is_match = if plan.config.search.invert_match {
        !raw_match
      } else if capture_spans {
        !spans.is_empty()
      } else {
        raw_match
      };

      if is_match {
        let span_count = if capture_spans { spans.len() } else { 1 };
        matched_line_count += 1;
        total_match_count += span_count;

        match match_mode {
          MatchMode::Standard => {
            self.emit_stream_standard_match(
              path,
              line_number,
              line_start,
              line,
              &spans,
              plan,
              sink,
            )?;
            if plan
              .config
              .search
              .max_count
              .is_some_and(|max_count| matched_line_count >= max_count)
            {
              break;
            }
          }
          MatchMode::FilesWithMatches => {
            if let Some(path) = path {
              sink.emit_file_match(path.to_owned())?;
            }
            return Ok(SearchStats {
              matches: span_count,
              matched_lines: 1,
              ..SearchStats::default()
            });
          }
          MatchMode::Count
          | MatchMode::CountMatches
          | MatchMode::FilesWithoutMatch => {}
        }
      }

      if line_end == bytes.len() {
        break;
      }
      line_start = line_end + 1;
      line_number += 1;
    }

    match match_mode {
      MatchMode::Count
        if matched_line_count > 0 || plan.config.output.include_zero =>
      {
        sink.emit_count(path.map(str::to_owned), matched_line_count)?;
      }
      MatchMode::CountMatches
        if total_match_count > 0 || plan.config.output.include_zero =>
      {
        sink.emit_count(path.map(str::to_owned), total_match_count)?;
      }
      MatchMode::FilesWithoutMatch if matched_line_count == 0 => {
        if let Some(path) = path {
          sink.emit_file_without_match(path.to_owned())?;
        }
      }
      _ => {}
    }

    Ok(SearchStats {
      matches: total_match_count,
      matched_lines: matched_line_count,
      ..SearchStats::default()
    })
  }

  fn search_contents_standard_candidate_fast(
    &self,
    path: Option<&str>,
    bytes: &[u8],
    matcher: &super::matcher::CompiledMatcher,
    plan: &SearchPlan,
    capture_spans: bool,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<SearchStats> {
    let delimiter = b'\n';
    let mut matched_line_count = 0usize;
    let mut total_match_count = 0usize;
    let mut line_start = 0usize;
    let mut line_number = 1usize;

    while line_start < bytes.len() {
      let Some(candidate) = matcher.find_candidate_line(&bytes[line_start..])
      else {
        break;
      };
      let candidate_offset = match candidate {
        super::matcher::CandidateLineMatch::Confirmed(offset)
        | super::matcher::CandidateLineMatch::Candidate(offset) => {
          line_start + offset
        }
      };
      let candidate_line_start =
        match memrchr(delimiter, &bytes[line_start..candidate_offset]) {
          Some(offset) => line_start + offset + 1,
          None => line_start,
        };
      line_number +=
        memchr_iter(delimiter, &bytes[line_start..candidate_line_start])
          .count();
      line_start = candidate_line_start;
      let line_end = next_record_end(bytes, line_start, delimiter);
      let line = &bytes[line_start..line_end];
      let raw_match = match candidate {
        super::matcher::CandidateLineMatch::Confirmed(_) => true,
        super::matcher::CandidateLineMatch::Candidate(_) => {
          matcher.is_match(line)
        }
      };
      let spans = if !capture_spans {
        Vec::new()
      } else if raw_match {
        matcher.line_spans(line)?
      } else {
        Vec::new()
      };
      let is_match = if capture_spans { !spans.is_empty() } else { raw_match };

      if is_match {
        matched_line_count += 1;
        total_match_count += if capture_spans { spans.len() } else { 1 };
        self.emit_stream_standard_match(
          path,
          line_number,
          line_start,
          line,
          &spans,
          plan,
          sink,
        )?;
        if plan
          .config
          .search
          .max_count
          .is_some_and(|max_count| matched_line_count >= max_count)
        {
          break;
        }
      }

      if line_end == bytes.len() {
        break;
      }
      line_start = line_end + 1;
      line_number += 1;
    }

    Ok(SearchStats {
      matches: total_match_count,
      matched_lines: matched_line_count,
      ..SearchStats::default()
    })
  }

  fn emit_standard_lines(
    &self,
    path: Option<&str>,
    lines: &[(usize, usize, &[u8])],
    matched_entries: &[(usize, Vec<MatchSpan>)],
    plan: &SearchPlan,
    sink: &mut dyn SearchOutcomeSink,
  ) -> io::Result<()> {
    if plan.config.search.passthru {
      let mut emitted_matches = 0usize;
      let match_positions: std::collections::HashSet<usize> =
        matched_entries.iter().map(|(index, _)| *index).collect();
      let match_spans = matched_entries
        .iter()
        .map(|(index, spans)| (*index, spans.clone()))
        .collect::<std::collections::HashMap<usize, Vec<MatchSpan>>>();
      for (index, (line_number, absolute_offset, line)) in
        lines.iter().copied().enumerate()
      {
        if match_positions.contains(&index) {
          let spans = match_spans.get(&index).cloned().unwrap_or_default();
          sink.emit_match(MatchRecord::new_with_offset(
            path.map(str::to_owned),
            line_number,
            absolute_offset,
            line.to_vec(),
            if plan.config.search.invert_match { Vec::new() } else { spans },
          )?)?;
          emitted_matches += 1;
          if plan
            .config
            .search
            .max_count
            .is_some_and(|max_count| emitted_matches >= max_count)
          {
            for (line_number, absolute_offset, line) in
              lines.iter().copied().skip(index + 1)
            {
              sink.emit_context(MatchRecord::new_with_offset(
                path.map(str::to_owned),
                line_number,
                absolute_offset,
                line.to_vec(),
                vec![],
              )?)?;
            }
            break;
          }
        } else {
          sink.emit_context(MatchRecord::new_with_offset(
            path.map(str::to_owned),
            line_number,
            absolute_offset,
            line.to_vec(),
            vec![],
          )?)?;
        }
      }
      return Ok(());
    }

    if plan.config.context.before == 0 && plan.config.context.after == 0 {
      let mut emitted = 0usize;
      for (index, spans) in matched_entries {
        let (line_number, absolute_offset, line) = lines[*index];
        if plan.config.output.vimgrep && !plan.config.search.invert_match {
          for span in spans.iter().cloned() {
            sink.emit_match(MatchRecord::new_with_offset(
              path.map(str::to_owned),
              line_number,
              absolute_offset,
              line.to_vec(),
              vec![span],
            )?)?;
          }
        } else if plan.config.output.only_matching
          && !plan.config.search.invert_match
        {
          for span in spans.iter().cloned() {
            sink.emit_match(MatchRecord::new_with_offset(
              path.map(str::to_owned),
              line_number,
              absolute_offset + span.start,
              line[span.start..span.end].to_vec(),
              vec![MatchSpan::new(0, span.end - span.start)?],
            )?)?;
          }
        } else {
          sink.emit_match(MatchRecord::new_with_offset(
            path.map(str::to_owned),
            line_number,
            absolute_offset,
            line.to_vec(),
            if plan.config.search.invert_match {
              Vec::new()
            } else {
              spans.clone()
            },
          )?)?;
        }
        emitted += 1;
        if plan
          .config
          .search
          .max_count
          .is_some_and(|max_count| emitted >= max_count)
        {
          break;
        }
      }
      return Ok(());
    }

    let limited_matches = if let Some(max_count) = plan.config.search.max_count
    {
      &matched_entries[..matched_entries.len().min(max_count)]
    } else {
      matched_entries
    };
    if limited_matches.is_empty() {
      return Ok(());
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &(index, _) in limited_matches {
      let start = index.saturating_sub(plan.config.context.before);
      let end =
        (index + plan.config.context.after).min(lines.len().saturating_sub(1));
      if let Some((_, last_end)) = ranges.last_mut() {
        if start <= *last_end + 1 {
          *last_end = (*last_end).max(end);
          continue;
        }
      }
      ranges.push((start, end));
    }

    let mut match_positions = std::collections::HashSet::new();
    let mut match_spans = std::collections::HashMap::new();
    for (index, spans) in limited_matches {
      match_positions.insert(index);
      match_spans.insert(*index, spans.clone());
    }

    for (range_index, (start, end)) in ranges.iter().enumerate() {
      if range_index > 0 {
        sink.emit_context_separator()?;
      }
      for index in *start..=*end {
        let (line_number, absolute_offset, line) = lines[index];
        if match_positions.contains(&index) {
          let spans = match_spans.get(&index).cloned().unwrap_or_default();
          sink.emit_match(MatchRecord::new_with_offset(
            path.map(str::to_owned),
            line_number,
            absolute_offset,
            line.to_vec(),
            if plan.config.search.invert_match { Vec::new() } else { spans },
          )?)?;
        } else {
          sink.emit_context(MatchRecord::new_with_offset(
            path.map(str::to_owned),
            line_number,
            absolute_offset,
            line.to_vec(),
            vec![],
          )?)?;
        }
      }
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn capped_parallel_worker_count_limits_cpu_oversubscription() {
    assert!(
      capped_parallel_worker_count(None, None) <= MAX_PARALLEL_SEARCH_WORKERS
    );
    assert_eq!(capped_parallel_worker_count(None, Some(1)), 1);
    assert_eq!(capped_parallel_worker_count(None, Some(3)), 3);
    assert_eq!(capped_parallel_worker_count(Some(1), Some(8)), 1);
  }

  #[test]
  fn plain_standard_fast_path_emits_each_matching_line_once() {
    let cmd = RgCommand::parse_args(&["testing".into(), "sample.txt".into()])
      .expect("parse rg command");
    let plan = SearchPlan::from_command(&cmd);
    let matcher =
      super::matcher::CompiledMatcher::new(&plan.config.pattern_spec)
        .expect("compile matcher");
    let mut emitter = SearchResultEmitter::new();

    let stats = SearchEngine::default()
      .search_contents_plain_standard_fast(
        Some("sample.txt"),
        b"testing testing\nskip\n",
        &matcher,
        &plan,
        &mut emitter,
      )
      .expect("search contents");

    assert_eq!(
      emitter.into_outcomes(),
      vec![SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          1,
          0,
          b"testing testing".to_vec(),
          Vec::new(),
        )
        .unwrap(),
      )]
    );
    assert_eq!(stats.matches, 1);
    assert_eq!(stats.matched_lines, 1);
  }
}
