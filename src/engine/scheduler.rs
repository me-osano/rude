use crate::engine::{
    chunk::{ChunkMap, WorkQueue},
    config::EngineConfig,
    mirror::MirrorPool,
    speed::GlobalSpeedMeter,
    task::{DownloadState, DownloadTask, TaskOptions},
    worker::{download_chunk, WorkerResult},
};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Instant};
use url::Url;

/// Events emitted by the scheduler back to the manager.
#[derive(Debug)]
pub enum SchedulerEvent {
    Progress {
        downloaded: u64,
        speed_bps: u64,
        connections: usize,
    },
    StateChange(DownloadState),
    Complete { output_path: PathBuf },
    Error(String),
}

/// Drives the lifecycle of a single DownloadTask.
pub struct Scheduler {
    task: DownloadTask,
    config: EngineConfig,
    client: Client,
    event_tx: mpsc::Sender<SchedulerEvent>,
}

impl Scheduler {
    pub fn new(
        task: DownloadTask,
        config: EngineConfig,
        client: Client,
        event_tx: mpsc::Sender<SchedulerEvent>,
    ) -> Self {
        Self { task, config, client, event_tx }
    }

    /// Main entry point — runs the full download lifecycle.
    pub async fn run(mut self) -> Result<()> {
        // 1. Probe
        self.emit(SchedulerEvent::StateChange(DownloadState::Probing)).await;
        let probe = match self.probe().await {
            Ok(p) => p,
            Err(e) => {
                self.emit(SchedulerEvent::Error(e.to_string())).await;
                return Err(e);
            }
        };

        self.task.total_bytes = probe.content_length;
        self.task.accepts_ranges = probe.accepts_ranges;
        self.task.etag = probe.etag;

        // 2. Determine output path
        let filename = self.task.infer_filename();
        let dir = self
            .task
            .options
            .dir
            .clone()
            .unwrap_or_else(|| self.config.download_dir.clone());
        tokio::fs::create_dir_all(&dir).await?;
        let output_path = dir.join(&filename);

        // 2.5. Disk space preflight check
        if let Some(total) = probe.content_length {
            if let Err(e) = crate::engine::io::ensure_disk_space(&dir, total) {
                self.emit(SchedulerEvent::Error(e.to_string())).await;
                return Err(e);
            }
        }

        // 3. Build chunk map
        // Adjust split count based on HTTP version (HTTP/2 uses multiplexing)
        let effective_split = if !probe.accepts_ranges {
            1
        } else {
            self.task.options.split.unwrap_or(self.config.split)
        };

        // For HTTP/2, we use fewer connections but more streams per connection
        let effective_connections = if probe.is_http2 {
            // HTTP/2 multiplexing: use 1-2 connections with multiple streams
            tracing::info!("HTTP/2 detected, using multiplexed streams");
            effective_split.min(2)
        } else {
            self.task
                .options
                .max_connections
                .unwrap_or(self.config.max_connections_per_server)
        };

        let chunk_map = match probe.content_length {
            Some(len) if len >= self.config.min_split_size && effective_split > 1 => {
                ChunkMap::new(len, effective_split)
            }
            Some(len) => ChunkMap::single(len),
            None => ChunkMap::unknown(),
        };

        // 4. Pre-allocate file
        let file = pre_allocate(&output_path, probe.content_length).await?;
        drop(file); // close; workers will re-open with seek

        // 5. Set up work queue and mirrors
        let queue = WorkQueue::new(chunk_map.clone());
        let mirrors = MirrorPool::new(
            self.task.urls.clone(),
            self.task
                .options
                .max_connections
                .unwrap_or(self.config.max_connections_per_server),
        );

        self.emit(SchedulerEvent::StateChange(DownloadState::Active)).await;

        // 6. Download loop with adaptive workers + work stealing
        self.download_loop(
            &queue,
            &mirrors,
            &output_path,
            probe.content_length,
        )
        .await?;

        // 7. Integrity check
        if let Some(cs) = &self.task.options.checksum {
            self.verify_checksum(&output_path, cs).await?;
        }

        // 8. Done
        self.emit(SchedulerEvent::StateChange(DownloadState::Assembling)).await;
        self.emit(SchedulerEvent::Complete {
            output_path: output_path.clone(),
        })
        .await;
        Ok(())
    }

    // ── Probe ────────────────────────────────────────────────────────────────

