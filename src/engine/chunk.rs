use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// State of a single byte-range chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkState {
    Pending,
    InFlight { worker_id: usize },
    Complete,
    Failed { attempts: u32 },
}

/// A single chunk / byte-range within a download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub index: usize,
    pub start: u64,
    /// Inclusive end byte
    pub end: u64,
    /// Bytes downloaded for this chunk so far (for resumption)
    pub downloaded: u64,
    pub state: ChunkState,
}

impl Chunk {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn remaining(&self) -> u64 {
        self.len().saturating_sub(self.downloaded)
    }

    pub fn is_complete(&self) -> bool {
        self.downloaded >= self.len()
    }

    /// Current offset into the file for the next read.
    pub fn current_offset(&self) -> u64 {
        self.start + self.downloaded
    }
}

/// The full chunk map for a download — wraps a Vec<Chunk> behind a Mutex
/// so workers can mutate state concurrently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMap {
    pub chunks: Vec<Chunk>,
}

impl ChunkMap {
    /// Divide `total_bytes` into `n` roughly-equal chunks.
    pub fn new(total_bytes: u64, n: usize) -> Self {
        let chunk_size = (total_bytes + n as u64 - 1) / n as u64;
        let mut chunks = Vec::with_capacity(n);
        for i in 0..n {
            let start = i as u64 * chunk_size;
            let end = ((i as u64 + 1) * chunk_size - 1).min(total_bytes - 1);
            if start > total_bytes - 1 {
                break;
            }
            chunks.push(Chunk {
                index: i,
                start,
                end,
                downloaded: 0,
                state: ChunkState::Pending,
            });
        }
        Self { chunks }
    }

    /// Single-chunk map for servers that don't support range requests.
    pub fn single(total_bytes: u64) -> Self {
        Self {
            chunks: vec![Chunk {
                index: 0,
                start: 0,
                end: total_bytes.saturating_sub(1),
                downloaded: 0,
                state: ChunkState::Pending,
            }],
        }
    }

    /// Unknown size — one open-ended chunk.
    pub fn unknown() -> Self {
        Self {
            chunks: vec![Chunk {
                index: 0,
                start: 0,
                end: u64::MAX,
                downloaded: 0,
                state: ChunkState::Pending,
            }],
        }
    }

    pub fn total_downloaded(&self) -> u64 {
        self.chunks.iter().map(|c| c.downloaded).sum()
    }

    pub fn all_complete(&self) -> bool {
        self.chunks.iter().all(|c| c.is_complete())
    }

    pub fn pending_count(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| matches!(c.state, ChunkState::Pending))
            .count()
    }
}

// ─── Work-Stealing Queue ─────────────────────────────────────────────────────

/// Thread-safe work queue that supports work stealing.
///
/// Workers own a chunk index. When they finish early they can steal
/// the largest remaining chunk from the slowest peer by splitting it.
///
/// Work stealing uses remaining TIME (not bytes) as the metric:
/// `remaining_time = remaining_bytes / current_speed`
#[derive(Debug, Clone)]
pub struct WorkQueue {
    inner: Arc<Mutex<ChunkMap>>,
    /// Atomic counter of total bytes written — used for speed sampling
    pub bytes_written: Arc<AtomicU64>,
    /// Per-worker speed tracking for intelligent work stealing
    worker_speeds: Arc<Mutex<HashMap<usize, u64>>>,
}

