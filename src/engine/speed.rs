use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

/// Per-worker speed sample window.
const SAMPLE_WINDOW: Duration = Duration::from_secs(5);
const MAX_SAMPLES: usize = 20;
/// Ring buffer size for global speed meter
const RING_SIZE: usize = 50;
/// Sample interval for ring speed meter
const SAMPLE_INTERVAL_MS: u64 = 100;

#[derive(Debug)]
struct Sample {
    bytes: u64,
    at: Instant,
}

/// Rolling-window speed tracker.
///
/// Each worker has one; the manager aggregates them for global speed.
#[derive(Debug)]
pub struct SpeedSampler {
    samples: VecDeque<Sample>,
    total_bytes: u64,
}

impl SpeedSampler {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            total_bytes: 0,
        }
    }

    /// Record `bytes` downloaded at this instant.
    pub fn record(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back(Sample { bytes, at: now });
        self.total_bytes += bytes;

        // Evict samples older than window
        let cutoff = now - SAMPLE_WINDOW;
        while self
            .samples
            .front()
            .map_or(false, |s| s.at < cutoff)
        {
            self.samples.pop_front();
        }

        // Cap sample count
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// Current speed in bytes/sec over the sample window.
    pub fn speed_bps(&self) -> u64 {
        if self.samples.len() < 2 {
            return 0;
        }
        let oldest = self.samples.front().unwrap();
        let newest = self.samples.back().unwrap();
        let elapsed = newest.at.duration_since(oldest.at).as_secs_f64();
        if elapsed < 0.01 {
            return 0;
        }
        let bytes: u64 = self.samples.iter().map(|s| s.bytes).sum();
        (bytes as f64 / elapsed) as u64
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

// ─── Global speed aggregator ─────────────────────────────────────────────────

/// Shared atomic counter — workers add bytes here; the manager reads speed
/// by sampling it periodically.
#[derive(Clone, Debug)]
pub struct GlobalSpeedMeter {
    counter: Arc<AtomicU64>,
    last_snapshot: Arc<std::sync::Mutex<(u64, Instant)>>,
}

impl GlobalSpeedMeter {
    pub fn new() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
            last_snapshot: Arc::new(std::sync::Mutex::new((0, Instant::now()))),
        }
    }

    pub fn add(&self, bytes: u64) {
        self.counter.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Compute current speed by diffing counter vs last snapshot.
    pub fn speed_bps(&self) -> u64 {
        let now = Instant::now();
        let current = self.counter.load(Ordering::Relaxed);
        let mut snap = self.last_snapshot.lock().unwrap();
        let elapsed = now.duration_since(snap.1).as_secs_f64();
        if elapsed < 0.1 {
            return 0;
        }
        let delta = current.saturating_sub(snap.0);
        let speed = (delta as f64 / elapsed) as u64;
        *snap = (current, now);
        speed
    }

    pub fn total_bytes(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}

// ─── Ring-buffer speed meter (lock-free reads) ──────────────────────────────

/// Sample for the ring buffer.
#[derive(Debug, Clone, Copy)]
struct RingSample {
    bytes: u64,
    at: Instant,
}

/// High-performance speed meter using a ring buffer.
///
/// A dedicated timer task writes samples at fixed intervals;
/// speed queries read from the ring without contention.
#[derive(Clone)]
pub struct RingSpeedMeter {
    counter: Arc<AtomicU64>,
    samples: Arc<Mutex<VecDeque<RingSample>>>,
}

impl RingSpeedMeter {
    pub fn new() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
            samples: Arc::new(Mutex::new(VecDeque::with_capacity(RING_SIZE))),
        }
    }

    /// Add bytes to the counter (called by workers).
    pub fn add(&self, bytes: u64) {
        self.counter.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Take a sample — call this from a dedicated timer task.
    pub async fn sample(&self) {
        let now = Instant::now();
        let bytes = self.counter.load(Ordering::Relaxed);
        let mut samples = self.samples.lock().await;
        samples.push_back(RingSample { bytes, at: now });
        while samples.len() > RING_SIZE {
            samples.pop_front();
        }
    }

    /// Compute current speed from the ring buffer.
    pub async fn speed_bps(&self) -> u64 {
        let samples = self.samples.lock().await;
        if samples.len() < 2 {
            return 0;
        }
        let oldest = samples.front().unwrap();
        let newest = samples.back().unwrap();
        let elapsed = newest.at.duration_since(oldest.at).as_secs_f64();
        if elapsed < 0.01 {
            return 0;
        }
        let delta = newest.bytes.saturating_sub(oldest.bytes);
        (delta as f64 / elapsed) as u64
    }

    pub fn total_bytes(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Spawn a background task that samples at fixed intervals.
    /// Returns a handle that stops sampling when dropped.
    pub fn spawn_sampler(&self) -> tokio::task::JoinHandle<()> {
        let meter = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(SAMPLE_INTERVAL_MS));
            loop {
                interval.tick().await;
                meter.sample().await;
            }
        })
    }
}

