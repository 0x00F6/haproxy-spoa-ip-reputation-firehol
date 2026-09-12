//! Helpers shared by unit tests.

use crate::firehol::{builder, ipset};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

/// A unique directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "haproxy-spoa-firehol-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub const TEST_BUILD_EPOCH: u64 = 1_700_000_000;

/// `abuse.ipset`: category `abuse`, covers 10.0.0.0/8 and 192.0.2.1.
pub const ABUSE_IPSET: &str = "\
# Maintainer      : Team A
# Maintainer URL  : https://a.example
# List source URL : https://a.example/list
# Source File Date: Thu Sep 10 23:59:49 UTC 2026
#
# Category        : abuse
#
10.0.0.0/8
192.0.2.1
";

/// `spam.netset`: category `spam`, covers 10.1.0.0/16 (inside the abuse range).
pub const SPAM_NETSET: &str = "\
# Maintainer      : Team B
# Maintainer URL  : https://b.example
# List source URL : https://b.example/list
# Source File Date: Fri Aug  7 10:10:14 UTC 2026
#
# Category        : spam
#
10.1.0.0/16
";

/// Writes a small database built from the two sample lists into `dir` and returns its path.
pub fn build_test_db(dir: &Path) -> PathBuf {
    let ipsets = [
        ipset::parse("abuse.ipset", ABUSE_IPSET).expect("parse abuse.ipset"),
        ipset::parse("spam.netset", SPAM_NETSET).expect("parse spam.netset"),
    ];
    let path = dir.join("test.mmdb");
    let epoch = UNIX_EPOCH + Duration::from_secs(TEST_BUILD_EPOCH);
    builder::write_mmdb(&ipsets, epoch, &path).expect("write test mmdb");
    path
}
