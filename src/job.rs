use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::git::DependencyMode;
use crate::workspace::{
    BuildProgress, BuildStage, ErrorCode, IndexCompletion, OperationError, RootIdentity,
    SnapshotTarget,
};

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct JobRequestSummary {
    pub root: RootIdentity,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub target: SnapshotTarget,
    pub dependency_mode: DependencyMode,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
#[allow(clippy::large_enum_variant)]
pub enum JobState {
    Queued,
    Capturing,
    SelectingSeed,
    Indexing {
        files_done: usize,
        files_total: usize,
        files_reused: usize,
        files_parsed: usize,
    },
    ResolvingGraph,
    Publishing,
    Completed {
        completion: IndexCompletion,
    },
    Failed {
        error: OperationError,
    },
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct JobStatus {
    pub job_id: String,
    pub workspace_id: String,
    pub request: JobRequestSummary,
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_cache: Option<String>,
}

pub struct JobReporter {
    job: Arc<Job>,
}

struct Job {
    job_id: String,
    workspace_id: String,
    request: JobRequestSummary,
    state: Mutex<JobData>,
    cancelled: Arc<AtomicBool>,
}

struct JobData {
    state: JobState,
    rejected_cache: Option<String>,
    progress: Option<BuildProgress>,
}

struct RegistryState {
    jobs: HashMap<String, Arc<Job>>,
    active_workspaces: HashMap<String, ActiveJob>,
    handles: Vec<JoinHandle<()>>,
}

struct ActiveJob {
    job_id: String,
    request_key: String,
}

pub struct JobRegistry {
    next_id: AtomicU64,
    state: Mutex<RegistryState>,
}

impl JobRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            state: Mutex::new(RegistryState {
                jobs: HashMap::new(),
                active_workspaces: HashMap::new(),
                handles: Vec::new(),
            }),
        })
    }

    pub fn start(
        self: &Arc<Self>,
        workspace_id: String,
        request_key: String,
        request: JobRequestSummary,
        work: impl FnOnce(JobReporter, Arc<AtomicBool>) -> Result<IndexCompletion, OperationError>
        + Send
        + 'static,
    ) -> Result<JobStatus, OperationError> {
        self.start_with_hook(workspace_id, request_key, request, work, || {})
    }

    fn start_with_hook(
        self: &Arc<Self>,
        workspace_id: String,
        request_key: String,
        request: JobRequestSummary,
        work: impl FnOnce(JobReporter, Arc<AtomicBool>) -> Result<IndexCompletion, OperationError>
        + Send
        + 'static,
        after_spawn: impl FnOnce(),
    ) -> Result<JobStatus, OperationError> {
        let mut registry = self.state.lock().expect("job registry poisoned");
        if let Some(active) = registry.active_workspaces.get(&workspace_id) {
            if active.request_key == request_key {
                return Ok(registry.jobs[&active.job_id].status());
            }
            return Err(OperationError::new(
                ErrorCode::WorkspaceBusy,
                "workspace already has an active indexing job",
            )
            .with_detail("active_job_id", &active.job_id));
        }
        let job_id = format!("job-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let job = Arc::new(Job {
            job_id: job_id.clone(),
            workspace_id,
            request,
            state: Mutex::new(JobData {
                state: JobState::Queued,
                rejected_cache: None,
                progress: None,
            }),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        registry.jobs.insert(job_id.clone(), job.clone());
        registry.active_workspaces.insert(
            job.workspace_id.clone(),
            ActiveJob {
                job_id: job_id.clone(),
                request_key,
            },
        );
        let (start_tx, start_rx) = mpsc::sync_channel(0);
        let handle = thread::Builder::new().name(job_id).spawn({
            let job = job.clone();
            let cancelled = job.cancelled.clone();
            let registry = self.clone();
            move || {
                if start_rx.recv().is_err() {
                    return;
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    work(JobReporter { job: job.clone() }, cancelled)
                }))
                .unwrap_or_else(|_| {
                    Err(OperationError::new(
                        ErrorCode::Internal,
                        "indexing worker panicked",
                    ))
                });
                registry.finish(&job, result);
            }
        });
        match handle {
            Ok(handle) => {
                registry.handles.push(handle);
                let status = job.status();
                drop(registry);
                start_tx.send(()).expect("registered worker disappeared");
                after_spawn();
                Ok(status)
            }
            Err(error) => {
                job.state.lock().expect("job state poisoned").state = JobState::Failed {
                    error: OperationError::new(
                        ErrorCode::Internal,
                        format!("cannot start indexing job: {error}"),
                    ),
                };
                if registry
                    .active_workspaces
                    .get(&job.workspace_id)
                    .is_some_and(|active| active.job_id == job.job_id)
                {
                    registry.active_workspaces.remove(&job.workspace_id);
                }
                Ok(job.status())
            }
        }
    }

    pub fn status(&self, job_id: &str) -> Result<JobStatus, OperationError> {
        let job = self
            .state
            .lock()
            .expect("job registry poisoned")
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| {
                OperationError::new(ErrorCode::JobNotFound, "indexing job not found")
                    .with_detail("job_id", job_id)
            })?;
        Ok(job.status())
    }

    pub fn cancel(&self, job_id: &str) -> Result<JobStatus, OperationError> {
        let job = self
            .state
            .lock()
            .expect("job registry poisoned")
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| {
                OperationError::new(ErrorCode::JobNotFound, "indexing job not found")
                    .with_detail("job_id", job_id)
            })?;
        let state = job.state.lock().expect("job state poisoned");
        if !state.state.is_terminal() {
            job.cancelled.store(true, Ordering::Relaxed);
        }
        Ok(job.status_from(&state))
    }

    pub fn close(&self) {
        self.close_after_cancel(|| {});
    }

    fn close_after_cancel(&self, after_cancel: impl FnOnce()) {
        let handles = {
            let mut registry = self.state.lock().expect("job registry poisoned");
            for active in registry.active_workspaces.values() {
                registry.jobs[&active.job_id]
                    .cancelled
                    .store(true, Ordering::Relaxed);
            }
            std::mem::take(&mut registry.handles)
        };
        after_cancel();
        for handle in handles {
            let _ = handle.join();
        }
    }

    fn finish(&self, job: &Job, result: Result<IndexCompletion, OperationError>) {
        let mut registry = self.state.lock().expect("job registry poisoned");
        job.state.lock().expect("job state poisoned").state = match result {
            Ok(completion) => JobState::Completed { completion },
            Err(error)
                if job.cancelled.load(Ordering::Relaxed)
                    && error.code == ErrorCode::JobCancelled =>
            {
                JobState::Cancelled
            }
            Err(error) => JobState::Failed { error },
        };
        if registry
            .active_workspaces
            .get(&job.workspace_id)
            .is_some_and(|active| active.job_id == job.job_id)
        {
            registry.active_workspaces.remove(&job.workspace_id);
        }
    }
}

