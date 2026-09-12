//! Hot-swappable, read-only view of the FireHOL MMDB database.
//!
//! The database bytes are read once into memory; the `geoip2` reader borrows them. Both live in
//! a single self-referential cell whose covariance is checked by the compiler, so no `unsafe`
//! code is needed to tie their lifetimes together. Reloads swap the whole snapshot atomically:
//! in-flight lookups keep the previous snapshot alive until they finish.

use crate::display::{EpochSeconds, HumanCount};
use crate::metrics;
use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwapOption;
use geoip2::{FireholEntry, Reader};
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime};
use tracing::{debug, info, warn};

/// `geoip2::Reader` specialised for the FireHOL record layout.
type FireholReader<'a> = Reader<'a, FireholEntry<'a>>;

self_cell::self_cell!(
    /// Database bytes together with the reader borrowing them.
    struct OwnedReader {
        owner: Box<[u8]>,
        #[covariant]
        dependent: FireholReader,
    }
);

struct Snapshot {
    reader: OwnedReader,
    /// Modification time of the file this snapshot was loaded from, used to skip redundant reloads.
    modified: Option<SystemTime>,
}

/// Thread-safe holder of the current database snapshot.
pub struct Mmdb {
    snapshot: ArcSwapOption<Snapshot>,
    /// Serialises loads so concurrent triggers (file watcher, updater) never parse the same file twice.
    load_lock: Mutex<()>,
}

impl Default for Mmdb {
    fn default() -> Self {
        Self::new()
    }
}

impl Mmdb {
    pub const fn new() -> Self {
        Self {
            snapshot: ArcSwapOption::const_empty(),
            load_lock: Mutex::new(()),
        }
    }

    /// Loads (or reloads) the database from `path`.
    pub fn load(&self, path: &Path) -> Result<()> {
        let _guard = self.lock();
        self.load_locked(path, modified_time(path).ok())
    }

    /// Reloads the database only when the file modification time differs from the loaded
    /// snapshot. Returns `Ok(true)` when a reload happened.
    pub fn reload_if_changed(&self, path: &Path) -> Result<bool> {
        let _guard = self.lock();
        let modified = modified_time(path)
            .with_context(|| format!("failed to read metadata of {}", path.display()))?;
        if self
            .snapshot
            .load()
            .as_deref()
            .is_some_and(|snapshot| snapshot.modified == Some(modified))
        {
            debug!("mmdb {} unchanged, reload skipped", path.display());
            return Ok(false);
        }
        self.load_locked(path, Some(modified))?;
        Ok(true)
    }