impl WorkQueue {
    pub fn new(map: ChunkMap) -> Self {
        Self {
            inner: Arc::new(Mutex::new(map)),
            bytes_written: Arc::new(AtomicU64::new(0)),
            worker_speeds: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Update the recorded speed for a worker.
    pub async fn update_worker_speed(&self, worker_id: usize, speed_bps: u64) {
        self.worker_speeds.lock().await.insert(worker_id, speed_bps);
    }

    /// Get the current speed for a worker.
    pub async fn get_worker_speed(&self, worker_id: usize) -> u64 {
        self.worker_speeds
            .lock()
            .await
            .get(&worker_id)
            .copied()
            .unwrap_or(0)
    }

    /// Claim the next pending chunk for `worker_id`.
    /// In sequential mode, always returns the lowest-index pending chunk.
    pub async fn claim_next(
        &self,
        worker_id: usize,
        sequential: bool,
    ) -> Option<Chunk> {
        let mut map = self.inner.lock().await;
        let idx = if sequential {
            map.chunks
                .iter()
                .position(|c| matches!(c.state, ChunkState::Pending))
        } else {
            // Claim the largest pending chunk (maximises parallelism benefit)
            map.chunks
                .iter()
                .enumerate()
                .filter(|(_, c)| matches!(c.state, ChunkState::Pending))
                .max_by_key(|(_, c)| c.remaining())
                .map(|(i, _)| i)
        };

        if let Some(i) = idx {
            map.chunks[i].state = ChunkState::InFlight { worker_id };
            Some(map.chunks[i].clone())
        } else {
            None
        }
    }

    /// Work stealing: split the chunk with the highest remaining TIME and hand the
    /// second half to `thief_worker_id`. Returns the stolen sub-chunk.
    ///
    /// The steal priority metric is: `remaining_time = remaining_bytes / speed`
    /// This ensures we steal from slow connections, not just large chunks.
    pub async fn steal(&self, thief_worker_id: usize) -> Option<Chunk> {
        let mut map = self.inner.lock().await;
        let speeds = self.worker_speeds.lock().await;

        // Calculate remaining time for each in-flight chunk
        // Higher score = more time remaining = better steal target
        let steal_idx = map
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                matches!(c.state, ChunkState::InFlight { .. })
                    && c.remaining() > 2 * 1024 * 1024 // only steal if >2 MiB left
            })
            .map(|(i, c)| {
                let worker_id = match c.state {
                    ChunkState::InFlight { worker_id } => worker_id,
                    _ => unreachable!(),
                };
                let speed = speeds.get(&worker_id).copied().unwrap_or(1);
                // Remaining time in arbitrary units (higher = better steal target)
                // Use u64::MAX for zero speed to prioritize stalled workers
                let remaining_time = if speed == 0 {
                    u64::MAX
                } else {
                    c.remaining() / speed
                };
                (i, remaining_time)
            })
            .max_by_key(|(_, remaining_time)| *remaining_time)
            .map(|(i, _)| i);

        let i = steal_idx?;

        // Split: the original chunk keeps the first half,
        // the stolen chunk gets the second half.
        let original = &map.chunks[i];
        let split_point = original.current_offset() + (original.remaining() / 2);

        let stolen_chunk = Chunk {
            index: map.chunks.len(),
            start: split_point,
            end: original.end,
            downloaded: 0,
            state: ChunkState::InFlight {
                worker_id: thief_worker_id,
            },
        };

        // Shrink the original
        map.chunks[i].end = split_point - 1;

        map.chunks.push(stolen_chunk.clone());
        tracing::debug!(
            "Work steal: worker {} stole chunk {} ({}..{}) from worker {:?}",
            thief_worker_id,
            stolen_chunk.index,
            stolen_chunk.start,
            stolen_chunk.end,
            map.chunks[i].state
        );

        Some(stolen_chunk)
    }

    /// Mark a chunk as complete and add its bytes to the atomic counter.
    pub async fn mark_complete(&self, chunk_index: usize, bytes: u64) {
        let mut map = self.inner.lock().await;
        if let Some(c) = map.chunks.iter_mut().find(|c| c.index == chunk_index) {
            c.downloaded = c.len();
            c.state = ChunkState::Complete;
        }
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Update progress for an in-flight chunk.
    pub async fn update_progress(&self, chunk_index: usize, bytes_so_far: u64) {
        let mut map = self.inner.lock().await;
        if let Some(c) = map.chunks.iter_mut().find(|c| c.index == chunk_index) {
            let delta = bytes_so_far.saturating_sub(c.downloaded);
            c.downloaded = bytes_so_far;
            self.bytes_written.fetch_add(delta, Ordering::Relaxed);
        }
    }

    /// Mark a chunk as failed (to be retried by the scheduler).
    pub async fn mark_failed(&self, chunk_index: usize) {
        let mut map = self.inner.lock().await;
        if let Some(c) = map.chunks.iter_mut().find(|c| c.index == chunk_index) {
            c.state = match &c.state {
                ChunkState::InFlight { .. } => ChunkState::Failed { attempts: 1 },
                ChunkState::Failed { attempts } => ChunkState::Failed {
                    attempts: attempts + 1,
                },
                other => other.clone(),
            };
        }
    }

    /// Reset failed chunks back to Pending so the scheduler can retry them.
    pub async fn reset_failed(&self, max_attempts: u32) -> usize {
        let mut map = self.inner.lock().await;
        let mut reset = 0;
        for c in map.chunks.iter_mut() {
            if let ChunkState::Failed { attempts } = c.state {
                if attempts <= max_attempts {
                    c.state = ChunkState::Pending;
                    reset += 1;
                }
            }
        }
        reset
    }

    pub async fn all_complete(&self) -> bool {
        self.inner.lock().await.all_complete()
    }

    pub async fn snapshot(&self) -> ChunkMap {
        self.inner.lock().await.clone()
    }
}
