use crate::engine::{
    chunk::{Chunk, WorkQueue},
    config::EngineConfig,
    io::WriteHandle,
    mirror::MirrorPool,
    speed::{SpeedSampler, SpeedUpdateTx, WorkerSpeedUpdate},
};
use anyhow::{anyhow, Result};
use reqwest::Client;
use std::io::SeekFrom;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::time::sleep;
use url::Url;

/// Write buffer size — aligned to 4K pages for optimal I/O
pub const WRITE_BUF: usize = 256 * 1024; // 256 KiB write buffer

/// Result returned by a worker when it finishes a chunk attempt.
#[derive(Debug)]
pub enum WorkerResult {
    Complete {
        chunk_index: usize,
        bytes: u64,
        speed_bps: u64,
        mirror: Url,
    },
    Failed {
        chunk_index: usize,
        mirror: Url,
        error: String,
        /// HTTP status code if available (for error classification)
        status_code: Option<u16>,
    },
}

/// Download a single chunk from `mirror_url`, writing into `file` at the
/// correct byte offset.
///
/// Reports incremental progress back through `queue.update_progress`.
/// Speed throttling (token bucket) is applied if `max_speed_bps > 0`.
pub async fn download_chunk(
    worker_id: usize,
    chunk: Chunk,
    mirror_url: Url,
    client: Client,
    queue: WorkQueue,
    file: File,
    config: EngineConfig,
) -> WorkerResult {
    let chunk_index = chunk.index;
    let result = do_download(
        worker_id,
        &chunk,
        &mirror_url,
        &client,
        &queue,
        file,
        &config,
        None, // No speed updates
    )
    .await;

    match result {
        Ok((bytes, speed)) => WorkerResult::Complete {
            chunk_index,
            bytes,
            speed_bps: speed,
            mirror: mirror_url,
        },
        Err((e, status)) => {
            tracing::warn!(
                "Worker {} chunk {} failed: {}",
                worker_id,
                chunk_index,
                e
            );
            WorkerResult::Failed {
                chunk_index,
                mirror: mirror_url,
                error: e.to_string(),
                status_code: status,
            }
        }
    }
}

async fn do_download(
    worker_id: usize,
    chunk: &Chunk,
    url: &Url,
    client: &Client,
    queue: &WorkQueue,
    mut file: File,
    config: &EngineConfig,
    speed_tx: Option<&SpeedUpdateTx>,
) -> std::result::Result<(u64, u64), (anyhow::Error, Option<u16>)> {
    let resume_offset = chunk.current_offset();
    let end = chunk.end;

    let range_header = if end == u64::MAX {
        format!("bytes={}-", resume_offset)
    } else {
        format!("bytes={}-{}", resume_offset, end)
    };

    let resp = client
        .get(url.as_str())
        .header("Range", &range_header)
        .send()
        .await
        .map_err(|e| (e.into(), None))?;

    let status = resp.status();
    let status_code = status.as_u16();
    if !status.is_success() && status_code != 206 {
        return Err((anyhow!("HTTP {}", status), Some(status_code)));
    }

    // Seek file to correct offset
    file.seek(SeekFrom::Start(resume_offset))
        .await
        .map_err(|e| (e.into(), None))?;

    let mut sampler = SpeedSampler::new();
    let mut bytes_written: u64 = 0;
    let mut buf = Vec::with_capacity(WRITE_BUF);

    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;

    let start = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_speed_update = Instant::now();

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| (e.into(), None))?;
        buf.extend_from_slice(&bytes);

        if buf.len() >= WRITE_BUF {
            file.write_all(&buf).await.map_err(|e| (e.into(), None))?;
            let n = buf.len() as u64;
            bytes_written += n;
            sampler.record(n);
            buf.clear();

            // Throttle if speed cap is set
            throttle(bytes_written, start, config.max_download_speed.max(0)).await;

            // Report progress every 250 ms
            if last_progress.elapsed() > Duration::from_millis(250) {
                queue
                    .update_progress(chunk.index, chunk.downloaded + bytes_written)
                    .await;
                last_progress = Instant::now();
            }

            // Send real-time speed updates every 500ms for adaptive scheduling
            if let Some(tx) = speed_tx {
                if last_speed_update.elapsed() > Duration::from_millis(500) {
                    let current_speed = sampler.speed_bps();
                    // Update worker speed in queue for work-stealing decisions
                    queue.update_worker_speed(worker_id, current_speed).await;
                    let _ = tx
                        .send(WorkerSpeedUpdate {
                            worker_id,
                            chunk_index: chunk.index,
                            speed_bps: current_speed,
                            bytes_downloaded: chunk.downloaded + bytes_written,
                        })
                        .await;
                    last_speed_update = Instant::now();
                }
            }
        }
    }

    // Flush remainder
    if !buf.is_empty() {
        file.write_all(&buf).await.map_err(|e| (e.into(), None))?;
        bytes_written += buf.len() as u64;
        sampler.record(buf.len() as u64);
    }

    file.flush().await.map_err(|e| (e.into(), None))?;

    Ok((bytes_written, sampler.speed_bps()))
}