    /// Looks up `addr` and hands the matching record to `f` without copying any of its strings.
    ///
    /// Returns `None` when no database is loaded or when the address has no record.
    pub fn lookup<R>(&self, addr: IpAddr, f: impl FnOnce(&FireholEntry<'_>) -> R) -> Option<R> {
        let snapshot = self.snapshot.load();
        let reader = snapshot.as_deref()?.reader.borrow_dependent();
        match reader.lookup(addr) {
            Ok(entry) => Some(f(&entry)),
            Err(geoip2::Error::NotFound | geoip2::Error::IPv4Only) => None,
            Err(err) => {
                warn!("mmdb lookup failed for {addr}: {err:?}");
                None
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.load_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn load_locked(&self, path: &Path, modified: Option<SystemTime>) -> Result<()> {
        let start = Instant::now();
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .into_boxed_slice();
        let reader = OwnedReader::try_new(bytes, |bytes| {
            FireholReader::from_bytes(bytes)
                .map_err(|err| anyhow!("failed to parse mmdb {}: {err:?}", path.display()))
        })?;

        let metadata = reader.borrow_dependent().get_metadata();
        metrics::MMDB_NODE_COUNT.set(i64::from(metadata.node_count));
        info!(
            build_time = %EpochSeconds::from(metadata.build_epoch),
            database_type = metadata.database_type,
            ip_version = metadata.ip_version,
            node_count = %HumanCount::from(metadata.node_count),
            "mmdb loaded from {} in {:?}",
            path.display(),
            start.elapsed(),
        );

        self.snapshot
            .store(Some(Arc::new(Snapshot { reader, modified })));
        metrics::MMDB_FILE_LOADED
            .with_label_values(&[&path.to_string_lossy()])
            .inc();
        Ok(())
    }
}

fn modified_time(path: &Path) -> io::Result<SystemTime> {
    fs::metadata(path)?.modified()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TempDir, build_test_db};
    use std::fs::File;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    /// Copies a borrowed record array so it can leave the lookup closure.
    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn lookup_without_database_returns_none() {
        let mmdb = Mmdb::new();
        assert_eq!(mmdb.lookup(IpAddr::V4(Ipv4Addr::LOCALHOST), |_| ()), None);
    }

    #[test]
    fn lookup_exposes_merged_records_without_copies() {
        let dir = TempDir::new("mmdb-lookup");
        let path = build_test_db(dir.path());
        let mmdb = Mmdb::new();
        mmdb.load(&path).unwrap();

        let overlap = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        let (files, categories, maintainers, dates) = mmdb
            .lookup(overlap, |entry| {
                (
                    owned(&entry.file_name),
                    owned(&entry.category),
                    owned(&entry.maintainer),
                    owned(&entry.source_file_date_rfc3339),
                )
            })
            .expect("record for 10.1.2.3");
        assert_eq!(files, ["abuse.ipset", "spam.netset"]);
        assert_eq!(categories, ["abuse", "spam"]);
        assert_eq!(maintainers, ["Team A", "Team B"]);
        assert_eq!(
            dates,
            ["2026-09-10T23:59:49+00:00", "2026-08-07T10:10:14+00:00"]
        );

        let single = IpAddr::V4(Ipv4Addr::new(10, 2, 0, 1));
        assert_eq!(
            mmdb.lookup(single, |entry| owned(&entry.file_name)),
            Some(vec!["abuse.ipset".to_string()])
        );
        let host = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        assert_eq!(
            mmdb.lookup(host, |entry| owned(&entry.category)),
            Some(vec!["abuse".to_string()])
        );
        assert_eq!(
            mmdb.lookup(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), |_| ()),
            None
        );
        assert_eq!(mmdb.lookup(IpAddr::V6(Ipv6Addr::LOCALHOST), |_| ()), None);
    }

    #[test]
    fn reload_if_changed_follows_the_modification_time() {
        let dir = TempDir::new("mmdb-reload");
        let path = build_test_db(dir.path());
        let mmdb = Mmdb::new();

        assert!(mmdb.reload_if_changed(&path).unwrap(), "first call loads");
        assert!(
            !mmdb.reload_if_changed(&path).unwrap(),
            "same mtime is skipped"
        );

        let file = File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        drop(file);
        assert!(mmdb.reload_if_changed(&path).unwrap(), "new mtime reloads");
        assert!(!mmdb.reload_if_changed(&path).unwrap());

        assert!(
            mmdb.reload_if_changed(&dir.path().join("missing.mmdb"))
                .is_err()
        );
        assert_eq!(
            mmdb.lookup(IpAddr::V4(Ipv4Addr::new(10, 2, 0, 1)), |entry| entry
                .file_name
                .len()),
            Some(1),
            "a failed reload keeps the previous snapshot"
        );
    }

    #[test]
    fn corrupt_file_is_rejected_and_previous_snapshot_kept() {
        let dir = TempDir::new("mmdb-corrupt");
        let path = build_test_db(dir.path());
        let mmdb = Mmdb::new();
        mmdb.load(&path).unwrap();

        let corrupt = dir.path().join("corrupt.mmdb");
        fs::write(&corrupt, b"definitely not an mmdb file").unwrap();
        assert!(mmdb.load(&corrupt).is_err());
        assert_eq!(
            mmdb.lookup(IpAddr::V4(Ipv4Addr::new(10, 2, 0, 1)), |entry| entry
                .file_name
                .len()),
            Some(1)
        );
    }
}
