use std::{
  collections::VecDeque,
  io,
  sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  },
  thread,
  time::{Duration, Instant},
};

use crossbeam_deque::{Injector, Steal, Stealer, Worker as DequeWorker};
use kanal::Sender;
use lio::{Lio, api::resource::Resource};

use super::{
  render::{ByteSink, StreamingRenderer},
  search::{
    LIO_READ_BATCH_SIZE, LOCAL_DIR_TASK_THRESHOLD, SearchFile,
    TRAVERSAL_TASK_BURST_SIZE, WRITER_CHANNEL_FLUSH_THRESHOLD,
    WorkerFilePipeline,
  },
  walker::FileWalker,
  *,
};
use crate::app::AppContext;

pub(super) struct Worker {
  ctx: AppContext,
  plan: Arc<SearchPlan>,
  presentation: PresentationSpec,
  writer_tx: Sender<WriterMessage>,
  coordination: WorkerCoordination,
  handoff: WorkerHandoff,
  admission_stage: AdmissionStage,
  drain_stage: DrainStage,
  traversal_stage: TraversalStage,
  idle_stage: IdleStage,
  profile: WorkerLoopProfile,
}

impl Worker {
  pub(super) fn new(
    worker_index: usize,
    assignment: WorkerAssignment,
    local_tasks: DequeWorker<super::walker::ParallelWalkTask>,
    plan: Arc<SearchPlan>,
    matcher: super::matcher::WorkerMatcher,
    presentation: PresentationSpec,
    writer_tx: Sender<WriterMessage>,
    include_path: bool,
    effective_match_mode: MatchMode,
    capture_spans: bool,
    outstanding_tasks: Arc<AtomicUsize>,
    global_tasks: Arc<Injector<super::walker::ParallelWalkTask>>,
    stealers: Arc<Vec<Stealer<super::walker::ParallelWalkTask>>>,
    walker: FileWalker,
  ) -> io::Result<Self> {
    Ok(Self {
      ctx: AppContext::new()?,
      plan,
      presentation,
      writer_tx,
      coordination: WorkerCoordination { outstanding_tasks },
      handoff: WorkerHandoff {
        ready_files: VecDeque::from(assignment.immediate_files),
      },
      admission_stage: AdmissionStage,
      drain_stage: DrainStage {
        engine: SearchEngine::default(),
        include_path,
        effective_match_mode,
        capture_spans,
        file_pipeline: WorkerFilePipeline::new(),
        matcher,
        stats: SearchStats::default(),
      },
      traversal_stage: TraversalStage {
        worker_index,
        global_tasks,
        stealers,
        walker,
        local_tasks,
      },
      idle_stage: IdleStage {
        idle_spins: 0,
        last_runtime_call_end: None,
        search_match_time_at_last_runtime_call: Duration::ZERO,
      },
      profile: WorkerLoopProfile::default(),
    })
  }

  pub(super) fn run(mut self) -> io::Result<(SearchStats, WorkerLoopProfile)> {
    let plan = Arc::clone(&self.plan);
    let mut renderer = StreamingRenderer::new(
      self.presentation,
      plan.as_ref(),
      ChannelByteSink::new(self.writer_tx.clone()),
    );

    loop {
      self.profile.iterations += 1;
      let admitted_files = self.admission_stage.run(
        self.ctx.lio(),
        &self.ctx.cwd(),
        &mut self.handoff,
        &mut self.drain_stage.file_pipeline,
        &mut self.profile,
      )?;

      let drain = self.drain_stage.run(
        self.ctx.lio(),
        plan.as_ref(),
        &mut renderer,
        &mut self.profile,
      )?;

      if drain.should_stop {
        break;
      }

      let traversal = self.traversal_stage.run(
        &self.ctx,
        &self.coordination,
        &mut self.handoff,
        &self.drain_stage.file_pipeline,
        &mut self.profile,
      )?;

      let progressed =
        admitted_files > 0 || drain.drained > 0 || traversal.progressed;

      if progressed {
        self.idle_stage.idle_spins = 0;
        continue;
      }

      if self.idle_stage.run(
        self.ctx.lio(),
        &self.coordination,
        &mut self.drain_stage.file_pipeline,
        &mut self.profile,
      )? {
        break;
      }
    }

    let stats = self.drain_stage.stats;
    match renderer.finish(stats, Duration::ZERO, Duration::ZERO) {
      Ok(writer) => {
        writer.finish_worker(stats)?;
        Ok((stats, self.profile))
      }
      Err(err) => {
        let sink = ChannelByteSink::new(self.writer_tx);
        sink.send_error(io::Error::new(err.kind(), err.to_string()));
        let _ = sink.finish_worker(stats);
        Err(err)
      }
    }
  }
}

