use async_watcher::AsyncDebouncer;
use notify::{RecommendedWatcher, RecursiveMode};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs;
use tracing::{error, info};

use crate::BUILD_VERSION;
use crate::firehol_ip_reputation::Mmdb;
use anyhow::{Context, Result};

pub struct FileWatcher {
    mmdb_path: PathBuf,
    mmdb: Arc<Mmdb>,
    last_modified_timestamp: Arc<std::sync::atomic::AtomicU64>,
}

impl FileWatcher {
    pub fn new(mmdb_path: PathBuf, mmdb: Arc<Mmdb>) -> Self {
        Self {
            mmdb_path,
            mmdb,
            last_modified_timestamp: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub async fn start(&self) -> Result<AsyncDebouncer<RecommendedWatcher>> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);

        let mut debouncer = AsyncDebouncer::new(
            std::time::Duration::from_secs(1),
            Some(std::time::Duration::from_secs(1)),
            tx,
        )
        .await?;

        let watch_path = if self.mmdb_path.exists() {
            &self.mmdb_path
        } else {
            self.mmdb_path.parent().with_context(|| {
                format!("failed to get parent directory of {:?}", self.mmdb_path)
            })?
        };

        debouncer
            .watcher()
            .watch(watch_path, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch {:?}", watch_path))?;

        info!("started watching mmdb file: {:?}", self.mmdb_path);

        let mmdb_path = self.mmdb_path.clone();
        let mmdb = Arc::clone(&self.mmdb);
        let last_modified_timestamp = Arc::clone(&self.last_modified_timestamp);

        tokio::task::spawn(async move {
            let span = tracing::info_span!(BUILD_VERSION);
            span.in_scope(async move || {
                while let Some(event) = rx.recv().await {
                    match event {
                        Ok(event) => {
                            if !event.iter().any(|e| {
                                e.event.paths.iter().any(|path| {
                                    *path == mmdb_path
                                        || (mmdb_path.file_name().is_some()
                                            && path.file_name() == mmdb_path.file_name())
                                })
                            }) || !mmdb_path.exists()
                            {
                                continue;
                            }

                            let Ok(metadata) = fs::metadata(&mmdb_path).await else {
                                continue;
                            };
                            let Ok(modified) = metadata.modified() else {
                                continue;
                            };
                            let modified = modified
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);

                            if modified
                                != last_modified_timestamp
                                    .load(std::sync::atomic::Ordering::Relaxed)
                            {
                                info!("mmdb file change detected, reloading...");
                                if let Err(err) = mmdb.load(&mmdb_path) {
                                    error!("failed to reload mmdb: {err}");
                                }
                                last_modified_timestamp
                                    .store(modified, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        Err(err) => {
                            error!("watcher error: {err:?}");
                        }
                    }
                }
                error!("watcher stopped unexpectedly");
            })
            .await;
        });

        Ok(debouncer)
    }
}
