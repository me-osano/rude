use crate::engine::task::DownloadTask;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

/// Persisted session — survives daemon restart.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Session {
    pub tasks: Vec<DownloadTask>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load session from disk. Returns empty session if file doesn't exist.
    pub async fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let raw = fs::read_to_string(path).await?;
        let session: Self = serde_json::from_str(&raw)?;
        tracing::info!(
            "Restored {} tasks from session {}",
            session.tasks.len(),
            path.display()
        );
        Ok(session)
    }

    /// Save session to disk atomically (write to tmp, rename).
    /// Uses spawn_blocking to avoid blocking the async runtime during serialization.
    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Serialize in a blocking task to avoid stalling the runtime
        let snapshot = self.clone();
        let raw = tokio::task::spawn_blocking(move || serde_json::to_string_pretty(&snapshot))
            .await
            .map_err(|e| anyhow::anyhow!("Serialization task panicked: {}", e))??;

        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, raw).await?;
        fs::rename(&tmp_path, path).await?;
        tracing::debug!("Session saved to {}", path.display());
        Ok(())
    }

    pub fn upsert(&mut self, task: DownloadTask) {
        if let Some(existing) = self.tasks.iter_mut().find(|t| t.id == task.id) {
            *existing = task;
        } else {
            self.tasks.push(task);
        }
    }

    pub fn remove(&mut self, id: &crate::engine::task::TaskId) {
        self.tasks.retain(|t| &t.id != id);
    }
}
