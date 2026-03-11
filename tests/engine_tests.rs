use rude_core::engine::chunk::{ChunkMap, ChunkState, WorkQueue};
use rude_core::engine::speed::SpeedSampler;

// ── ChunkMap tests ────────────────────────────────────────────────────────────

#[test]
fn chunk_map_covers_full_range() {
    let size = 100_000_000u64; // 100 MiB
    let map = ChunkMap::new(size, 8);
    assert_eq!(map.chunks.len(), 8);
    assert_eq!(map.chunks[0].start, 0);
    assert_eq!(map.chunks[7].end, size - 1);

    // No gaps or overlaps
    for i in 1..map.chunks.len() {
        assert_eq!(map.chunks[i].start, map.chunks[i - 1].end + 1);
    }
}

#[test]
fn single_chunk_map() {
    let map = ChunkMap::single(1024);
    assert_eq!(map.chunks.len(), 1);
    assert_eq!(map.chunks[0].start, 0);
    assert_eq!(map.chunks[0].end, 1023);
}

#[test]
fn chunk_remaining_correct() {
    let map = ChunkMap::new(1000, 4);
    for chunk in &map.chunks {
        assert_eq!(chunk.remaining(), chunk.len());
    }
}

// ── WorkQueue tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn work_queue_claim_all() {
    let map = ChunkMap::new(40_000_000, 4);
    let queue = WorkQueue::new(map);

    let mut claimed = Vec::new();
    while let Some(c) = queue.claim_next(0, false).await {
        claimed.push(c.index);
    }
    assert_eq!(claimed.len(), 4);
}

#[tokio::test]
async fn work_queue_sequential_ordering() {
    let map = ChunkMap::new(40_000_000, 4);
    let queue = WorkQueue::new(map);

    let mut last_index: Option<usize> = None;
    while let Some(c) = queue.claim_next(0, true).await {
        if let Some(prev) = last_index {
            assert!(c.index > prev, "sequential must be ascending");
        }
        last_index = Some(c.index);
        queue.mark_complete(c.index, c.len()).await;
    }
}

#[tokio::test]
async fn work_queue_mark_complete() {
    let map = ChunkMap::new(10_000, 2);
    let queue = WorkQueue::new(map);

    let c0 = queue.claim_next(0, false).await.unwrap();
    let bytes = c0.len();
    queue.mark_complete(c0.index, bytes).await;

    let snap = queue.snapshot().await;
    assert!(matches!(snap.chunks[c0.index].state, ChunkState::Complete));
    assert_eq!(queue.bytes_written.load(std::sync::atomic::Ordering::Relaxed), bytes);
}

#[tokio::test]
async fn work_stealing_splits_chunk() {
    let map = ChunkMap::new(100_000_000, 1); // single big chunk
    let queue = WorkQueue::new(map);

    // Worker 0 claims the only chunk
    let c = queue.claim_next(0, false).await.unwrap();
    assert_eq!(c.index, 0);

    // Worker 1 steals half of it
    let stolen = queue.steal(1).await;
    assert!(stolen.is_some(), "steal should succeed on large in-flight chunk");
    let stolen = stolen.unwrap();
    assert_eq!(stolen.worker_id_from_state(), Some(1));

    // Total coverage is preserved
    let snap = queue.snapshot().await;
    let total_covered: u64 = snap.chunks.iter().map(|c| c.len()).sum();
    assert_eq!(total_covered, 100_000_000);
}

// ── SpeedSampler tests ────────────────────────────────────────────────────────

#[test]
fn speed_sampler_zero_before_samples() {
    let s = SpeedSampler::new();
    assert_eq!(s.speed_bps(), 0);
}

#[test]
fn speed_sampler_records_total() {
    let mut s = SpeedSampler::new();
    s.record(1000);
    s.record(2000);
    assert_eq!(s.total_bytes(), 3000);
}

// ── Helper extension for test ─────────────────────────────────────────────────

trait ChunkExt {
    fn worker_id_from_state(&self) -> Option<usize>;
}

impl ChunkExt for rude_core::engine::chunk::Chunk {
    fn worker_id_from_state(&self) -> Option<usize> {
        match self.state {
            ChunkState::InFlight { worker_id } => Some(worker_id),
            _ => None,
        }
    }
}