impl JobReporter {
    pub fn report(&self, progress: BuildProgress) {
        let mut state = self.job.state.lock().expect("job state poisoned");
        let valid_stage = match state.state.stage() {
            None => {
                matches!(state.state, JobState::Queued) && progress.stage == BuildStage::Capturing
            }
            Some(current) => {
                progress.stage == current || stage_rank(progress.stage) == stage_rank(current) + 1
            }
        };
        let valid_counters = state.progress.as_ref().is_none_or(|previous| {
            (previous.stage == BuildStage::Capturing && progress.stage == BuildStage::SelectingSeed)
                || (progress.files_done >= previous.files_done
                    && progress.files_total >= previous.files_total
                    && progress.files_reused >= previous.files_reused
                    && progress.files_parsed >= previous.files_parsed)
        });
        if !valid_stage || !valid_counters {
            return;
        }
        state.state = match progress.stage {
            BuildStage::Capturing => JobState::Capturing,
            BuildStage::SelectingSeed => JobState::SelectingSeed,
            BuildStage::Indexing => JobState::Indexing {
                files_done: progress.files_done,
                files_total: progress.files_total,
                files_reused: progress.files_reused,
                files_parsed: progress.files_parsed,
            },
            BuildStage::ResolvingGraph => JobState::ResolvingGraph,
            BuildStage::Publishing => JobState::Publishing,
        };
        if progress.rejected_cache.is_some() {
            state.rejected_cache = progress.rejected_cache.clone();
        }
        state.progress = Some(progress);
    }
}

impl Job {
    fn status(&self) -> JobStatus {
        let state = self.state.lock().expect("job state poisoned");
        self.status_from(&state)
    }

