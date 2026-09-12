//! Builds the FireHOL MMDB file from parsed list files.

use super::ipset::{Ipset, Metadata};
use crate::display::HumanSize;
use anyhow::{Context, Result};
use mmdb_writer::{IpVersion, RecordSize, Value, Writer};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{info, warn};

/// `database_type` recognised by the patched `geoip2` reader (see `geoip2-rs.patch`).
const DATABASE_TYPE: &str = "Firehol-DB";

// Record field names; they must match the fields of `geoip2::FireholEntry`.
const FILE_NAME: &str = "file_name";
const SOURCE_FILE_DATE: &str = "source_file_date_rfc3339";
const LIST_SOURCE_URL: &str = "list_source_url";
const MAINTAINER_URL: &str = "maintainer_url";
const MAINTAINER: &str = "maintainer";
const CATEGORY: &str = "category";

/// Record stored for every network of a section.
///
/// Each field is a single-element array so that overlapping networks coming from different
/// lists deep-merge into parallel arrays: index `i` of every array describes the same list.
fn record(file_name: &str, metadata: &Metadata) -> Value {
    let one = |value: &str| Value::array([Value::from(value)]);
    Value::map([
        (FILE_NAME, one(file_name)),
        (SOURCE_FILE_DATE, one(&metadata.source_file_date_rfc3339)),
        (LIST_SOURCE_URL, one(&metadata.list_source_url)),
        (MAINTAINER_URL, one(&metadata.maintainer_url)),
        (MAINTAINER, one(&metadata.maintainer)),
        (CATEGORY, one(&metadata.category)),
    ])
}

/// Serialises `ipsets` into an IPv4 MMDB and atomically replaces `output` with it.
///
/// Returns the size of the written file in bytes.
pub fn write_mmdb(ipsets: &[Ipset], build_epoch: SystemTime, output: &Path) -> Result<u64> {
    let mut writer = Writer::builder(DATABASE_TYPE)
        .ip_version(IpVersion::V4)
        .record_size(RecordSize::Bits28)
        .build_epoch(build_epoch)
        .build();

    for ipset in ipsets {
        for section in &ipset.sections {
            let value = record(&ipset.file_name, &section.metadata);
            for &network in &section.networks {
                // Same semantics as `MergeStrategy::DeepMerge`, without cloning the record
                // once more per network.
                writer
                    .insert_with(network, |existing| {
                        Some(match existing {
                            Some(old) => Value::merge_deep(old, &value),
                            None => value.clone(),
                        })
                    })
                    .with_context(|| {
                        format!("failed to insert {network} from {}", ipset.file_name)
                    })?;
            }
        }
    }

    let bytes = writer.to_bytes().context("failed to serialise mmdb")?;
    let size = bytes.len() as u64;
    let temp_path = output.with_extension("tmp");
    info!(
        "writing mmdb ({}) to {}",
        HumanSize(size),
        temp_path.display()
    );
    TempFile::write(temp_path, &bytes)?.persist(output)?;
    Ok(size)
}

/// A file written in place of the final one and renamed over it on success.
///
/// The temporary file is removed on drop unless [`persist`](Self::persist) succeeded, so a failed
/// build never leaves a partial file behind.
struct TempFile {
    path: Option<PathBuf>,
}

impl TempFile {
    /// Creates `path`, writes `bytes` and flushes them to disk.
    fn write(path: PathBuf, bytes: &[u8]) -> Result<Self> {
        let mut file =
            File::create(&path).with_context(|| format!("failed to create {}", path.display()))?;
        let temp = Self { path: Some(path) };
        let path = temp.path.as_deref().expect("path set at creation");
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", path.display()))?;
        Ok(temp)
    }

    /// Atomically renames the file to `target`.
    fn persist(mut self, target: &Path) -> Result<()> {
        let path = self.path.take().expect("temp file already persisted");
        fs::rename(&path, target).map_err(|err| {
            // Best effort: never leave the temporary file behind.
            remove_quietly(&path);
            anyhow::Error::from(err).context(format!(
                "failed to rename {} to {}",
                path.display(),
                target.display()
            ))
        })
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            remove_quietly(&path);
        }
    }
}

fn remove_quietly(path: &Path) {
    if let Err(err) = fs::remove_file(path)
        && err.kind() != ErrorKind::NotFound
    {
        warn!("failed to remove temporary file {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{ABUSE_IPSET, SPAM_NETSET, TEST_BUILD_EPOCH, TempDir};
    use geoip2::{FireholEntry, Reader};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn writes_a_database_the_reader_understands() {
        let dir = TempDir::new("builder");
        let output = dir.path().join("out.mmdb");
        fs::write(&output, b"stale").unwrap();

        let ipsets = [
            super::super::ipset::parse("abuse.ipset", ABUSE_IPSET).unwrap(),
            super::super::ipset::parse("spam.netset", SPAM_NETSET).unwrap(),
        ];
        let epoch = UNIX_EPOCH + Duration::from_secs(TEST_BUILD_EPOCH);
        let size = write_mmdb(&ipsets, epoch, &output).unwrap();

        let bytes = fs::read(&output).unwrap();
        assert_eq!(bytes.len() as u64, size);
        assert!(
            !output.with_extension("tmp").exists(),
            "temp file must be gone"
        );

        let reader = Reader::<FireholEntry>::from_bytes(&bytes).unwrap();
        let metadata = reader.get_metadata();
        assert_eq!(metadata.database_type, DATABASE_TYPE);
        assert_eq!(metadata.build_epoch, TEST_BUILD_EPOCH);
        assert_eq!(metadata.ip_version, 4);
        assert_eq!(metadata.record_size, 28);

        let entry = reader.lookup("10.1.0.1".parse().unwrap()).unwrap();
        assert_eq!(entry.file_name, ["abuse.ipset", "spam.netset"]);
        assert_eq!(
            entry.list_source_url,
            ["https://a.example/list", "https://b.example/list"]
        );
        assert_eq!(
            entry.maintainer_url,
            ["https://a.example", "https://b.example"]
        );
    }

    #[test]
    fn temp_file_is_removed_when_not_persisted() {
        let dir = TempDir::new("tempfile");
        let path = dir.path().join("db.tmp");
        let temp = TempFile::write(path.clone(), b"partial").unwrap();
        assert!(path.exists());
        drop(temp);
        assert!(!path.exists());
    }

    #[test]
    fn persist_failure_removes_the_temp_file() {
        let dir = TempDir::new("tempfile-persist");
        let path = dir.path().join("db.tmp");
        let temp = TempFile::write(path.clone(), b"partial").unwrap();
        let target = dir.path().join("missing-dir").join("db.mmdb");
        assert!(temp.persist(&target).is_err());
        assert!(!path.exists());
    }
}
