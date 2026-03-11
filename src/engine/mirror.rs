use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use url::Url;

/// Classification of HTTP errors for retry logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Fatal errors that should not be retried (403, 404, 410, etc.)
    Fatal,
    /// Retriable errors with backoff (429, 5xx network errors)
    RetriableWithBackoff,
    /// Retriable errors immediately (503)
    RetriableImmediate,
}

impl ErrorClass {
    /// Classify an HTTP status code.
    pub fn from_status(status: u16) -> Self {
        match status {
            // Client errors that indicate permanent failure
            400 | 401 | 403 | 404 | 405 | 410 | 451 => Self::Fatal,
            // Rate limiting — backoff required
            429 => Self::RetriableWithBackoff,
            // Service unavailable — often transient
            503 => Self::RetriableImmediate,
            // Other server errors — backoff
            500..=599 => Self::RetriableWithBackoff,
            // Success codes shouldn't reach here, but treat as retriable
            _ => Self::RetriableWithBackoff,
        }
    }

    /// Classify a connection/network error (no HTTP status).
    pub fn from_connection_error() -> Self {
        Self::RetriableWithBackoff
    }
}

/// Health state of a mirror with exponential backoff.
#[derive(Debug, Clone)]
pub struct MirrorState {
    pub url: Url,
    pub failures: u32,
    pub avg_speed_bps: u64,
    pub active_connections: usize,
    pub banned: bool,
    /// Last failure timestamp for backoff calculation
    pub last_failure: Option<Instant>,
    /// Current backoff duration in seconds (doubles each failure: 1, 2, 4, 8...)
    pub backoff_secs: u64,
    /// Last error class (determines retry strategy)
    pub last_error_class: Option<ErrorClass>,
}

impl MirrorState {
    pub fn new(url: Url) -> Self {
        Self {
            url,
            failures: 0,
            avg_speed_bps: 0,
            active_connections: 0,
            banned: false,
            last_failure: None,
            backoff_secs: 0,
            last_error_class: None,
        }
    }

    /// Check if this mirror is available for a new connection.
    /// Respects backoff timing for failed mirrors.
    pub fn is_available(&self) -> bool {
        if self.banned {
            return false;
        }

        // Check if we're still in backoff period
        if let Some(last_fail) = self.last_failure {
            if self.backoff_secs > 0 {
                let elapsed = last_fail.elapsed().as_secs();
                if elapsed < self.backoff_secs {
                    return false; // Still in cooldown
                }
            }
        }

        true
    }

    /// Time until this mirror exits backoff (0 if available now).
    pub fn cooldown_remaining_secs(&self) -> u64 {
        if let Some(last_fail) = self.last_failure {
            let elapsed = last_fail.elapsed().as_secs();
            self.backoff_secs.saturating_sub(elapsed)
        } else {
            0
        }
    }
}

/// Manages the set of mirrors for a single download task.
///
/// Features:
/// - Weighted round-robin selection (prefer faster mirrors)
/// - Exponential backoff per mirror on failure
/// - Error classification (fatal vs retriable)
/// - Automatic banning after threshold
#[derive(Debug, Clone)]
pub struct MirrorPool {
    inner: Arc<Mutex<Vec<MirrorState>>>,
    /// Max connections per mirror
    max_per_mirror: usize,
    /// Max failures before banning
    max_failures: u32,
}

impl MirrorPool {
    pub fn new(urls: Vec<Url>, max_per_mirror: usize) -> Self {
        let mirrors = urls.into_iter().map(MirrorState::new).collect();
        Self {
            inner: Arc::new(Mutex::new(mirrors)),
            max_per_mirror,
            max_failures: 5,
        }
    }

    /// Select the best available mirror URL for a new connection.
    ///
    /// Selection order:
    /// 1. Not banned, not in backoff, not at connection limit
    /// 2. Highest average speed
    /// 3. Fewest active connections (tie-break)
    pub async fn pick(&self) -> Option<Url> {
        let mut mirrors = self.inner.lock().await;
        let best = mirrors
            .iter_mut()
            .filter(|m| m.is_available() && m.active_connections < self.max_per_mirror)
            .max_by_key(|m| {
                // Weight: speed biased, connection-penalised
                m.avg_speed_bps.saturating_sub(m.active_connections as u64 * 100_000)
            });

        if let Some(m) = best {
            m.active_connections += 1;
            Some(m.url.clone())
        } else {
            None
        }
    }

