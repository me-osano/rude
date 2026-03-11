use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level engine configuration.
/// Serialises to/from `~/.config/rude/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    // ── Download limits ────────────────────────────────────────────────────
    /// Max simultaneous download tasks (aria2: max-concurrent-downloads)
    pub max_concurrent_downloads: usize,

    /// Max HTTP connections per server (aria2: max-connection-per-server)
    pub max_connections_per_server: usize,

    /// Max total connections across all tasks
    pub max_total_connections: usize,

    // ── Chunking / splitting ───────────────────────────────────────────────
    /// Number of chunks to split each file into (aria2: split)
    pub split: usize,

    /// Minimum file size to split (bytes). Below this, single-connection.
    pub min_split_size: u64,

    /// Chunk size for reads / range requests (bytes)
    pub chunk_size: u64,

    // ── Surge-style adaptive workers ──────────────────────────────────────
    /// Enable adaptive worker management (Surge: restarts slow connections)
    pub adaptive_workers: bool,

    /// Speed threshold: if a worker's speed drops below this fraction of the
    /// median worker speed, it is restarted (e.g. 0.3 = 30%)
    pub slow_worker_threshold: f64,

    /// Enable work stealing: fast idle workers take chunks from slow peers
    pub work_stealing: bool,

    // ── Retry / resilience ────────────────────────────────────────────────
    /// Max retries per chunk before marking the task as failed
    pub max_retries: u32,

    /// Base retry delay (ms); uses exponential backoff
    pub retry_delay_ms: u64,

    // ── Output ────────────────────────────────────────────────────────────
    /// Default download directory
    pub download_dir: PathBuf,

    /// Allocate full file size before downloading (fallocate)
    pub file_allocation: FileAllocation,

    // ── Session ───────────────────────────────────────────────────────────
    /// Path to session file for crash recovery (aria2: save-session)
    pub session_file: Option<PathBuf>,

    /// Save session interval (seconds). 0 = only on clean shutdown.
    pub save_session_interval: u64,

    // ── RPC server ────────────────────────────────────────────────────────
    pub rpc: RpcConfig,

    // ── Bandwidth throttle ────────────────────────────────────────────────
    /// Global download speed cap (bytes/sec). 0 = unlimited.
    pub max_download_speed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FileAllocation {
    None,
    #[default]
    Prealloc,
    Falloc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// Shared secret token for authentication
    pub secret: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_downloads: 5,
            max_connections_per_server: 8,
            max_total_connections: 32,
            split: 8,
            min_split_size: 20 * 1024 * 1024, // 20 MiB
            chunk_size: 4 * 1024 * 1024,       // 4 MiB
            adaptive_workers: true,
            slow_worker_threshold: 0.3,
            work_stealing: true,
            max_retries: 5,
            retry_delay_ms: 1000,
            download_dir: dirs::download_dir()
                .unwrap_or_else(|| PathBuf::from(".")),
            file_allocation: FileAllocation::Prealloc,
            session_file: dirs::config_dir()
                .map(|p| p.join("rude/session.json")),
            save_session_interval: 60,
            rpc: RpcConfig::default(),
            max_download_speed: 0,
        }
    }
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 16800,
            secret: None,
        }
    }
}

impl EngineConfig {
    /// Load config from the standard path, falling back to defaults.
    pub fn load() -> anyhow::Result<Self> {
        let path = dirs::config_dir()
            .map(|p| p.join("rude/config.toml"))
            .unwrap_or_else(|| PathBuf::from("rude.toml"));

        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let cfg: Self = toml::from_str(&raw)?;
            tracing::info!("Loaded config from {}", path.display());
            Ok(cfg)
        } else {
            tracing::info!("No config file found, using defaults");
            Ok(Self::default())
        }
    }

    /// Persist current config to disk.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = dirs::config_dir()
            .map(|p| p.join("rude/config.toml"))
            .unwrap_or_else(|| PathBuf::from("rude.toml"));

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw)?;
        Ok(())
    }
}