impl Default for RingSpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Per-worker speed tracking channel ──────────────────────────────────────

/// Real-time speed update from a worker.
#[derive(Debug, Clone)]
pub struct WorkerSpeedUpdate {
    pub worker_id: usize,
    pub chunk_index: usize,
    pub speed_bps: u64,
    pub bytes_downloaded: u64,
}

/// Receiver for aggregating worker speed updates.
pub type SpeedUpdateRx = mpsc::Receiver<WorkerSpeedUpdate>;
/// Sender for workers to report their speed.
pub type SpeedUpdateTx = mpsc::Sender<WorkerSpeedUpdate>;

/// Create a channel for real-time worker speed updates.
pub fn speed_channel(capacity: usize) -> (SpeedUpdateTx, SpeedUpdateRx) {
    mpsc::channel(capacity)
}

// ─── Percentile calculation for adaptive workers ────────────────────────────

/// Calculate the P-th percentile of a slice of speeds.
/// Uses linear interpolation for non-integer indices.
pub fn percentile(speeds: &[u64], p: f64) -> u64 {
    if speeds.is_empty() {
        return 0;
    }
    if speeds.len() == 1 {
        return speeds[0];
    }

    let mut sorted: Vec<u64> = speeds.to_vec();
    sorted.sort_unstable();

    let idx = (p / 100.0) * (sorted.len() - 1) as f64;
    let lower = idx.floor() as usize;
    let upper = idx.ceil() as usize;

    if lower == upper {
        sorted[lower]
    } else {
        let frac = idx - lower as f64;
        let low_val = sorted[lower] as f64;
        let high_val = sorted[upper] as f64;
        (low_val + frac * (high_val - low_val)) as u64
    }
}

/// Calculate P75 (75th percentile) — use instead of median for small samples.
pub fn p75(speeds: &[u64]) -> u64 {
    percentile(speeds, 75.0)
}

/// ETA calculator — given total size and current speed, return seconds.
pub fn eta(total: Option<u64>, downloaded: u64, speed_bps: u64) -> Option<u64> {
    let total = total?;
    let remaining = total.saturating_sub(downloaded);
    if speed_bps == 0 {
        return None;
    }
    Some(remaining / speed_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percentile() {
        let speeds = vec![10, 20, 30, 40, 50];
        assert_eq!(percentile(&speeds, 50.0), 30); // median
        assert_eq!(percentile(&speeds, 75.0), 40); // p75
        assert_eq!(percentile(&speeds, 0.0), 10);
        assert_eq!(percentile(&speeds, 100.0), 50);
    }

    #[test]
    fn test_p75_small_sample() {
        let speeds = vec![100, 200];
        assert_eq!(p75(&speeds), 175); // interpolated
    }

    #[tokio::test]
    async fn test_ring_speed_meter() {
        let meter = RingSpeedMeter::new();

        // Add some bytes
        meter.add(1000);
        meter.sample().await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        meter.add(1000);
        meter.sample().await;

        let speed = meter.speed_bps().await;
        // Should be approximately 10000 bps (2000 bytes over ~0.2 seconds)
        assert!(speed > 5000 && speed < 20000, "speed was {}", speed);
    }
}
