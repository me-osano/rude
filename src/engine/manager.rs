use crate::engine::{
    config::EngineConfig,
    scheduler::{Scheduler, SchedulerEvent},
    session::Session,
    task::{DownloadProgress, DownloadState, DownloadTask, TaskId, TaskOptions},
};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::interval;
use url::Url;

/// Progress broadcast — keyed by TaskId. Consumers call `.subscribe()`.
pub type ProgressMap = Arc<DashMap<TaskId, watch::Sender<Option<DownloadProgress>>>>;

/// The central manager. Clone-safe (all state behind Arc).
#[derive(Clone)]
pub struct DownloadManager {
    config: EngineConfig,
    client: Client,
    tasks: Arc<DashMap<TaskId, DownloadTask>>,
    progress: ProgressMap,
    /// Limits concurrent active downloads
    semaphore: Arc<Semaphore>,
}

impl DownloadManager {
    /// Create a new manager. Call `restore_session()` before using in production.
    pub fn new(config: EngineConfig) -> Self {
        let client = Client::builder()
            .tcp_keepalive(Duration::from_secs(30))
            .connection_verbose(false)
            .build()
            .expect("Failed to build HTTP client");

        let sem = Arc::new(Semaphore::new(config.max_concurrent_downloads));
        Self {
            config,
            client,
            tasks: Arc::new(DashMap::new()),
            progress: Arc::new(DashMap::new()),
            semaphore: sem,
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Queue a new download. Returns the TaskId.
    pub fn add(&self, urls: Vec<Url>, options: TaskOptions) -> Result<TaskId> {
        if urls.is_empty() {
            return Err(anyhow!("At least one URL required"));
        }
        let task = DownloadTask::new(urls, options);
        let id = task.id.clone();
        tracing::info!("Queued download: {} -> {}", id, task.primary_url());
        self.tasks.insert(id.clone(), task);
        self.spawn_task(id.clone());
        Ok(id)
    }

    /// Pause an active task.
    pub fn pause(&self, id: &TaskId) -> Result<()> {
        let mut task = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("Task not found: {}", id))?;
        if task.state.is_active() {
            task.state = DownloadState::Paused;
            tracing::info!("Paused {}", id);
        }
        Ok(())
    }

    /// Resume a paused task.
    pub fn resume(&self, id: &TaskId) -> Result<()> {
        let mut task = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("Task not found: {}", id))?;
        if matches!(task.state, DownloadState::Paused) {
            task.state = DownloadState::Queued;
            drop(task);
            self.spawn_task(id.clone());
        }
        Ok(())
    }

    /// Cancel and optionally delete the partial file.
    pub fn cancel(&self, id: &TaskId, remove_file: bool) -> Result<()> {
        if let Some((_, task)) = self.tasks.remove(id) {
            if remove_file {
                if let Some(path) = task.output_path {
                    let _ = std::fs::remove_file(path);
                }
            }
            self.progress.remove(id);
            tracing::info!("Cancelled {}", id);
        }
        Ok(())
    }

    /// List all tasks (snapshot).
    pub fn list(&self) -> Vec<DownloadTask> {
        self.tasks.iter().map(|e| e.value().clone()).collect()
    }

    /// Get a single task by ID.
    pub fn get(&self, id: &TaskId) -> Option<DownloadTask> {
        self.tasks.get(id).map(|e| e.value().clone())
    }

    /// Subscribe to live progress for a task.
    /// Returns None if task doesn't exist or has no progress channel yet.
    pub fn subscribe(&self, id: &TaskId) -> Option<watch::Receiver<Option<DownloadProgress>>> {
        self.progress.get(id).map(|tx| tx.subscribe())
    }