struct WorkerCoordination {
  outstanding_tasks: Arc<AtomicUsize>,
}

struct WorkerHandoff {
  ready_files: VecDeque<SearchFile>,
}

struct AdmissionStage;

struct DrainStage {
  engine: SearchEngine,
  include_path: bool,
  effective_match_mode: MatchMode,
  capture_spans: bool,
  file_pipeline: WorkerFilePipeline,
  matcher: super::matcher::WorkerMatcher,
  stats: SearchStats,
}

struct TraversalStage {
  worker_index: usize,
  global_tasks: Arc<Injector<super::walker::ParallelWalkTask>>,
  stealers: Arc<Vec<Stealer<super::walker::ParallelWalkTask>>>,
  walker: FileWalker,
  local_tasks: DequeWorker<super::walker::ParallelWalkTask>,
}

struct IdleStage {
  idle_spins: usize,
  last_runtime_call_end: Option<Instant>,
  search_match_time_at_last_runtime_call: Duration,
}

#[derive(Debug, Default, Clone, Copy)]
struct DrainStageResult {
  drained: usize,
  should_stop: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct TraversalStageResult {
  progressed: bool,
}

#[derive(Debug)]
enum NextTraversalTask {
  Task(super::walker::ParallelWalkTask),
  Retry,
  Empty,
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct WorkerLoopProfile {
  pub(super) iterations: usize,
  pub(super) traversal_tasks: usize,
  pub(super) file_events_drained: usize,
  pub(super) open_events: usize,
  pub(super) read_events: usize,
  pub(super) files_admitted: usize,
  pub(super) sync_files_processed: usize,
  pub(super) try_run_calls: usize,
  pub(super) try_run_completions: usize,
  pub(super) try_run_empty_calls: usize,
  pub(super) drive_wait_calls: usize,
  pub(super) traversal_time: Duration,
  pub(super) traversal_split_time: Duration,
  pub(super) traversal_filter_time: Duration,
  pub(super) drain_time: Duration,
  pub(super) open_event_time: Duration,
  pub(super) read_event_time: Duration,
  pub(super) match_render_time: Duration,
  pub(super) sync_search_time: Duration,
  pub(super) try_run_time: Duration,
  pub(super) drive_wait_time: Duration,
  pub(super) between_runtime_non_search_time: Duration,
  pub(super) idle_time: Duration,
}

impl WorkerLoopProfile {
  pub(super) fn merge(self, other: Self) -> Self {
    Self {
      iterations: self.iterations + other.iterations,
      traversal_tasks: self.traversal_tasks + other.traversal_tasks,
      file_events_drained: self.file_events_drained + other.file_events_drained,
      open_events: self.open_events + other.open_events,
      read_events: self.read_events + other.read_events,
      files_admitted: self.files_admitted + other.files_admitted,
      sync_files_processed: self.sync_files_processed
        + other.sync_files_processed,
      try_run_calls: self.try_run_calls + other.try_run_calls,
      try_run_completions: self.try_run_completions + other.try_run_completions,
      try_run_empty_calls: self.try_run_empty_calls + other.try_run_empty_calls,
      drive_wait_calls: self.drive_wait_calls + other.drive_wait_calls,
      traversal_time: self.traversal_time + other.traversal_time,
      traversal_split_time: self.traversal_split_time
        + other.traversal_split_time,
      traversal_filter_time: self.traversal_filter_time
        + other.traversal_filter_time,
      drain_time: self.drain_time + other.drain_time,
      open_event_time: self.open_event_time + other.open_event_time,
      read_event_time: self.read_event_time + other.read_event_time,
      match_render_time: self.match_render_time + other.match_render_time,
      sync_search_time: self.sync_search_time + other.sync_search_time,
      try_run_time: self.try_run_time + other.try_run_time,
      drive_wait_time: self.drive_wait_time + other.drive_wait_time,
      between_runtime_non_search_time: self.between_runtime_non_search_time
        + other.between_runtime_non_search_time,
      idle_time: self.idle_time + other.idle_time,
    }
  }

