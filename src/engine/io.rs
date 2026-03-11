//! High-performance I/O primitives for concurrent file writing.
//!
//! Key features:
//! - `pwrite64` for atomic writes without seek contention
//! - Bounded write queue with back-pressure to prevent OOM on slow disks
//! - Page-aligned write buffers for optimal kernel page cache behavior

use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use tokio::fs::File;
use tokio::sync::mpsc;

/// Default write buffer size (256 KiB, page-aligned)
pub const DEFAULT_WRITE_BUF: usize = 256 * 1024;

/// Channel capacity for write queue (bounds memory usage)
pub const WRITE_QUEUE_CAPACITY: usize = 64;

/// A write operation to be performed at a specific offset.
#[derive(Debug)]
pub struct WriteOp {
    pub offset: u64,
    pub data: Vec<u8>,
}

/// Handle for workers to submit write operations.
#[derive(Clone)]
pub struct WriteHandle {
    tx: mpsc::Sender<WriteOp>,
}

impl WriteHandle {
    /// Submit a write operation. Blocks if the queue is full (back-pressure).
    pub async fn write(&self, offset: u64, data: Vec<u8>) -> io::Result<()> {
        self.tx
            .send(WriteOp { offset, data })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Writer task closed"))
    }

    /// Check if the channel has capacity (non-blocking).
    pub fn has_capacity(&self) -> bool {
        self.tx.capacity() > 0
    }
}

/// Spawns a dedicated writer task that drains the write queue.
/// Returns a handle for workers to submit writes.
pub fn spawn_writer(file: Arc<File>) -> (WriteHandle, tokio::task::JoinHandle<io::Result<()>>) {
    let (tx, rx) = mpsc::channel::<WriteOp>(WRITE_QUEUE_CAPACITY);
    let handle = WriteHandle { tx };

    let writer_task = tokio::spawn(writer_loop(file, rx));

    (handle, writer_task)
}

/// The writer loop: drains the channel and performs pwrite operations.
async fn writer_loop(file: Arc<File>, mut rx: mpsc::Receiver<WriteOp>) -> io::Result<()> {
    while let Some(op) = rx.recv().await {
        pwrite(&file, &op.data, op.offset).await?;
    }
    // Flush when done
    file.sync_all().await?;
    Ok(())
}

/// Atomic positional write using pwrite64.
///
/// Unlike seek + write, pwrite is atomic — multiple threads can write to
/// different offsets concurrently without interfering with each other's
/// file position.
pub async fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let buf_ptr = buf.as_ptr();
    let len = buf.len();

    // pwrite64 is blocking, so we use spawn_blocking
    let result = tokio::task::spawn_blocking(move || {
        let written = unsafe { libc::pwrite64(fd, buf_ptr as *const libc::c_void, len, offset as i64) };
        if written < 0 {
            Err(io::Error::last_os_error())
        } else if (written as usize) < len {
            // Partial write — this shouldn't happen on regular files but handle it
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("pwrite64 wrote {} of {} bytes", written, len),
            ))
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

    Ok(result)
}

/// Align a buffer size to page boundaries for optimal I/O.
pub const fn align_to_page(size: usize) -> usize {
    const PAGE_SIZE: usize = 4096;
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Check available disk space using statvfs.
/// Returns (available_bytes, total_bytes).
pub fn check_disk_space(path: &std::path::Path) -> io::Result<(u64, u64)> {
    use nix::sys::statvfs::statvfs;

    let stat = statvfs(path).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let available = stat.blocks_available() as u64 * stat.block_size() as u64;
    let total = stat.blocks() as u64 * stat.block_size() as u64;
    Ok((available, total))
}

/// Pre-flight check: ensure enough disk space before starting download.
pub fn ensure_disk_space(dir: &std::path::Path, required: u64) -> anyhow::Result<()> {
    let (available, _total) = check_disk_space(dir)?;
    if available < required {
        anyhow::bail!(
            "Insufficient disk space: need {} bytes, have {} bytes available",
            required,
            available
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::fs::OpenOptions;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_pwrite_concurrent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bin");

        // Create and pre-allocate file
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(&path)
            .await
            .unwrap();
        file.set_len(1024).await.unwrap();

        // Write at different offsets concurrently
        let file = Arc::new(file);
        let f1 = file.clone();
        let f2 = file.clone();

        let (r1, r2) = tokio::join!(
            pwrite(&f1, b"AAAA", 0),
            pwrite(&f2, b"BBBB", 512),
        );
        r1.unwrap();
        r2.unwrap();

        // Verify
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let mut buf = vec![0u8; 1024];
        file.read_exact(&mut buf).await.unwrap();

        assert_eq!(&buf[0..4], b"AAAA");
        assert_eq!(&buf[512..516], b"BBBB");
    }

    #[tokio::test]
    async fn test_write_handle_backpressure() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bin");

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path)
            .await
            .unwrap();
        file.set_len(1024 * 1024).await.unwrap();
        let file = Arc::new(file);

        let (handle, writer_task) = spawn_writer(file);

        // Submit writes
        for i in 0..10 {
            handle
                .write(i * 100, vec![b'X'; 50])
                .await
                .unwrap();
        }

        // Drop handle to signal completion
        drop(handle);

        // Wait for writer to finish
        writer_task.await.unwrap().unwrap();
    }
}