    /// Restore previously saved session (call on startup).
    pub async fn restore_session(&self) -> Result<()> {
        if let Some(path) = &self.config.session_file {
            let session = Session::load(path).await?;
            for task in session.tasks {
                let id = task.id.clone();
                let was_active = task.state.is_active();
                self.tasks.insert(id.clone(), task);
                if was_active {
                    // Re-queue interrupted downloads
                    self.tasks.entry(id.clone()).and_modify(|t| {
                        t.state = DownloadState::Queued;
                    });
                    self.spawn_task(id);
                }
            }
        }
        Ok(())
    }

    /// Persist current state to session file.
    pub async fn save_session(&self) -> Result<()> {
        if let Some(path) = &self.config.session_file {
            let mut session = Session::new();
            for entry in self.tasks.iter() {
                session.upsert(entry.value().clone());
            }
            session.save(path).await?;
        }
        Ok(())
    }

    /// Start the periodic session saver. Call once at startup.
    pub fn start_session_saver(&self) {
        let interval_secs = self.config.save_session_interval;
        if interval_secs == 0 {
            return;
        }
        let mgr = self.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(interval_secs));
            loop {
                ticker.tick().await;
                if let Err(e) = mgr.save_session().await {
                    tracing::warn!("Session save failed: {}", e);
                }
            }
        });
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    fn spawn_task(&self, id: TaskId) {
        let mgr = self.clone();
        tokio::spawn(async move {
            // Acquire concurrency slot
            let _permit = mgr.semaphore.acquire().await.unwrap();

            let task = match mgr.tasks.get(&id).map(|e| e.value().clone()) {
                Some(t) => t,
                None => return,
            };

            // Mark active
            if let Some(mut t) = mgr.tasks.get_mut(&id) {
                t.state = DownloadState::Probing;
                t.started_at = Some(chrono::Utc::now());
            }

            // Progress channel
            let (prog_tx, _) = watch::channel(None);
            mgr.progress.insert(id.clone(), prog_tx.clone());

            let (event_tx, mut event_rx) = mpsc::channel::<SchedulerEvent>(64);

            // Spawn scheduler
            let sched = Scheduler::new(task, mgr.config.clone(), mgr.client.clone(), event_tx);
            let sched_handle: JoinHandle<Result<()>> = tokio::spawn(sched.run());

            // Forward events → task state + progress watch
            while let Some(event) = event_rx.recv().await {
                match event {
                    SchedulerEvent::StateChange(state) => {
                        if let Some(mut t) = mgr.tasks.get_mut(&id) {
                            t.state = state;
                        }
                    }
                    SchedulerEvent::Progress { downloaded, speed_bps, connections } => {
                        if let Some(t) = mgr.tasks.get(&id) {
                            let progress = DownloadProgress {
                                task_id: id.clone(),
                                state: t.state.clone(),
                                total_bytes: t.total_bytes,
                                downloaded_bytes: downloaded,
                                speed_bps,
                                eta_secs: crate::engine::speed::eta(
                                    t.total_bytes, downloaded, speed_bps
                                ),
                                connections,
                                chunk_progress: vec![],
                            };
                            let _ = prog_tx.send(Some(progress));
                        }
                    }
                    SchedulerEvent::Complete { output_path } => {
                        if let Some(mut t) = mgr.tasks.get_mut(&id) {
                            t.state = DownloadState::Complete;
                            t.output_path = Some(output_path);
                            t.completed_at = Some(chrono::Utc::now());
                        }
                        tracing::info!("Download complete: {}", id);
                    }
                    SchedulerEvent::Error(e) => {
                        if let Some(mut t) = mgr.tasks.get_mut(&id) {
                            t.state = DownloadState::Error(e.clone());
                            t.error_message = Some(e);
                        }
                    }
                }
            }

            // Await scheduler completion
            if let Err(e) = sched_handle.await.unwrap_or_else(|e| Err(anyhow::anyhow!("{}", e))) {
                tracing::error!("Scheduler error for {}: {}", id, e);
                if let Some(mut t) = mgr.tasks.get_mut(&id) {
                    t.state = DownloadState::Error(e.to_string());
                }
            }
        });
    }
}