    /// Pick an alternative mirror (for mirror switching before connection restart).
    /// Returns a different mirror than the current one if available.
    pub async fn pick_alternative(&self, current: &Url) -> Option<Url> {
        let mut mirrors = self.inner.lock().await;
        let best = mirrors
            .iter_mut()
            .filter(|m| {
                &m.url != current
                    && m.is_available()
                    && m.active_connections < self.max_per_mirror
            })
            .max_by_key(|m| m.avg_speed_bps.saturating_sub(m.active_connections as u64 * 100_000));

        if let Some(m) = best {
            m.active_connections += 1;
            Some(m.url.clone())
        } else {
            None
        }
    }

    /// Report that a connection to `url` completed successfully with this speed.
    pub async fn report_success(&self, url: &Url, speed_bps: u64) {
        let mut mirrors = self.inner.lock().await;
        if let Some(m) = mirrors.iter_mut().find(|m| &m.url == url) {
            m.active_connections = m.active_connections.saturating_sub(1);
            // Reset failure state on success
            m.failures = 0;
            m.backoff_secs = 0;
            m.last_failure = None;
            m.last_error_class = None;
            // EWMA speed update (α=0.3)
            m.avg_speed_bps = if m.avg_speed_bps == 0 {
                speed_bps
            } else {
                ((m.avg_speed_bps as f64 * 0.7) + (speed_bps as f64 * 0.3)) as u64
            };
        }
    }

    /// Report a connection failure with error classification.
    /// Applies exponential backoff and bans after threshold.
    pub async fn report_failure(&self, url: &Url) {
        self.report_failure_with_class(url, ErrorClass::RetriableWithBackoff)
            .await;
    }

    /// Report a failure with specific HTTP status code for proper classification.
    pub async fn report_failure_with_status(&self, url: &Url, status: u16) {
        let error_class = ErrorClass::from_status(status);
        self.report_failure_with_class(url, error_class).await;
    }

    /// Report a failure with a specific error class.
    pub async fn report_failure_with_class(&self, url: &Url, error_class: ErrorClass) {
        let mut mirrors = self.inner.lock().await;
        if let Some(m) = mirrors.iter_mut().find(|m| &m.url == url) {
            m.active_connections = m.active_connections.saturating_sub(1);
            m.failures += 1;
            m.last_failure = Some(Instant::now());
            m.last_error_class = Some(error_class);

            match error_class {
                ErrorClass::Fatal => {
                    // Immediately ban on fatal errors
                    tracing::warn!("Mirror {} banned due to fatal error", url);
                    m.banned = true;
                }
                ErrorClass::RetriableWithBackoff => {
                    // Exponential backoff: 1, 2, 4, 8, 16... seconds (capped at 60)
                    m.backoff_secs = (1u64 << m.failures.min(6)).min(60);
                    tracing::debug!(
                        "Mirror {} backoff: {} failures, {} sec cooldown",
                        url,
                        m.failures,
                        m.backoff_secs
                    );
                    if m.failures >= self.max_failures {
                        tracing::warn!("Mirror {} banned after {} failures", url, m.failures);
                        m.banned = true;
                    }
                }
                ErrorClass::RetriableImmediate => {
                    // No backoff, but still track failures
                    m.backoff_secs = 0;
                    if m.failures >= self.max_failures {
                        tracing::warn!("Mirror {} banned after {} failures", url, m.failures);
                        m.banned = true;
                    }
                }
            }
        }
    }

    /// Release a connection slot without reporting success or failure.
    /// Used when cancelling or switching mirrors.
    pub async fn release(&self, url: &Url) {
        let mut mirrors = self.inner.lock().await;
        if let Some(m) = mirrors.iter_mut().find(|m| &m.url == url) {
            m.active_connections = m.active_connections.saturating_sub(1);
        }
    }

    /// True if at least one mirror is still available.
    pub async fn has_available(&self) -> bool {
        self.inner.lock().await.iter().any(|m| m.is_available())
    }

    pub async fn snapshot(&self) -> Vec<MirrorState> {
        self.inner.lock().await.clone()
    }

    /// Get total active connections across all mirrors.
    pub async fn total_connections(&self) -> usize {
        self.inner
            .lock()
            .await
            .iter()
            .map(|m| m.active_connections)
            .sum()
    }
}    pub async fn snapshot(&self) -> Vec<MirrorState> {
        self.inner.lock().await.clone()
    }
}
