use crate::engine::task::{DownloadProgress, DownloadState, DownloadTask, TaskId};
use serde::{Deserialize, Serialize};

// ─── Requests ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AddUriRequest {
    /// Primary URL + optional mirrors
    pub uris: Vec<String>,
    #[serde(default)]
    pub options: AddOptions,
}

#[derive(Debug, Default, Deserialize)]
pub struct AddOptions {
    pub out: Option<String>,
    pub dir: Option<String>,
    pub split: Option<usize>,
    pub max_connection_per_server: Option<usize>,
    pub checksum: Option<String>,
    pub max_download_limit: Option<u64>,
    pub header: Option<Vec<String>>,
    pub referer: Option<String>,
    pub sequential_download: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct TaskIdRequest {
    pub gid: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangeOptionRequest {
    pub gid: String,
    pub options: serde_json::Value,
}

// ─── Responses ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AddUriResponse {
    pub gid: String,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub gid: String,
    pub status: String,
    pub total_length: String,
    pub completed_length: String,
    pub download_speed: String,
    pub connections: u32,
    pub error_message: Option<String>,
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Serialize)]
pub struct FileInfo {
    pub index: u32,
    pub path: String,
    pub length: String,
    pub completed_length: String,
    pub selected: bool,
    pub uris: Vec<UriInfo>,
}

#[derive(Debug, Serialize)]
pub struct UriInfo {
    pub uri: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct GlobalStatResponse {
    pub download_speed: String,
    pub num_active: u32,
    pub num_waiting: u32,
    pub num_stopped: u32,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub result: &'static str,
}

impl OkResponse {
    pub fn ok() -> Self {
        Self { result: "OK" }
    }
}

// ─── Conversions ──────────────────────────────────────────────────────────────

pub fn task_to_status(task: &DownloadTask, progress: Option<&DownloadProgress>) -> StatusResponse {
    let status_str = match &task.state {
        DownloadState::Queued => "waiting",
        DownloadState::Probing => "active",
        DownloadState::Active => "active",
        DownloadState::Paused => "paused",
        DownloadState::Assembling => "active",
        DownloadState::Complete => "complete",
        DownloadState::Error(_) => "error",
    };

    let downloaded = progress.map(|p| p.downloaded_bytes).unwrap_or(0);
    let speed = progress.map(|p| p.speed_bps).unwrap_or(0);
    let conns = progress.map(|p| p.connections as u32).unwrap_or(0);

    let path = task
        .output_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    StatusResponse {
        gid: task.id.0.clone(),
        status: status_str.to_string(),
        total_length: task.total_bytes.unwrap_or(0).to_string(),
        completed_length: downloaded.to_string(),
        download_speed: speed.to_string(),
        connections: conns,
        error_message: task.error_message.clone(),
        files: vec![FileInfo {
            index: 1,
            path,
            length: task.total_bytes.unwrap_or(0).to_string(),
            completed_length: downloaded.to_string(),
            selected: true,
            uris: task
                .urls
                .iter()
                .map(|u| UriInfo {
                    uri: u.to_string(),
                    status: "used".to_string(),
                })
                .collect(),
        }],
    }
}
