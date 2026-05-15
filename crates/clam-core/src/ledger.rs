//! Append-only JSONL ledger of payments made via `pay_and_fetch`.
//!
//! Each line is one [`LedgerEntry`] serialized as JSON. The writer never
//! rewrites or rotates the file; the reader streams the whole file and filters
//! in memory (volumes for self-custody use are tiny — humans approve every
//! entry interactively, so dozens per day at most).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
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
        file.flush().await.map_err(|source| LedgerError::Io {
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
        
        // Use a min-heap to keep the top `limit` most recent entries.
        // We order by ts ascending so the oldest (smallest) is at the top.
        // Wait, BinaryHeap is a max-heap.
        // We want to keep the N largest elements.
        // If we use Reverse(ts), the smallest ts (oldest) will be at the root (max).
        // Then we can pop the oldest when size > limit.
        #[derive(Debug)]
        struct HeapEntry(LedgerEntry);
        
        impl PartialEq for HeapEntry {
            fn eq(&self, other: &Self) -> bool {
                self.0.ts == other.0.ts
            }
        }
        impl Eq for HeapEntry {}
        impl PartialOrd for HeapEntry {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for HeapEntry {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                // Reverse ordering so the oldest is the "max" and gets popped first
                other.0.ts.cmp(&self.0.ts)
            }
        }
        
        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::with_capacity(limit + 1);

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
                        heap.push(HeapEntry(entry));
                        if heap.len() > limit {
                            heap.pop();
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed ledger line");
                }
            }
        }

        let mut entries: Vec<LedgerEntry> = heap.into_iter().map(|e| e.0).collect();
        // Since we popped the oldest, the remaining are the most recent.
        // BinaryHeap into_iter does not guarantee sorted order, so sort them.
        entries.sort_by_key(|e| Reverse(e.ts));
        Ok(entries)
    }
}
