use crate::engine::chunk::ChunkMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;
use uuid::Uuid;

/// Opaque unique identifier for a download task.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lifecycle state of a download task — modelled as an explicit state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadState {
    /// Waiting for a worker slot
    Queued,
    /// Probing headers (Content-Length, Accept-Ranges, ETag)
    Probing,
    /// Actively downloading
    Active,
    /// Paused by user or scheduler
    Paused,
    /// All chunks complete, assembling file
    Assembling,
    /// Download finished successfully
    Complete,
    /// Unrecoverable error
    Error(String),
}

impl DownloadState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Error(_))
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active | Self::Probing | Self::Assembling)
    }
}

/// Integrity verification spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checksum {
    pub algorithm: ChecksumAlgo,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecksumAlgo {
    Sha256,
    Sha1,
    Md5,
}

/// Options that can be set per-task, overriding engine defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskOptions {
    /// Override max connections for this task
    pub max_connections: Option<usize>,
    /// Override split count
    pub split: Option<usize>,
    /// Sequential (streaming) mode — download chunks in order
    pub sequential: bool,
    /// Custom output filename
    pub out: Option<String>,
    /// Custom output directory
    pub dir: Option<PathBuf>,
    /// Expected checksum for integrity verification
    pub checksum: Option<Checksum>,
    /// Max download speed for this task (bytes/sec). 0 = inherit global.
    pub max_speed: u64,
    /// HTTP headers to send
    pub headers: Vec<(String, String)>,
    /// HTTP referer
    pub referer: Option<String>,
    /// User-Agent override
    pub user_agent: Option<String>,
}

/// Live snapshot of download progress (cheap to clone, sent over RPC).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub task_id: TaskId,
    pub state: DownloadState,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub connections: usize,
    pub chunk_progress: Vec<ChunkProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkProgress {
    pub index: usize,
    pub start: u64,
    pub end: u64,
    pub downloaded: u64,
    pub speed_bps: u64,
    pub worker_id: Option<usize>,
}

/// Full task descriptor — persisted to session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTask {
    pub id: TaskId,
    /// Primary URL + mirror list
    pub urls: Vec<Url>,
    pub state: DownloadState,
    pub options: TaskOptions,
    /// Final resolved output path
    pub output_path: Option<PathBuf>,
    /// Server-reported content length (None if unknown)
    pub total_bytes: Option<u64>,
    /// Whether server supports range requests
    pub accepts_ranges: bool,
    /// ETag / Last-Modified for resumption
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Chunk state (for resumption)
    pub chunk_map: Option<ChunkMap>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
}

impl DownloadTask {
    pub fn new(urls: Vec<Url>, options: TaskOptions) -> Self {
        Self {
            id: TaskId::new(),
            urls,
            state: DownloadState::Queued,
            options,
            output_path: None,
            total_bytes: None,
            accepts_ranges: false,
            etag: None,
            last_modified: None,
            chunk_map: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            error_message: None,
        }
    }

    pub fn primary_url(&self) -> &Url {
        &self.urls[0]
    }

    /// Infer filename from URL or Content-Disposition.
    pub fn infer_filename(&self) -> String {
        self.options.out.clone().unwrap_or_else(|| {
            self.primary_url()
                .path_segments()
                .and_then(|s| s.last())
                .filter(|s| !s.is_empty())
                .unwrap_or("download")
                .to_string()
        })
    }
}