  fn search_match_time(&self) -> Duration {
    self.traversal_time + self.match_render_time + self.sync_search_time
  }
}

impl AdmissionStage {
  fn run(
    &mut self,
    lio: &Lio,
    cwd: &Resource,
    handoff: &mut WorkerHandoff,
    file_pipeline: &mut WorkerFilePipeline,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<usize> {
    let mut admitted = 0usize;
    while self.try_admit_next_file(lio, cwd, handoff, file_pipeline, profile)? {
      admitted += 1;
    }
    Ok(admitted)
  }

  fn try_admit_next_file(
    &mut self,
    lio: &Lio,
    cwd: &Resource,
    handoff: &mut WorkerHandoff,
    file_pipeline: &mut WorkerFilePipeline,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<bool> {
    if !file_pipeline.has_capacity() {
      return Ok(false);
    }

    let Some(file) = handoff.ready_files.pop_front() else {
      return Ok(false);
    };

    file_pipeline.submit_file(lio, cwd, file)?;
    profile.files_admitted += 1;
    Ok(true)
  }
}

impl DrainStage {
  fn run(
    &mut self,
    lio: &Lio,
    plan: &SearchPlan,
    renderer: &mut StreamingRenderer<'_, ChannelByteSink>,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<DrainStageResult> {
    let drain_started = Instant::now();
    let (drained, should_stop, _) = self.engine.drain_worker_file_events(
      &mut self.file_pipeline,
      lio,
      plan,
      self.include_path,
      self.effective_match_mode,
      &mut self.matcher,
      self.capture_spans,
      renderer,
      &mut self.stats,
      profile,
    )?;
    profile.drain_time += drain_started.elapsed();
    profile.file_events_drained += drained;
    Ok(DrainStageResult { drained, should_stop })
  }
}

impl TraversalStage {
  fn run(
    &mut self,
    ctx: &AppContext,
    coordination: &WorkerCoordination,
    handoff: &mut WorkerHandoff,
    file_pipeline: &WorkerFilePipeline,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<TraversalStageResult> {
    let mut progressed = false;
    let mut traversal_budget =
      self.traversal_budget(&handoff.ready_files, file_pipeline);

    while traversal_budget > 0 {
      traversal_budget -= 1;
      let task = match self.pop_next_task() {
        NextTraversalTask::Task(task) => task,
        NextTraversalTask::Retry => continue,
        NextTraversalTask::Empty => break,
      };

      progressed = true;
      if self.process_task(
        task,
        ctx,
        coordination,
        handoff,
        file_pipeline,
        profile,
      )? {
        break;
      }
    }

    Ok(TraversalStageResult { progressed })
  }

  fn traversal_budget(
    &self,
    ready_files: &VecDeque<SearchFile>,
    file_pipeline: &WorkerFilePipeline,
  ) -> usize {
    if ready_files.len() < LIO_READ_BATCH_SIZE
      && file_pipeline.file_count() < LIO_READ_BATCH_SIZE
    {
      LOCAL_DIR_TASK_THRESHOLD * 2
    } else {
      TRAVERSAL_TASK_BURST_SIZE
    }
  }

  fn pop_next_task(&self) -> NextTraversalTask {
    if let Some(task) = self.local_tasks.pop() {
      return NextTraversalTask::Task(task);
    }

    match self.global_tasks.steal() {
      Steal::Success(task) => NextTraversalTask::Task(task),
      Steal::Retry => NextTraversalTask::Retry,
      Steal::Empty => {
        for (index, stealer) in self.stealers.iter().enumerate() {
          if index == self.worker_index {
            continue;
          }
          match stealer.steal_batch_and_pop(&self.local_tasks) {
            Steal::Success(task) => return NextTraversalTask::Task(task),
            Steal::Retry => return NextTraversalTask::Retry,
            Steal::Empty => {}
          }
        }
        NextTraversalTask::Empty
      }
    }
  }

  fn process_task(
    &mut self,
    task: super::walker::ParallelWalkTask,
    ctx: &AppContext,
    coordination: &WorkerCoordination,
    handoff: &mut WorkerHandoff,
    file_pipeline: &WorkerFilePipeline,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<bool> {
    profile.traversal_tasks += 1;
    let split_started = Instant::now();
    let (files, child_tasks) = self.walker.split_parallel_task(ctx, task)?;
    let split_elapsed = split_started.elapsed();
    profile.traversal_split_time += split_elapsed;

    let filter_started = Instant::now();
    self.enqueue_child_tasks(child_tasks, coordination);
    coordination.outstanding_tasks.fetch_sub(1, Ordering::AcqRel);
    let should_pause = self.enqueue_files(files, handoff, file_pipeline);
    let filter_elapsed = filter_started.elapsed();
    profile.traversal_filter_time += filter_elapsed;
    profile.traversal_time += split_elapsed + filter_elapsed;
    Ok(should_pause)
  }

  fn enqueue_child_tasks(
    &self,
    child_tasks: Vec<super::walker::ParallelWalkTask>,
    coordination: &WorkerCoordination,
  ) {
    if child_tasks.is_empty() {
      return;
    }

    coordination
      .outstanding_tasks
      .fetch_add(child_tasks.len(), Ordering::AcqRel);
    let mut local_kept = 0usize;
    for child in child_tasks {
      if self.local_tasks.len() < LOCAL_DIR_TASK_THRESHOLD && local_kept < 8 {
        self.local_tasks.push(child);
        local_kept += 1;
      } else {
        self.global_tasks.push(child);
      }
    }
    if self.local_tasks.len() > LOCAL_DIR_TASK_THRESHOLD * 2 {
      let spill =
        self.local_tasks.len().saturating_sub(LOCAL_DIR_TASK_THRESHOLD);
      for _ in 0..spill {
        if let Some(task) = self.local_tasks.pop() {
          self.global_tasks.push(task);
        }
      }
    }
  }

  fn enqueue_files(
    &mut self,
    files: Vec<super::walker::WalkFile>,
    handoff: &mut WorkerHandoff,
    file_pipeline: &WorkerFilePipeline,
  ) -> bool {
    if files.is_empty() {
      return false;
    }

    handoff
      .ready_files
      .extend(files.into_iter().map(SearchFile::from_walk_file));
    handoff.ready_files.len() >= LIO_READ_BATCH_SIZE
      || !file_pipeline.has_capacity()
  }
}

impl IdleStage {
  fn run(
    &mut self,
    lio: &Lio,
    coordination: &WorkerCoordination,
    file_pipeline: &mut WorkerFilePipeline,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<bool> {
    let outstanding = coordination.outstanding_tasks.load(Ordering::Acquire);

    if file_pipeline.has_pending_work() {
      return self.run_pending_io(lio, outstanding, file_pipeline, profile);
    }

    if outstanding == 0 {
      return Ok(true);
    }

    self.backoff(256, profile);
    Ok(false)
  }

  fn run_pending_io(
    &mut self,
    lio: &Lio,
    outstanding: usize,
    file_pipeline: &mut WorkerFilePipeline,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<bool> {
    let completions = self.try_run_pending_io(lio, profile)?;
    if completions > 0 {
      profile.try_run_completions += completions;
      self.idle_spins = 0;
      return Ok(false);
    }

    profile.try_run_empty_calls += 1;
    if outstanding == 0 {
      self.drive_pending_io_wait(lio, file_pipeline, profile)?;
    } else {
      self.backoff(1024, profile);
    }
    Ok(false)
  }

  fn try_run_pending_io(
    &mut self,
    lio: &Lio,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<usize> {
    self.record_between_runtime_non_search_time(profile);
    profile.try_run_calls += 1;
    let try_run_started = Instant::now();
    let completions = lio.try_run()?;
    profile.try_run_time += try_run_started.elapsed();
    self.finish_runtime_call(profile);
    Ok(completions)
  }

  fn drive_pending_io_wait(
    &mut self,
    lio: &Lio,
    file_pipeline: &mut WorkerFilePipeline,
    profile: &mut WorkerLoopProfile,
  ) -> io::Result<()> {
    self.record_between_runtime_non_search_time(profile);
    profile.drive_wait_calls += 1;
    let drive_wait_started = Instant::now();
    file_pipeline.drive_wait(lio)?;
    profile.drive_wait_time += drive_wait_started.elapsed();
    self.finish_runtime_call(profile);
    self.idle_spins = 0;
    Ok(())
  }

  fn record_between_runtime_non_search_time(
    &mut self,
    profile: &mut WorkerLoopProfile,
  ) {
    let Some(last_runtime_call_end) = self.last_runtime_call_end else {
      return;
    };
    let elapsed = last_runtime_call_end.elapsed();
    let search_match_elapsed = profile
      .search_match_time()
      .saturating_sub(self.search_match_time_at_last_runtime_call);
    profile.between_runtime_non_search_time +=
      elapsed.saturating_sub(search_match_elapsed);
  }

  fn finish_runtime_call(&mut self, profile: &WorkerLoopProfile) {
    self.last_runtime_call_end = Some(Instant::now());
    self.search_match_time_at_last_runtime_call = profile.search_match_time();
  }

  fn backoff(
    &mut self,
    yield_threshold: usize,
    profile: &mut WorkerLoopProfile,
  ) {
    let idle_started = Instant::now();
    self.idle_spins += 1;
    if self.idle_spins < 64 {
      std::hint::spin_loop();
    } else if self.idle_spins < yield_threshold {
      thread::yield_now();
    } else {
      thread::sleep(Duration::from_micros(50));
    }
    profile.idle_time += idle_started.elapsed();
  }
}

pub(super) enum WriterMessage {
  Chunk(Vec<u8>),
  WorkerDone(SearchStats),
  Error(io::Error),
}

struct ChannelByteSink {
  tx: Sender<WriterMessage>,
  pending: Vec<u8>,
}

impl ChannelByteSink {
  fn new(tx: Sender<WriterMessage>) -> Self {
    Self { tx, pending: Vec::with_capacity(WRITER_CHANNEL_FLUSH_THRESHOLD) }
  }

  fn flush_pending(&mut self) -> io::Result<()> {
    if self.pending.is_empty() {
      return Ok(());
    }
    self
      .tx
      .send(WriterMessage::Chunk(std::mem::take(&mut self.pending)))
      .map_err(|err| {
        io::Error::other(format!("rg: writer channel failed: {err}"))
      })
  }

  fn finish_worker(mut self, stats: SearchStats) -> io::Result<()> {
    self.flush_pending()?;
    self.tx.send(WriterMessage::WorkerDone(stats)).map_err(|err| {
      io::Error::other(format!("rg: writer channel failed: {err}"))
    })
  }

  fn send_error(&self, err: io::Error) {
    let _ = self.tx.send(WriterMessage::Error(err));
  }
}

impl ByteSink for ChannelByteSink {
  fn write_chunk(&mut self, bytes: Vec<u8>) -> io::Result<()> {
    self.pending.extend_from_slice(&bytes);
    if self.pending.len() >= WRITER_CHANNEL_FLUSH_THRESHOLD {
      self.flush_pending()?;
    }
    Ok(())
  }
}

#[derive(Default)]
pub(super) struct WorkerAssignment {
  pub(super) immediate_files: Vec<SearchFile>,
  pub(super) shards: Vec<super::walker::ParallelWalkTask>,
}