/// Simple token-bucket throttler.
/// If we're ahead of the allowed rate, sleep the deficit.
async fn throttle(bytes_written: u64, start: Instant, max_bps: u64) {
    if max_bps == 0 {
        return;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let allowed = (elapsed * max_bps as f64) as u64;
    if bytes_written > allowed {
        let excess = bytes_written - allowed;
        let sleep_secs = excess as f64 / max_bps as f64;
        sleep(Duration::from_secs_f64(sleep_secs)).await;
    }
}

/// Enhanced worker that uses pwrite and the write-back pressure system.
/// This variant is optimized for high-throughput scenarios.
pub async fn download_chunk_v2(
    worker_id: usize,
    chunk: Chunk,
    mirror_url: Url,
    client: Client,
    queue: WorkQueue,
    write_handle: WriteHandle,
    config: EngineConfig,
    speed_tx: Option<SpeedUpdateTx>,
) -> WorkerResult {
    let chunk_index = chunk.index;
    let result = do_download_v2(
        worker_id,
        &chunk,
        &mirror_url,
        &client,
        &queue,
        write_handle,
        &config,
        speed_tx.as_ref(),
    )
    .await;

    match result {
        Ok((bytes, speed)) => WorkerResult::Complete {
            chunk_index,
            bytes,
            speed_bps: speed,
            mirror: mirror_url,
        },
        Err((e, status)) => {
            tracing::warn!(
                "Worker {} chunk {} failed: {}",
                worker_id,
                chunk_index,
                e
            );
            WorkerResult::Failed {
                chunk_index,
                mirror: mirror_url,
                error: e.to_string(),
                status_code: status,
            }
        }
    }
}

/// Download using pwrite + WriteHandle for back-pressure.
async fn do_download_v2(
    worker_id: usize,
    chunk: &Chunk,
    url: &Url,
    client: &Client,
    queue: &WorkQueue,
    write_handle: WriteHandle,
    config: &EngineConfig,
    speed_tx: Option<&SpeedUpdateTx>,
) -> std::result::Result<(u64, u64), (anyhow::Error, Option<u16>)> {
    let resume_offset = chunk.current_offset();
    let end = chunk.end;

    let range_header = if end == u64::MAX {
        format!("bytes={}-", resume_offset)
    } else {
        format!("bytes={}-{}", resume_offset, end)
    };

    let resp = client
        .get(url.as_str())
        .header("Range", &range_header)
        .send()
        .await
        .map_err(|e| (e.into(), None))?;

    let status = resp.status();
    let status_code = status.as_u16();
    if !status.is_success() && status_code != 206 {
        return Err((anyhow!("HTTP {}", status), Some(status_code)));
    }

    let mut sampler = SpeedSampler::new();
    let mut bytes_written: u64 = 0;
    let mut current_offset = resume_offset;
    let mut buf = Vec::with_capacity(WRITE_BUF);

    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;

    let start = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_speed_update = Instant::now();

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| (e.into(), None))?;
        buf.extend_from_slice(&bytes);

        if buf.len() >= WRITE_BUF {
            // Use pwrite via WriteHandle — this applies back-pressure if queue is full
            let data = std::mem::take(&mut buf);
            let write_len = data.len() as u64;
            write_handle
                .write(current_offset, data)
                .await
                .map_err(|e| (e.into(), None))?;

            current_offset += write_len;
            bytes_written += write_len;
            sampler.record(write_len);
            buf = Vec::with_capacity(WRITE_BUF);

            // Throttle if speed cap is set
            throttle(bytes_written, start, config.max_download_speed.max(0)).await;

            // Report progress every 250 ms
            if last_progress.elapsed() > Duration::from_millis(250) {
                queue
                    .update_progress(chunk.index, chunk.downloaded + bytes_written)
                    .await;
                last_progress = Instant::now();
            }

            // Send real-time speed updates every 500ms for adaptive scheduling
            if let Some(tx) = speed_tx {
                if last_speed_update.elapsed() > Duration::from_millis(500) {
                    let current_speed = sampler.speed_bps();
                    queue.update_worker_speed(worker_id, current_speed).await;
                    let _ = tx
                        .send(WorkerSpeedUpdate {
                            worker_id,
                            chunk_index: chunk.index,
                            speed_bps: current_speed,
                            bytes_downloaded: chunk.downloaded + bytes_written,
                        })
                        .await;
                    last_speed_update = Instant::now();
                }
            }
        }
    }

    // Flush remainder
    if !buf.is_empty() {
        let write_len = buf.len() as u64;
        write_handle
            .write(current_offset, buf)
            .await
            .map_err(|e| (e.into(), None))?;
        bytes_written += write_len;
        sampler.record(write_len);
    }

    Ok((bytes_written, sampler.speed_bps()))
}
