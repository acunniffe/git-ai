use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FamilyDrainSnapshot {
    pub queued: bool,
    pub dirty: bool,
    pub in_flight: bool,
    pub last_error: Option<String>,
}

impl FamilyDrainSnapshot {
    pub fn is_idle(&self) -> bool {
        !self.queued && !self.dirty && !self.in_flight
    }
}

#[derive(Default)]
struct FamilyDrainState {
    queued: bool,
    dirty: bool,
    in_flight: bool,
    last_error: Option<String>,
}

struct FamilyDrain {
    state: Mutex<FamilyDrainState>,
    execution_lock: Arc<AsyncMutex<()>>,
    completion: Notify,
}

impl FamilyDrain {
    fn new() -> Self {
        Self {
            state: Mutex::new(FamilyDrainState::default()),
            execution_lock: Arc::new(AsyncMutex::new(())),
            completion: Notify::new(),
        }
    }

    fn snapshot(&self) -> FamilyDrainSnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        FamilyDrainSnapshot {
            queued: state.queued,
            dirty: state.dirty,
            in_flight: state.in_flight,
            last_error: state.last_error.clone(),
        }
    }
}

pub(crate) struct FamilyDrainScheduler {
    families: Mutex<HashMap<String, Arc<FamilyDrain>>>,
    permits: Arc<Semaphore>,
}