    fn status_from(&self, state: &JobData) -> JobStatus {
        JobStatus {
            job_id: self.job_id.clone(),
            workspace_id: self.workspace_id.clone(),
            request: self.request.clone(),
            state: state.state.clone(),
            rejected_cache: state.rejected_cache.clone(),
        }
    }
}

impl JobState {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled
        )
    }

    fn stage(&self) -> Option<BuildStage> {
        match self {
            Self::Capturing => Some(BuildStage::Capturing),
            Self::SelectingSeed => Some(BuildStage::SelectingSeed),
            Self::Indexing { .. } => Some(BuildStage::Indexing),
            Self::ResolvingGraph => Some(BuildStage::ResolvingGraph),
            Self::Publishing => Some(BuildStage::Publishing),
            Self::Queued | Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled => None,
        }
    }
}

const fn stage_rank(stage: BuildStage) -> u8 {
    match stage {
        BuildStage::Capturing => 0,
        BuildStage::SelectingSeed => 1,
        BuildStage::Indexing => 2,
        BuildStage::ResolvingGraph => 3,
        BuildStage::Publishing => 4,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    use crate::git::DependencyMode;
    use crate::index::IndexStats;
    use crate::workspace::{
        BuildProgress, BuildStage, ErrorCode, IndexCompletion, OperationError, Provenance,
        RootIdentity, SnapshotTarget,
    };

    use super::{JobRegistry, JobRequestSummary, JobState};

    #[test]
    fn start_returns_before_a_controlled_worker_finishes() {
        let registry = JobRegistry::new();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let completion = completion("workspace-1", "snapshot-1");

        let status = registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |_, _| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(completion)
                },
            )
            .unwrap();

        assert_eq!(status.job_id, "job-1");
        started_rx.recv().unwrap();
        assert_eq!(registry.status("job-1").unwrap().state, JobState::Queued);
        release_tx.send(()).unwrap();
        registry.close();
        assert!(matches!(
            registry.status("job-1").unwrap().state,
            JobState::Completed { .. }
        ));
    }

    #[test]
    fn work_starts_after_handle_registration_and_registry_unlock() {
        let registry = JobRegistry::new();
        let registry_from_work = registry.clone();
        let lock_was_free = Arc::new(AtomicBool::new(false));
        let observed = lock_was_free.clone();
        let (lock_tx, lock_rx) = mpsc::sync_channel(0);

        let status = registry
            .start_with_hook(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |_, _| {
                    lock_tx
                        .send(registry_from_work.state.try_lock().is_ok())
                        .unwrap();
                    Ok(completion("workspace-1", "snapshot-1"))
                },
                move || observed.store(lock_rx.recv().unwrap(), Ordering::Relaxed),
            )
            .unwrap();

        assert_eq!(status.state, JobState::Queued);
        registry.close();
        assert!(lock_was_free.load(Ordering::Relaxed));
    }

    #[test]
    fn identical_active_request_returns_the_existing_job() {
        let registry = JobRegistry::new();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let duplicate_runs = Arc::new(AtomicUsize::new(0));
        let first = registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |_, _| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(completion("workspace-1", "snapshot-1"))
                },
            )
            .unwrap();
        started_rx.recv().unwrap();

        let duplicate = registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                {
                    let duplicate_runs = duplicate_runs.clone();
                    move |_, _| {
                        duplicate_runs.fetch_add(1, Ordering::Relaxed);
                        Ok(completion("workspace-1", "snapshot-duplicate"))
                    }
                },
            )
            .unwrap();

        assert_eq!(first.job_id, "job-1");
        assert_eq!(duplicate.job_id, first.job_id);
        release_tx.send(()).unwrap();
        registry.close();
        assert_eq!(duplicate_runs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn conflicting_request_reports_workspace_busy_and_active_job() {
        let registry = JobRegistry::new();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |_, _| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(completion("workspace-1", "snapshot-1"))
                },
            )
            .unwrap();
        started_rx.recv().unwrap();

        let error = registry
            .start(
                "workspace-1".into(),
                "request-2".into(),
                request("workspace-1"),
                |_, _| Ok(completion("workspace-1", "snapshot-2")),
            )
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::WorkspaceBusy);
        assert_eq!(error.details["active_job_id"], "job-1");
        release_tx.send(()).unwrap();
        registry.close();
    }

    #[test]
    fn different_workspaces_run_concurrently() {
        let registry = JobRegistry::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_one_tx, release_one_rx) = mpsc::sync_channel(0);
        let (release_two_tx, release_two_rx) = mpsc::sync_channel(0);
        let first = registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                {
                    let started_tx = started_tx.clone();
                    move |reporter, _| {
                        reporter.report(progress(BuildStage::Capturing, 0, 0));
                        started_tx.send("workspace-1").unwrap();
                        release_one_rx.recv().unwrap();
                        Ok(completion("workspace-1", "snapshot-1"))
                    }
                },
            )
            .unwrap();
        let second = registry
            .start(
                "workspace-2".into(),
                "request-2".into(),
                request("workspace-2"),
                move |reporter, _| {
                    reporter.report(progress(BuildStage::Capturing, 0, 0));
                    started_tx.send("workspace-2").unwrap();
                    release_two_rx.recv().unwrap();
                    Ok(completion("workspace-2", "snapshot-2"))
                },
            )
            .unwrap();

        let mut started = [started_rx.recv().unwrap(), started_rx.recv().unwrap()];
        started.sort_unstable();
        assert_eq!(started, ["workspace-1", "workspace-2"]);
        assert_eq!(first.job_id, "job-1");
        assert_eq!(second.job_id, "job-2");
        assert_eq!(registry.status("job-1").unwrap().state, JobState::Capturing);
        assert_eq!(registry.status("job-2").unwrap().state, JobState::Capturing);
        release_one_tx.send(()).unwrap();
        release_two_tx.send(()).unwrap();
        registry.close();
    }

    #[test]
    fn cancel_reaches_terminal_without_changing_a_completed_job() {
        let registry = JobRegistry::new();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |_, cancelled| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    if cancelled.load(Ordering::Acquire) {
                        Err(OperationError::new(
                            ErrorCode::JobCancelled,
                            "indexing job was cancelled",
                        ))
                    } else {
                        Ok(completion("workspace-1", "snapshot-1"))
                    }
                },
            )
            .unwrap();
        started_rx.recv().unwrap();

        assert_eq!(registry.cancel("job-1").unwrap().state, JobState::Queued);
        release_tx.send(()).unwrap();
        registry.close();
        assert_eq!(registry.status("job-1").unwrap().state, JobState::Cancelled);
        assert_eq!(registry.cancel("job-1").unwrap().state, JobState::Cancelled);

        let (completed_started_tx, completed_started_rx) = mpsc::sync_channel(0);
        let (completed_release_tx, completed_release_rx) = mpsc::sync_channel(0);
        let completed = registry
            .start(
                "workspace-1".into(),
                "request-2".into(),
                request("workspace-1"),
                move |_, _| {
                    completed_started_tx.send(()).unwrap();
                    completed_release_rx.recv().unwrap();
                    Ok(completion("workspace-1", "snapshot-2"))
                },
            )
            .unwrap();
        assert_eq!(completed.job_id, "job-2");
        completed_started_rx.recv().unwrap();
        assert_eq!(registry.cancel("job-2").unwrap().state, JobState::Queued);
        completed_release_tx.send(()).unwrap();
        registry.close();
        let before = registry.status("job-2").unwrap();
        assert!(matches!(before.state, JobState::Completed { .. }));
        assert_eq!(registry.cancel("job-2").unwrap(), before);

        let failed = registry
            .start(
                "workspace-1".into(),
                "request-3".into(),
                request("workspace-1"),
                |_, _| {
                    Err(OperationError::new(
                        ErrorCode::CaptureChanged,
                        "source changed during capture",
                    ))
                },
            )
            .unwrap();
        assert_eq!(failed.job_id, "job-3");
        registry.close();
        let before = registry.status("job-3").unwrap();
        assert!(matches!(
            &before.state,
            JobState::Failed {
                error: OperationError {
                    code: ErrorCode::CaptureChanged,
                    ..
                }
            }
        ));
        assert_eq!(registry.cancel("job-3").unwrap(), before);
    }

    #[test]
    fn panicking_worker_fails_and_releases_workspace() {
        let registry = JobRegistry::new();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |_, _| {
                    started_tx.send(()).unwrap();
                    panic!("intentional worker panic");
                },
            )
            .unwrap();
        started_rx.recv().unwrap();
        registry.close();

        assert!(matches!(
            registry.status("job-1").unwrap().state,
            JobState::Failed {
                error: OperationError {
                    code: ErrorCode::Internal,
                    ref message,
                    ..
                }
            } if message == "indexing worker panicked"
        ));
        let replacement = registry
            .start(
                "workspace-1".into(),
                "request-2".into(),
                request("workspace-1"),
                |_, _| Ok(completion("workspace-1", "snapshot-2")),
            )
            .unwrap();
        assert_eq!(replacement.job_id, "job-2");
        registry.close();
    }

    #[test]
    fn reporter_accepts_only_forward_progress() {
        let registry = JobRegistry::new();
        let (progress_tx, progress_rx) = mpsc::channel();
        let (reported_tx, reported_rx) = mpsc::sync_channel(0);
        registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |reporter, _| {
                    while let Some(progress) = progress_rx.recv().unwrap() {
                        reporter.report(progress);
                        reported_tx.send(()).unwrap();
                    }
                    Ok(completion("workspace-1", "snapshot-1"))
                },
            )
            .unwrap();

        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::Capturing, 0, 0),
        );
        let mut selecting = progress(BuildStage::SelectingSeed, 0, 3);
        selecting.rejected_cache = Some("rejected.db".into());
        report_and_wait(&progress_tx, &reported_rx, selecting);
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::Indexing, 1, 3),
        );
        assert_eq!(
            registry.status("job-1").unwrap().state,
            JobState::Indexing {
                files_done: 1,
                files_total: 3,
                files_reused: 0,
                files_parsed: 1,
            }
        );
        assert_eq!(
            registry.status("job-1").unwrap().rejected_cache.as_deref(),
            Some("rejected.db")
        );

        let mut backwards = progress(BuildStage::SelectingSeed, 0, 3);
        backwards.rejected_cache = Some("wrong.db".into());
        report_and_wait(&progress_tx, &reported_rx, backwards);
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::Indexing, 0, 3),
        );
        assert_eq!(
            registry.status("job-1").unwrap().state,
            JobState::Indexing {
                files_done: 1,
                files_total: 3,
                files_reused: 0,
                files_parsed: 1,
            }
        );
        assert_eq!(
            registry.status("job-1").unwrap().rejected_cache.as_deref(),
            Some("rejected.db")
        );

        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::Indexing, 3, 3),
        );
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::ResolvingGraph, 3, 3),
        );
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::Publishing, 3, 3),
        );
        assert_eq!(
            registry.status("job-1").unwrap().state,
            JobState::Publishing
        );
        progress_tx.send(None).unwrap();
        registry.close();
    }

    #[test]
    fn reporter_rejects_cross_stage_counter_regression() {
        let registry = JobRegistry::new();
        let (progress_tx, progress_rx) = mpsc::channel();
        let (reported_tx, reported_rx) = mpsc::sync_channel(0);
        registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                move |reporter, _| {
                    while let Some(progress) = progress_rx.recv().unwrap() {
                        reporter.report(progress);
                        reported_tx.send(()).unwrap();
                    }
                    Ok(completion("workspace-1", "snapshot-1"))
                },
            )
            .unwrap();

        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::Capturing, 0, 0),
        );
        report_and_wait(
            &progress_tx,
            &reported_rx,
            BuildProgress {
                stage: BuildStage::Capturing,
                files_done: 3,
                files_total: 3,
                files_reused: 0,
                files_parsed: 0,
                rejected_cache: None,
            },
        );
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::SelectingSeed, 0, 3),
        );
        assert_eq!(
            registry.status("job-1").unwrap().state,
            JobState::SelectingSeed
        );
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::Indexing, 2, 3),
        );
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::ResolvingGraph, 1, 3),
        );
        assert_eq!(
            registry.status("job-1").unwrap().state,
            JobState::Indexing {
                files_done: 2,
                files_total: 3,
                files_reused: 0,
                files_parsed: 2,
            }
        );
        report_and_wait(
            &progress_tx,
            &reported_rx,
            progress(BuildStage::ResolvingGraph, 2, 3),
        );
        assert_eq!(
            registry.status("job-1").unwrap().state,
            JobState::ResolvingGraph
        );
        progress_tx.send(None).unwrap();
        registry.close();
    }

    #[test]
    fn close_cancels_every_active_job() {
        let registry = JobRegistry::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_one_tx, release_one_rx) = mpsc::sync_channel(0);
        let (release_two_tx, release_two_rx) = mpsc::sync_channel(0);
        registry
            .start(
                "workspace-1".into(),
                "request-1".into(),
                request("workspace-1"),
                {
                    let started_tx = started_tx.clone();
                    move |_, cancelled| {
                        started_tx.send(()).unwrap();
                        cancelled_worker(cancelled, release_one_rx, "workspace-1", "snapshot-1")
                    }
                },
            )
            .unwrap();
        registry
            .start(
                "workspace-2".into(),
                "request-2".into(),
                request("workspace-2"),
                move |_, cancelled| {
                    started_tx.send(()).unwrap();
                    cancelled_worker(cancelled, release_two_rx, "workspace-2", "snapshot-2")
                },
            )
            .unwrap();
        started_rx.recv().unwrap();
        started_rx.recv().unwrap();

        registry.close_after_cancel(|| {
            release_one_tx.send(()).unwrap();
            release_two_tx.send(()).unwrap();
        });

        assert_eq!(registry.status("job-1").unwrap().state, JobState::Cancelled);
        assert_eq!(registry.status("job-2").unwrap().state, JobState::Cancelled);
    }

    fn request(workspace_id: &str) -> JobRequestSummary {
        JobRequestSummary {
            root: root(workspace_id),
            base_ref: "HEAD~1".into(),
            base_oid: "a".repeat(40),
            head_ref: "HEAD".into(),
            head_oid: "b".repeat(40),
            target: SnapshotTarget::Commit,
            dependency_mode: DependencyMode::Boundary,
        }
    }

    fn progress(stage: BuildStage, files_done: usize, files_total: usize) -> BuildProgress {
        BuildProgress {
            stage,
            files_done,
            files_total,
            files_reused: 0,
            files_parsed: files_done,
            rejected_cache: None,
        }
    }

    fn report_and_wait(
        progress_tx: &mpsc::Sender<Option<BuildProgress>>,
        reported_rx: &mpsc::Receiver<()>,
        progress: BuildProgress,
    ) {
        progress_tx.send(Some(progress)).unwrap();
        reported_rx.recv().unwrap();
    }

    fn cancelled_worker(
        cancelled: Arc<AtomicBool>,
        release_rx: mpsc::Receiver<()>,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> Result<IndexCompletion, OperationError> {
        release_rx.recv().unwrap();
        if cancelled.load(Ordering::Acquire) {
            Err(OperationError::new(
                ErrorCode::JobCancelled,
                "indexing job was cancelled",
            ))
        } else {
            Ok(completion(workspace_id, snapshot_id))
        }
    }

    fn root(workspace_id: &str) -> RootIdentity {
        RootIdentity {
            repository_id: "repository-1".into(),
            workspace_id: workspace_id.into(),
            repository_root: PathBuf::from("/repository"),
            worktree_root: PathBuf::from("/worktree"),
            git_dir: PathBuf::from("/repository/.git"),
            common_git_dir: PathBuf::from("/repository/.git"),
            index_path: PathBuf::from("/repository/.git/index"),
            object_format: "sha1".into(),
            branch: Some("main".into()),
            head_oid: "b".repeat(40),
        }
    }

    fn completion(workspace_id: &str, snapshot_id: &str) -> IndexCompletion {
        let root = root(workspace_id);
        IndexCompletion {
            snapshot_id: snapshot_id.into(),
            graph_image_id: format!("graph-{snapshot_id}"),
            provenance: Provenance {
                repository_id: root.repository_id,
                workspace_id: root.workspace_id,
                snapshot_id: snapshot_id.into(),
                common_git_dir: root.common_git_dir,
                git_dir: root.git_dir,
                repository_root: root.repository_root,
                worktree_root: root.worktree_root,
                branch: root.branch,
                base_ref: "HEAD~1".into(),
                base_oid: "a".repeat(40),
                head_ref: "HEAD".into(),
                head_oid: "b".repeat(40),
                target_state: SnapshotTarget::Commit,
                selected_layers: Vec::new(),
                dirty_digest: String::new(),
                commits_base_to_head: 1,
                changed_files: 1,
                index_generation: 1,
            },
            stats: IndexStats {
                files_total: 1,
                files_reused: 0,
                files_parsed: 1,
                files_skipped: 0,
            },
        }
    }
}