    async fn probe(&self) -> Result<ProbeResult> {
        for url in &self.task.urls {
            match self.probe_url(url).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    tracing::warn!("Probe failed for {}: {}", url, e);
                }
            }
        }
        Err(anyhow!("All mirrors failed during probe"))
    }

    async fn probe_url(&self, url: &Url) -> Result<ProbeResult> {
        let resp = self
            .client
            .head(url.as_str())
            .send()
            .await
            .context("HEAD request failed")?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("HEAD returned HTTP {}", status));
        }

        let content_length = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        let accepts_ranges = resp
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);

        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Detect HTTP/2 for multiplexing optimization
        let is_http2 = matches!(resp.version(), reqwest::Version::HTTP_2);

        Ok(ProbeResult {
            content_length,
            accepts_ranges,
            etag,
            is_http2,
        })
    }

    // ── Download loop ────────────────────────────────────────────────────────

    async fn download_loop(
        &self,
        queue: &WorkQueue,
        mirrors: &MirrorPool,
        output_path: &PathBuf,
        total_bytes: Option<u64>,
    ) -> Result<()> {
        let max_connections = self
            .task
            .options
            .max_connections
            .unwrap_or(self.config.max_connections_per_server);
        let sequential = self.task.options.sequential;

        let speed_meter = GlobalSpeedMeter::new();
        let mut handles: Vec<JoinHandle<WorkerResult>> = Vec::new();
        let mut worker_speeds: HashMap<usize, u64> = HashMap::new();
        let mut worker_id_counter = 0usize;

        // Adaptive worker polling interval
        let mut check_interval = tokio::time::interval(Duration::from_secs(2));

        loop {
            // Tick adaptive check
            tokio::select! {
                _ = check_interval.tick() => {
                    // Collect finished workers
                    let mut finished = Vec::new();
                    let mut still_running = Vec::new();
                    for h in handles.drain(..) {
                        if h.is_finished() {
                            finished.push(h);
                        } else {
                            still_running.push(h);
                        }
                    }
                    handles = still_running;

                    for h in finished {
                        match h.await? {
                            WorkerResult::Complete { chunk_index, bytes, speed_bps, mirror } => {
                                queue.mark_complete(chunk_index, bytes).await;
                                mirrors.report_success(&mirror, speed_bps).await;
                                worker_speeds.insert(chunk_index, speed_bps);
                            }
                            WorkerResult::Failed { chunk_index, mirror, error } => {
                                tracing::warn!("Chunk {} failed: {}", chunk_index, error);
                                queue.mark_failed(chunk_index).await;
                                mirrors.report_failure(&mirror).await;
                            }
                        }
                    }

                    // Exit condition
                    if queue.all_complete().await && handles.is_empty() {
                        break;
                    }

                    // Retry failed chunks
                    let retried = queue.reset_failed(self.config.max_retries).await;
                    if retried > 0 {
                        tracing::info!("Retrying {} failed chunks", retried);
                    }

                    // Adaptive: kill workers below speed threshold
                    if self.config.adaptive_workers && !worker_speeds.is_empty() {
                        let median = median_speed(&worker_speeds);
                        let threshold =
                            (median as f64 * self.config.slow_worker_threshold) as u64;
                        // (In a real impl, we'd track JoinHandle→speed and abort slow ones.
                        //  Shown here as the logic structure.)
                        if threshold > 0 {
                            tracing::debug!(
                                "Speed median={}bps threshold={}bps",
                                median,
                                threshold
                            );
                        }
                    }

                    // Spawn new workers for pending chunks up to connection limit
                    while handles.len() < max_connections {
                        let chunk = if self.config.work_stealing && !handles.is_empty() {
                            // First try claiming a new pending chunk,
                            // fall back to stealing from a slow worker
                            match queue.claim_next(worker_id_counter, sequential).await {
                                Some(c) => c,
                                None => match queue.steal(worker_id_counter).await {
                                    Some(c) => c,
                                    None => break,
                                },
                            }
                        } else {
                            match queue.claim_next(worker_id_counter, sequential).await {
                                Some(c) => c,
                                None => break,
                            }
                        };

                        let mirror_url = match mirrors.pick().await {
                            Some(u) => u,
                            None => {
                                tracing::error!("No mirrors available");
                                break;
                            }
                        };

                        let file = File::open(output_path).await?;
                        let q = queue.clone();
                        let config = self.config.clone();
                        let client = self.client.clone();
                        let wid = worker_id_counter;
                        worker_id_counter += 1;

                        handles.push(tokio::spawn(async move {
                            download_chunk(wid, chunk, mirror_url, client, q, file, config).await
                        }));
                    }

                    // Emit progress
                    let downloaded = queue.bytes_written.load(std::sync::atomic::Ordering::Relaxed);
                    let speed = speed_meter.speed_bps();
                    self.emit(SchedulerEvent::Progress {
                        downloaded,
                        speed_bps: speed,
                        connections: handles.len(),
                    }).await;
                }
            }
        }

        Ok(())
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn verify_checksum(
        &self,
        path: &PathBuf,
        cs: &crate::engine::task::Checksum,
    ) -> Result<()> {
        use digest::Digest;
        use md5::Md5;
        use sha1::Sha1;
        use sha2::Sha256;

        let data = tokio::fs::read(path).await?;
        let actual = match cs.algorithm {
            crate::engine::task::ChecksumAlgo::Sha256 => {
                hex::encode(Sha256::digest(&data))
            }
            crate::engine::task::ChecksumAlgo::Md5 => {
                hex::encode(Md5::digest(&data))
            }
            crate::engine::task::ChecksumAlgo::Sha1 => {
                hex::encode(Sha1::digest(&data))
            }
        };

        if actual.to_lowercase() != cs.value.to_lowercase() {
            return Err(anyhow!(
                "Checksum mismatch: expected {} got {}",
                cs.value,
                actual
            ));
        }
        tracing::info!("Checksum verified OK");
        Ok(())
    }

    async fn emit(&self, event: SchedulerEvent) {
        let _ = self.event_tx.send(event).await;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

struct ProbeResult {
    content_length: Option<u64>,
    accepts_ranges: bool,
    etag: Option<String>,
    is_http2: bool,
}

/// Calculate P75 (75th percentile) of worker speeds.
/// More robust than median for small sample sizes.
fn p75_speed(speeds: &HashMap<usize, u64>) -> u64 {
    let vals: Vec<u64> = speeds.values().copied().collect();
    crate::engine::speed::p75(&vals)
}

async fn pre_allocate(path: &PathBuf, size: Option<u64>) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await?;

    if let Some(len) = size {
        // Set file length to pre-allocate space
        file.set_len(len).await?;
    }

    Ok(file)
}