impl FamilyDrainScheduler {
    pub fn new(max_concurrent_drains: usize) -> Self {
        Self {
            families: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(max_concurrent_drains)),
        }
    }

    fn family(&self, family: &str) -> Arc<FamilyDrain> {
        let mut families = self
            .families
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        families
            .entry(family.to_string())
            .or_insert_with(|| Arc::new(FamilyDrain::new()))
            .clone()
    }

    /// Marks a family as needing a drain and returns whether a runner must be spawned.
    pub fn schedule(&self, family: &str) -> bool {
        let family = self.family(family);
        let mut state = family
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.in_flight {
            state.dirty = true;
            return false;
        }
        if state.queued {
            return false;
        }
        state.queued = true;
        true
    }

    pub async fn run<F, Fut, E>(&self, family_key: &str, mut drain: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
        E: Display,
    {
        let family = self.family(family_key);
        {
            let mut state = family
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.in_flight || !state.queued {
                return;
            }
            state.queued = false;
            state.in_flight = true;
        }

        loop {
            let permit = match self.permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(error) => {
                    self.finish_pass(&family, Err(error.to_string()));
                    return;
                }
            };
            let execution_lock = family.execution_lock.clone();
            let _execution_guard = execution_lock.lock_owned().await;
            let result = drain().await.map_err(|error| error.to_string());
            drop(_execution_guard);
            drop(permit);

            if !self.finish_pass(&family, result) {
                return;
            }
        }
    }

    /// Finishes one pass. Returns true when work arrived during that pass.
    fn finish_pass(&self, family: &FamilyDrain, result: Result<(), String>) -> bool {
        let mut state = family
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Err(error) = result {
            state.last_error = Some(error);
        }
        if state.dirty {
            state.dirty = false;
            state.queued = true;
            return true;
        }

        state.queued = false;
        state.in_flight = false;
        family.completion.notify_waiters();
        false
    }

    pub fn snapshot(&self, family: &str) -> Option<FamilyDrainSnapshot> {
        let family = {
            let families = self
                .families
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            families.get(family).cloned()
        }?;
        Some(family.snapshot())
    }

    pub async fn wait_idle(&self, family_key: &str) {
        let family = self.family(family_key);
        loop {
            let notified = family.completion.notified();
            if family.snapshot().is_idle() {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub fn execution_lock(&self, family: &str) -> Arc<AsyncMutex<()>> {
        self.family(family).execution_lock.clone()
    }

    pub fn gc_idle_families(&self) {
        let mut families = self
            .families
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        families.retain(|_, family| {
            !family.snapshot().is_idle()
                || Arc::strong_count(family) > 1
                || Arc::strong_count(&family.execution_lock) > 1
        });
    }

    #[cfg(test)]
    fn family_count(&self) -> usize {
        self.families
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn single_flight_runs_a_dirty_second_pass() {
        let scheduler = Arc::new(FamilyDrainScheduler::new(4));
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let passes = Arc::new(AtomicUsize::new(0));

        assert!(scheduler.schedule("family"));
        let task = {
            let scheduler = scheduler.clone();
            let first_started = first_started.clone();
            let release_first = release_first.clone();
            let passes = passes.clone();
            tokio::spawn(async move {
                scheduler
                    .run("family", move || {
                        let first_started = first_started.clone();
                        let release_first = release_first.clone();
                        let pass = passes.fetch_add(1, Ordering::SeqCst);
                        async move {
                            if pass == 0 {
                                first_started.notify_one();
                                release_first.notified().await;
                            }
                            Ok::<_, String>(())
                        }
                    })
                    .await;
            })
        };

        first_started.notified().await;
        assert!(!scheduler.schedule("family"));
        assert!(scheduler.snapshot("family").unwrap().dirty);
        release_first.notify_one();
        task.await.unwrap();

        assert_eq!(passes.load(Ordering::SeqCst), 2);
        assert!(scheduler.snapshot("family").unwrap().is_idle());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn limits_global_concurrent_drains_to_four() {
        let scheduler = Arc::new(FamilyDrainScheduler::new(4));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for family in 0..12 {
            let family = format!("family-{family}");
            assert!(scheduler.schedule(&family));
            let scheduler = scheduler.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            tasks.push(tokio::spawn(async move {
                scheduler
                    .run(&family, move || {
                        let active = active.clone();
                        let maximum = maximum.clone();
                        async move {
                            let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
                            maximum.fetch_max(concurrent, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok::<_, String>(())
                        }
                    })
                    .await;
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serializes_one_family_while_another_family_progresses() {
        let scheduler = Arc::new(FamilyDrainScheduler::new(4));
        let same_family_active = Arc::new(AtomicUsize::new(0));
        let same_family_max = Arc::new(AtomicUsize::new(0));
        let family_a_passes = Arc::new(AtomicUsize::new(0));
        let family_a_started = Arc::new(Notify::new());
        let release_family_a = Arc::new(Notify::new());
        let family_b_finished = Arc::new(Notify::new());

        assert!(scheduler.schedule("family-a"));
        let family_a_task = {
            let scheduler = scheduler.clone();
            let same_family_active = same_family_active.clone();
            let same_family_max = same_family_max.clone();
            let family_a_passes = family_a_passes.clone();
            let family_a_started = family_a_started.clone();
            let release_family_a = release_family_a.clone();
            tokio::spawn(async move {
                scheduler
                    .run("family-a", move || {
                        let same_family_active = same_family_active.clone();
                        let same_family_max = same_family_max.clone();
                        let pass = family_a_passes.fetch_add(1, Ordering::SeqCst);
                        let family_a_started = family_a_started.clone();
                        let release_family_a = release_family_a.clone();
                        async move {
                            let concurrent = same_family_active.fetch_add(1, Ordering::SeqCst) + 1;
                            same_family_max.fetch_max(concurrent, Ordering::SeqCst);
                            if pass == 0 {
                                family_a_started.notify_one();
                                release_family_a.notified().await;
                            }
                            same_family_active.fetch_sub(1, Ordering::SeqCst);
                            Ok::<_, String>(())
                        }
                    })
                    .await;
            })
        };

        family_a_started.notified().await;
        assert!(!scheduler.schedule("family-a"));
        assert!(scheduler.schedule("family-b"));
        let family_b_task = {
            let scheduler = scheduler.clone();
            let family_b_finished = family_b_finished.clone();
            tokio::spawn(async move {
                scheduler
                    .run("family-b", move || {
                        let family_b_finished = family_b_finished.clone();
                        async move {
                            family_b_finished.notify_one();
                            Ok::<_, String>(())
                        }
                    })
                    .await;
            })
        };

        timeout(Duration::from_millis(250), family_b_finished.notified())
            .await
            .expect("unrelated family should progress");
        release_family_a.notify_waiters();
        family_a_task.await.unwrap();
        family_b_task.await.unwrap();
        assert_eq!(same_family_max.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cleanup_cannot_create_overlapping_drains_for_an_active_family() {
        let scheduler = Arc::new(FamilyDrainScheduler::new(4));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let passes = Arc::new(AtomicUsize::new(0));

        assert!(scheduler.schedule("family"));
        let task = {
            let scheduler = scheduler.clone();
            let started = started.clone();
            let release = release.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            let passes = passes.clone();
            tokio::spawn(async move {
                scheduler
                    .run("family", move || {
                        let started = started.clone();
                        let release = release.clone();
                        let active = active.clone();
                        let maximum = maximum.clone();
                        let pass = passes.fetch_add(1, Ordering::SeqCst);
                        async move {
                            let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
                            maximum.fetch_max(concurrent, Ordering::SeqCst);
                            if pass == 0 {
                                started.notify_one();
                                release.notified().await;
                            }
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok::<_, String>(())
                        }
                    })
                    .await;
            })
        };

        started.notified().await;
        scheduler.gc_idle_families();
        assert!(!scheduler.schedule("family"));
        release.notify_one();
        task.await.unwrap();

        assert_eq!(passes.load(Ordering::SeqCst), 2);
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanup_removes_only_idle_unreferenced_families() {
        let scheduler = FamilyDrainScheduler::new(4);
        let retained_lock = scheduler.execution_lock("retained");
        scheduler.execution_lock("idle");

        scheduler.gc_idle_families();
        assert_eq!(scheduler.family_count(), 1);
        assert!(Arc::ptr_eq(
            &retained_lock,
            &scheduler.execution_lock("retained")
        ));
    }

    #[tokio::test]
    async fn records_the_last_error_and_notifies_completion() {
        let scheduler = Arc::new(FamilyDrainScheduler::new(4));
        assert!(scheduler.schedule("family"));
        let waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move {
                scheduler.wait_idle("family").await;
            })
        };

        scheduler
            .run("family", || async { Err::<(), _>("failed drain") })
            .await;
        waiter.await.unwrap();

        let snapshot = scheduler.snapshot("family").unwrap();
        assert!(snapshot.is_idle());
        assert_eq!(snapshot.last_error.as_deref(), Some("failed drain"));
    }
}
