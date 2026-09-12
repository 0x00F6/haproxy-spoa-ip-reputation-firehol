//! Reloads the MMDB snapshot whenever the database file changes on disk.

use crate::mmdb::Mmdb;
use anyhow::{Context, Result};
use async_watcher::notify::{self, RecommendedWatcher, RecursiveMode};
use async_watcher::{AsyncDebouncer, DebouncedEvent};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{Instrument, debug, error, info, warn};

/// Events are coalesced for this long so a rebuild (write + rename) triggers a single reload.
const DEBOUNCE: Duration = Duration::from_secs(1);

type EventBatch = Result<Vec<DebouncedEvent>, Vec<notify::Error>>;

/// Watches the directory containing the MMDB file and reloads [`Mmdb`] when the file changes.
///
/// The parent directory is watched rather than the file itself: the updater replaces the file
/// with an atomic rename, which would invalidate an inotify watch placed on the old inode. It
/// also lets the watcher pick up a database created after start-up.
pub struct MmdbWatcher {
    debouncer: AsyncDebouncer<RecommendedWatcher>,
    task: JoinHandle<()>,
}

impl MmdbWatcher {
    /// Starts watching `path` (which must be absolute) and reloading `mmdb` on change.
    pub async fn start(mmdb: Arc<Mmdb>, path: Arc<Path>) -> Result<Self> {
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .with_context(|| format!("mmdb path {} has no parent directory", path.display()))?;
        let file_name: OsString = path
            .file_name()
            .with_context(|| format!("mmdb path {} has no file name", path.display()))?
            .to_owned();

        let (tx, rx) = mpsc::channel(16);
        let mut debouncer = AsyncDebouncer::new(DEBOUNCE, Some(DEBOUNCE), tx)
            .await
            .context("failed to create file watcher")?;
        debouncer
            .watcher()
            .watch(dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch {}", dir.display()))?;
        info!("watching {} for changes", path.display());

        let task =
            tokio::spawn(watch_loop(rx, mmdb, path, file_name).instrument(crate::version_span()));
        Ok(Self { debouncer, task })
    }

    /// Stops watching and waits for the background task to finish.
    pub async fn stop(self) {
        self.debouncer.stop().await;
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn watch_loop(
    mut rx: mpsc::Receiver<EventBatch>,
    mmdb: Arc<Mmdb>,
    path: Arc<Path>,
    file_name: OsString,
) {
    while let Some(batch) = rx.recv().await {
        let events = match batch {
            Ok(events) => events,
            Err(errors) => {
                for err in errors {
                    error!("file watcher error: {err}");
                }
                continue;
            }
        };
        if !events
            .iter()
            .any(|event| event.path.file_name() == Some(file_name.as_os_str()))
        {
            continue;
        }

        debug!("change detected on {}", path.display());
        let (mmdb, path) = (Arc::clone(&mmdb), Arc::clone(&path));
        let reload = tokio::task::spawn_blocking(move || {
            let _span = crate::version_span().entered();
            mmdb.reload_if_changed(&path)
        });
        match reload.await {
            // A successful load logs its own summary line.
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => debug!("mmdb already up to date"),
            Ok(Err(err)) => error!("failed to reload mmdb: {err:#}"),
            Err(err) => error!("mmdb reload task failed: {err}"),
        }
    }
    warn!("mmdb file watcher stopped");
}
