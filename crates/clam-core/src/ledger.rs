//! Append-only JSONL ledger of payments made via `pay_and_fetch`.
//!
//! Each line is one [`LedgerEntry`] serialized as JSON. The writer never
//! rewrites or rotates the file; the reader streams the whole file and filters
//! in memory (volumes for self-custody use are tiny — humans approve every
//! entry interactively, so dozens per day at most).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::config::Network;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger I/O error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("ledger entry could not be serialized: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Settled,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LedgerEntry {
    pub ts: DateTime<Utc>,
    pub url: String,
    pub method: String,
    pub amount_usdc: f64,
    pub pay_to: String,
    pub network: Network,
    pub tx_signature: Option<String>,
    pub status: PaymentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Append-only ledger. Cloning is cheap; the underlying file handle is
/// serialized via an internal mutex so concurrent appends from multiple tool
/// calls remain ordered and atomic line-by-line.
#[derive(Clone)]
pub struct Ledger {
    path: PathBuf,
    write_lock: std::sync::Arc<Mutex<()>>,
}

impl Ledger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_lock: std::sync::Arc::new(Mutex::new(())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry to the JSONL ledger, creating the file if needed.
    pub async fn append(&self, entry: &LedgerEntry) -> Result<(), LedgerError> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');

        let _guard = self.write_lock.lock().await;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|source| LedgerError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|source| LedgerError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        file.sync_all().await.map_err(|source| LedgerError::Io {
            path: self.path.display().to_string(),
            source,
        })?;
        Ok(())
    }

    /// Reads entries filtered by an optional cutoff time, returning the most
    /// recent first, capped at `limit` (default 100).
    pub async fn list(
        &self,
        limit: Option<usize>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<LedgerEntry>, LedgerError> {
        let limit = limit.unwrap_or(100);

        let file = match File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(source) => {
                return Err(LedgerError::Io {
                    path: self.path.display().to_string(),
                    source,
                })
            }
        };

        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut entries: Vec<LedgerEntry> = Vec::new();

        while let Some(line) = lines.next_line().await.map_err(|source| LedgerError::Io {
            path: self.path.display().to_string(),
            source,
        })? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<LedgerEntry>(line) {
                Ok(entry) => {
                    if since.is_none_or(|c| entry.ts >= c) {
                        entries.push(entry);
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed ledger line");
                }
            }
        }

        entries.sort_by_key(|e| std::cmp::Reverse(e.ts));
        entries.truncate(limit);
        Ok(entries)
    }
}
