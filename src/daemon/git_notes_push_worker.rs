use crate::config::{Config, NotesBackendKind};
use crate::error::GitAiError;
use crate::git::find_repository_in_path;
use crate::git::sync_authorship::push_authorship_notes_with_local_lock;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

const INGRESS_CAPACITY: usize = 128;
const COMPLETION_CAPACITY: usize = 128;
const MAX_PENDING_DESTINATIONS_PER_FAMILY: usize = 8;

#[derive(Debug, Clone)]
pub struct GitNotesPushJob {
    pub family: String,
    pub worktree: String,
    pub destination: String,
    pub local_ref_lock: Arc<tokio::sync::Mutex<()>>,
}

impl GitNotesPushJob {
    fn sanitized_destination(&self) -> String {
        if let Ok(normalized) = crate::repo_url::normalize_repo_url(&self.destination) {
            return normalized;
        }
        if let Ok(mut url) = url::Url::parse(&self.destination) {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            return url.to_string();
        }
        if let Some((helper, _)) = self.destination.split_once("::") {
            return format!("{}::[redacted]", helper);
        }
        self.destination.clone()
    }
}

enum WorkerRequest {
    Enqueue(GitNotesPushJob),
    Drain(oneshot::Sender<()>),
}

struct Completion {
    job: GitNotesPushJob,
    started_at: Instant,
    result: Result<(), GitAiError>,
}

#[derive(Default)]
struct FamilyQueue {
    running: bool,
    pending: VecDeque<GitNotesPushJob>,
}

type PushExecutor = Arc<dyn Fn(&GitNotesPushJob) -> Result<(), GitAiError> + Send + Sync>;

#[derive(Clone)]
pub struct GitNotesPushWorkerHandle {
    request_tx: mpsc::Sender<WorkerRequest>,
}

impl GitNotesPushWorkerHandle {
    pub fn enqueue(&self, job: GitNotesPushJob) {
        let family = job.family.clone();
        let destination = job.sanitized_destination();
        match self.request_tx.try_send(WorkerRequest::Enqueue(job)) {
            Ok(()) => tracing::info!(
                component = "git_notes_push_worker",
                phase = "enqueue",
                %family,
                %destination,
                "queued asynchronous Git notes push"
            ),
            Err(error) => tracing::warn!(
                component = "git_notes_push_worker",
                phase = "queue_overflow",
                reason = %error,
                %family,
                %destination,
                "dropping Git notes push because the bounded ingress queue is full"
            ),
        }
    }

    pub async fn drain(&self) -> Result<(), String> {
        let (completion, receiver) = oneshot::channel();
        self.request_tx
            .send(WorkerRequest::Drain(completion))
            .await
            .map_err(|_| "Git notes push worker has stopped".to_string())?;
        receiver
            .await
            .map_err(|_| "Git notes push worker drain was cancelled".to_string())
    }
}

pub fn spawn_git_notes_push_worker() -> GitNotesPushWorkerHandle {
    spawn_with_executor(Arc::new(execute_push))
}

fn spawn_with_executor(executor: PushExecutor) -> GitNotesPushWorkerHandle {
    let (request_tx, request_rx) = mpsc::channel(INGRESS_CAPACITY);
    let (completion_tx, completion_rx) = mpsc::channel(COMPLETION_CAPACITY);
    tokio::spawn(run_worker(
        request_rx,
        completion_rx,
        completion_tx,
        executor,
    ));
    GitNotesPushWorkerHandle { request_tx }
}

