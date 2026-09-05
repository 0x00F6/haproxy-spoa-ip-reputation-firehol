use arc_swap::ArcSwapOption;
use geoip2::{FireholEntry, Reader};
use std::marker::PhantomData;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};
use yoke::Yoke;

use crate::utils::pretty_number;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use lazy_static::lazy_static;
use prometheus::{IntCounterVec, IntGauge, register_int_counter_vec, register_int_gauge};

lazy_static! {
    pub static ref FIREHOL_MMDB_NODE_COUNT: IntGauge =
        register_int_gauge!("firehol_mmdb_node_count", "number of blocked ip requests").unwrap();
    pub static ref FIREHOL_MMDB_FILE_LOADED: IntCounterVec =
        register_int_counter_vec!("firehol_mmdb_file_loaded", "number of loadeds", &["file"])
            .unwrap();
}

pub(crate) struct FireholReader<'a> {
    reader: Reader<'a, FireholEntry<'a>>,
    _marker: PhantomData<&'a ()>,
}

unsafe impl<'a> yoke::Yokeable<'a> for FireholReader<'static> {
    type Output = FireholReader<'a>;
    fn transform(&'a self) -> &'a FireholReader<'a> {
        self
    }
    fn transform_owned(self) -> FireholReader<'a> {
        self
    }
    unsafe fn make(from: FireholReader<'a>) -> Self {
        unsafe { std::mem::transmute::<FireholReader<'a>, FireholReader<'static>>(from) }
    }
    #[allow(clippy::unnecessary_cast)]
    fn transform_mut<F>(&'a mut self, f: F)
    where
        F: 'static + FnOnce(&'a mut Self::Output),
    {
        f(unsafe { &mut *(self as *mut Self as *mut Self::Output) });
    }
}

pub type ReaderWithYoke = Yoke<FireholReader<'static>, Box<[u8]>>;

#[derive(Clone, Debug)]
pub struct OwnedFireholEntry {
    pub file_name: Vec<String>,
    pub source_file_date_rfc3339: Vec<String>,
    pub maintainer: Vec<String>,
    pub category: Vec<String>,
}

impl From<FireholEntry<'_>> for OwnedFireholEntry {
    fn from(entry: FireholEntry<'_>) -> Self {
        Self {
            file_name: entry.file_name.iter().map(|s| s.to_string()).collect(),
            source_file_date_rfc3339: entry
                .source_file_date_rfc3339
                .iter()
                .map(|s| s.to_string())
                .collect(),
            maintainer: entry.maintainer.iter().map(|s| s.to_string()).collect(),
            category: entry.category.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Hot-swappable reader for a Firehol IP-reputation MMDB database.
///
/// `load` can be called repeatedly (e.g. on a periodic refresh) to swap in a
/// new database version without interrupting in-flight lookups.
pub struct Mmdb {
    reader: ArcSwapOption<ReaderWithYoke>,
}

impl Default for Mmdb {
    fn default() -> Self {
        Self::new()
    }
}

impl Mmdb {
    pub fn new() -> Self {
        Self {
            reader: ArcSwapOption::const_empty(),
        }
    }

    /// Loads (or reloads) the database from `mmdb_path`.
    pub fn load(&self, mmdb_path: &Path) -> Result<()> {
        FIREHOL_MMDB_FILE_LOADED
            .with_label_values(&[mmdb_path.to_string_lossy()])
            .inc();

        let start = Instant::now();
        let data =
            std::fs::read(mmdb_path).with_context(|| format!("failed to read {mmdb_path:?}"))?;

        let yoke_reader = Yoke::try_attach_to_cart(data.into_boxed_slice(), |cart| {
            let static_data: &'static [u8] = unsafe { std::mem::transmute(cart) };
            let reader = Reader::<'_, FireholEntry<'_>>::from_bytes(static_data)
                .map_err(|e| anyhow::anyhow!("failed to parse mmdb data: {e:?}"))?;
            Ok::<_, anyhow::Error>(FireholReader {
                reader,
                _marker: PhantomData,
            })
        })?;

        let elapsed = start.elapsed();
        let firehol_reader: &FireholReader = yoke_reader.get();
        let metadata = firehol_reader.reader.get_metadata();

        if let Some(datetime) = DateTime::<Utc>::from_timestamp(metadata.build_epoch as i64, 0) {
            let datetime = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
            FIREHOL_MMDB_NODE_COUNT.set(metadata.node_count as i64);

            info!(
                datetime = ?datetime,
                database_type = ?metadata.database_type,
                ip_version = ?metadata.ip_version,
                node_count = ?pretty_number(metadata.node_count),
                "mmdb loaded from {mmdb_path:?} in {elapsed:?}",
            )
        }

        self.reader.store(Some(Arc::new(yoke_reader)));
        debug!("mmdb reader stored in static variable");

        Ok(())
    }

    pub fn lookup(&self, addr: IpAddr) -> Option<OwnedFireholEntry> {
        let reader_guard = self.reader.load();
        let yoke_reader = reader_guard.as_ref()?;
        let firehol_reader: &FireholReader = yoke_reader.get();
        match firehol_reader.reader.lookup(addr) {
            Ok(entry) => Some(OwnedFireholEntry::from(entry)),
            Err(geoip2::Error::NotFound | geoip2::Error::IPv4Only) => None,
            Err(err) => {
                warn!("lookup_ip: error for ip {addr}: {err:?}");
                None
            }
        }
    }
}