async fn run_worker(
    mut request_rx: mpsc::Receiver<WorkerRequest>,
    mut completion_rx: mpsc::Receiver<Completion>,
    completion_tx: mpsc::Sender<Completion>,
    executor: PushExecutor,
) {
    let mut families: HashMap<String, FamilyQueue> = HashMap::new();
    let mut drain_waiters = Vec::new();

    loop {
        tokio::select! {
            request = request_rx.recv() => {
                let Some(request) = request else { break };
                match request {
                    WorkerRequest::Enqueue(job) => {
                        enqueue_job(&mut families, job, &completion_tx, &executor);
                    }
                    WorkerRequest::Drain(waiter) => drain_waiters.push(waiter),
                }
            }
            completion = completion_rx.recv() => {
                let Some(completion) = completion else { break };
                finish_job(&mut families, completion, &completion_tx, &executor);
            }
        }

        if families.is_empty() && request_rx.is_empty() {
            for waiter in drain_waiters.drain(..) {
                let _ = waiter.send(());
            }
        }
    }
}

fn enqueue_job(
    families: &mut HashMap<String, FamilyQueue>,
    job: GitNotesPushJob,
    completion_tx: &mpsc::Sender<Completion>,
    executor: &PushExecutor,
) {
    let family = job.family.clone();
    let queue = families.entry(family.clone()).or_default();
    if !queue.running {
        queue.running = true;
        start_job(job, completion_tx.clone(), executor.clone());
        return;
    }

    if let Some(existing) = queue
        .pending
        .iter_mut()
        .find(|pending| pending.destination == job.destination)
    {
        *existing = job;
        tracing::debug!(
            component = "git_notes_push_worker",
            phase = "coalesce",
            %family,
            "coalesced repeated Git notes push"
        );
        return;
    }

    if queue.pending.len() >= MAX_PENDING_DESTINATIONS_PER_FAMILY {
        tracing::warn!(
            component = "git_notes_push_worker",
            phase = "queue_overflow",
            %family,
            max_pending = MAX_PENDING_DESTINATIONS_PER_FAMILY,
            "dropping Git notes push because the family queue is full"
        );
        return;
    }
    queue.pending.push_back(job);
}

fn start_job(
    job: GitNotesPushJob,
    completion_tx: mpsc::Sender<Completion>,
    executor: PushExecutor,
) {
    let family = job.family.clone();
    let destination = job.sanitized_destination();
    tracing::info!(
        component = "git_notes_push_worker",
        phase = "start",
        %family,
        %destination,
        "starting asynchronous Git notes push"
    );
    tokio::task::spawn_blocking(move || {
        let started_at = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| executor(&job)))
            .unwrap_or_else(|_| {
                Err(GitAiError::Generic(
                    "Git notes push executor panicked".to_string(),
                ))
            });
        let _ = completion_tx.blocking_send(Completion {
            job,
            started_at,
            result,
        });
    });
}

fn finish_job(
    families: &mut HashMap<String, FamilyQueue>,
    completion: Completion,
    completion_tx: &mpsc::Sender<Completion>,
    executor: &PushExecutor,
) {
    let family = completion.job.family.clone();
    let destination = completion.job.sanitized_destination();
    let duration_ms = completion.started_at.elapsed().as_millis() as u64;
    match completion.result {
        Ok(()) => tracing::info!(
            component = "git_notes_push_worker",
            phase = "success",
            %family,
            %destination,
            duration_ms,
            "asynchronous Git notes push completed"
        ),
        Err(error) => {
            let (phase, reason) = sanitized_failure(&error);
            tracing::error!(
                component = "git_notes_push_worker",
                phase,
                %reason,
                %family,
                %destination,
                duration_ms,
                "asynchronous Git notes push failed"
            )
        }
    }

    let Some(queue) = families.get_mut(&family) else {
        return;
    };
    if let Some(next) = queue.pending.pop_front() {
        start_job(next, completion_tx.clone(), executor.clone());
    } else {
        families.remove(&family);
    }
}

fn sanitized_failure(error: &GitAiError) -> (&'static str, String) {
    let timed_out = matches!(error, GitAiError::IoError(error) if error.kind() == std::io::ErrorKind::TimedOut)
        || matches!(error, GitAiError::Generic(message) if message.contains("timed out"))
        || matches!(error, GitAiError::GitCliError { stderr, .. } if stderr.contains("timed out"));
    if timed_out {
        return ("timeout", "Git notes transport timed out".to_string());
    }

    match error {
        GitAiError::GitCliError {
            code: Some(code), ..
        } => ("failure", format!("Git exited with status {code}")),
        GitAiError::GitCliError { code: None, .. } => (
            "failure",
            "Git terminated without an exit status".to_string(),
        ),
        _ => ("failure", "internal Git notes push failure".to_string()),
    }
}

fn execute_push(job: &GitNotesPushJob) -> Result<(), GitAiError> {
    if Config::fresh_notes_backend_kind_cached() == NotesBackendKind::Http {
        tracing::info!(
            component = "git_notes_push_worker",
            phase = "backend_skip",
            family = %job.family,
            "skipping Git notes push because HTTP notes are enabled"
        );
        return Ok(());
    }
    let repository = find_repository_in_path(&job.worktree)?;
    push_authorship_notes_with_local_lock(&repository, &job.destination, &job.local_ref_lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn job(family: &str, destination: &str) -> GitNotesPushJob {
        GitNotesPushJob {
            family: family.into(),
            worktree: "/repo".into(),
            destination: destination.into(),
            local_ref_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    #[tokio::test]
    async fn serializes_pushes_within_a_family_and_drain_waits() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let executor: PushExecutor = {
            let active = active.clone();
            let peak = peak.clone();
            let completed = completed.clone();
            Arc::new(move |_| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(25));
                active.fetch_sub(1, Ordering::SeqCst);
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        let worker = spawn_with_executor(executor);
        for destination in ["one", "two", "three"] {
            worker.enqueue(job("family", destination));
        }

        worker.drain().await.unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 3);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn coalesces_repeated_pending_destinations() {
        let completed = Arc::new(AtomicUsize::new(0));
        let executor: PushExecutor = {
            let completed = completed.clone();
            Arc::new(move |_| {
                std::thread::sleep(Duration::from_millis(20));
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        let worker = spawn_with_executor(executor);
        for _ in 0..10 {
            worker.enqueue(job("family", "same"));
        }

        worker.drain().await.unwrap();
        assert!(completed.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn unrelated_families_can_run_concurrently() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let executor: PushExecutor = {
            let active = active.clone();
            let peak = peak.clone();
            Arc::new(move |_| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
        };
        let worker = spawn_with_executor(executor);
        for family in ["one", "two"] {
            worker.enqueue(job(family, family));
        }

        worker.drain().await.unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn pending_destinations_are_bounded_per_family() {
        let (completion_tx, _completion_rx) = mpsc::channel(COMPLETION_CAPACITY);
        let executor: PushExecutor = Arc::new(|_| Ok(()));
        let mut families = HashMap::from([(
            "family".to_string(),
            FamilyQueue {
                running: true,
                pending: VecDeque::new(),
            },
        )]);

        for index in 0..20 {
            enqueue_job(
                &mut families,
                job("family", &format!("destination-{index}")),
                &completion_tx,
                &executor,
            );
        }

        assert_eq!(
            families.get("family").unwrap().pending.len(),
            MAX_PENDING_DESTINATIONS_PER_FAMILY
        );
    }

    #[tokio::test]
    async fn executor_panic_completes_the_job_and_starts_the_next_one() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let executor: PushExecutor = {
            let attempts = attempts.clone();
            Arc::new(move |_| {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("simulated push panic");
                }
                Ok(())
            })
        };
        let worker = spawn_with_executor(executor);
        worker.enqueue(job("family", "one"));
        worker.enqueue(job("family", "two"));

        worker.drain().await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn git_failure_summary_does_not_include_credentials_or_command_arguments() {
        let error = GitAiError::GitCliError {
            code: Some(128),
            stderr: "authentication failed for https://token@example.com/repo.git".into(),
            args: vec!["push".into(), "https://token@example.com/repo.git".into()],
        };

        let (phase, reason) = sanitized_failure(&error);
        assert_eq!(phase, "failure");
        assert_eq!(reason, "Git exited with status 128");
        assert!(!reason.contains("token"));
    }
}
