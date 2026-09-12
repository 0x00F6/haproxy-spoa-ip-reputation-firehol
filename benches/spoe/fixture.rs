//! In-process agent for the benchmark: database (real or synthetic), `Mmdb` snapshot, SPOE
//! listener on an ephemeral loopback port, file watcher and Prometheus counters. This is the
//! same code path as the `haproxy-spoa-ip-reputation-firehol` binary, minus the FireHOL git
//! synchronisation.

use crate::workload::SplitMix64;
use haproxy_spoa_ip_reputation_firehol::firehol::{builder, ipset};
use haproxy_spoa_ip_reputation_firehol::metrics;
use haproxy_spoa_ip_reputation_firehol::mmdb::Mmdb;
use haproxy_spoa_ip_reputation_firehol::mmdb_watcher::MmdbWatcher;
use haproxy_spoa_ip_reputation_firehol::spoa::{self, IpFilter};
use prometheus::core::Collector;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_CATEGORIES: &str = "unroutable,abuse";
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

/// Cumulative agent metrics, read directly from the Prometheus registry (the Python client
/// scrapes the same counters over HTTP).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub allowed: u64,
    pub blocked: u64,
    pub loads: u64,
}

impl Counters {
    pub fn snapshot() -> Self {
        Self {
            allowed: metrics::IP_ALLOWED_REQUESTS.get(),
            blocked: counter_vec_total(&*metrics::IP_BLOCKED_REQUESTS),
            loads: counter_vec_total(&*metrics::MMDB_FILE_LOADED),
        }
    }

    pub fn since(self, before: Self) -> Self {
        Self {
            allowed: self.allowed - before.allowed,
            blocked: self.blocked - before.blocked,
            loads: self.loads - before.loads,
        }
    }
}

fn counter_vec_total(collector: &dyn Collector) -> u64 {
    collector
        .collect()
        .iter()
        .flat_map(|family| family.get_metric())
        .map(|metric| metric.get_counter().get_value())
        .sum::<f64>()
        .round() as u64
}

pub struct Fixture {
    runtime: Option<tokio::runtime::Runtime>,
    watcher: Option<MmdbWatcher>,
    temp_dir: Option<PathBuf>,
    /// Loopback address of the SPOE listener.
    pub addr: SocketAddr,
    pub mmdb: Arc<Mmdb>,
    pub mmdb_path: Arc<Path>,
    /// Human-readable description of the database in use.
    pub database: String,
    pub categories: Vec<String>,
}

impl Fixture {
    /// Loads the database and starts the agent. Environment variables:
    /// `BENCH_MMDB` (database file, default `./firehol.mmdb` when present, otherwise a synthetic
    /// database is generated), `BENCH_CATEGORIES` (default `unroutable,abuse`) and
    /// `BENCH_LOG_LEVEL` (default `error`).
    pub fn start() -> Self {
        init_tracing();
        metrics::init();

        let categories: Vec<String> = std::env::var("BENCH_CATEGORIES")
            .unwrap_or_else(|_| DEFAULT_CATEGORIES.to_owned())
            .split(',')
            .map(str::trim)
            .filter(|category| !category.is_empty())
            .map(str::to_owned)
            .collect();

        let (mmdb_path, temp_dir, database) = locate_or_build_database();
        let mmdb_path: Arc<Path> = std::path::absolute(&mmdb_path)
            .expect("absolute mmdb path")
            .into();
        let mmdb = Arc::new(Mmdb::new());
        mmdb.load(&mmdb_path)
            .unwrap_or_else(|err| panic!("failed to load {}: {err:#}", mmdb_path.display()));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("agent-worker")
            .build()
            .expect("tokio runtime");
        let listener = runtime
            .block_on(spoa::bind("127.0.0.1:0"))
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let filter = IpFilter::new(
            Arc::clone(&mmdb),
            categories.iter().cloned().collect(),
            HashSet::new(),
        );
        runtime.spawn(spoa::serve_listener(listener, filter));
        let watcher = runtime
            .block_on(MmdbWatcher::start(
                Arc::clone(&mmdb),
                Arc::clone(&mmdb_path),
            ))
            .expect("start mmdb watcher");

        eprintln!(
            "spoe benchmark: agent on {addr}, database {database} ({} tree nodes), dropped categories: {}",
            metrics::MMDB_NODE_COUNT.get(),
            categories.join(",")
        );

        Self {
            runtime: Some(runtime),
            watcher: Some(watcher),
            temp_dir,
            addr,
            mmdb,
            mmdb_path,
            database,
            categories,
        }
    }

    /// A filter identical to the one served by the agent, for in-process lookups.
    pub fn filter(&self) -> IpFilter {
        IpFilter::new(
            Arc::clone(&self.mmdb),
            self.categories.iter().cloned().collect(),
            HashSet::new(),
        )
    }
}

/// Updates the database mtime, like `os.utime(path, None)` in the Python benchmark, so the
/// watcher reloads it.
pub fn touch_database(path: &Path) -> io::Result<()> {
    File::options()
        .write(true)
        .open(path)?
        .set_modified(SystemTime::now())
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            if let Some(watcher) = self.watcher.take() {
                runtime.block_on(watcher.stop());
            }
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        }
        if let Some(dir) = self.temp_dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn init_tracing() {
    let level = std::env::var("BENCH_LOG_LEVEL")
        .ok()
        .and_then(|value| value.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::ERROR);
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Returns `(path, temp dir to remove, description)`.
fn locate_or_build_database() -> (PathBuf, Option<PathBuf>, String) {
    if let Some(path) = std::env::var_os("BENCH_MMDB").map(PathBuf::from) {
        assert!(
            path.is_file(),
            "BENCH_MMDB={} is not a file",
            path.display()
        );
        return (
            path.clone(),
            None,
            format!("{} (BENCH_MMDB)", path.display()),
        );
    }
    let local = PathBuf::from("firehol.mmdb");
    if local.is_file() {
        return (
            local.clone(),
            None,
            format!("{} (working directory)", local.display()),
        );
    }
    let dir = std::env::temp_dir().join(format!("spoa-bench-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("synthetic.mmdb");
    let networks = synthetic_database(&path);
    (
        path,
        Some(dir),
        format!("synthetic, {networks} networks in categories unroutable/abuse/attacks"),
    )
}

/// Builds a deterministic database with the project's own parser and writer. `unroutable`
/// covers the reserved IPv4 ranges (about 15% of the address space, so random IPv4 clients are
/// regularly blocked), `abuse` random hosts and /24 networks, `attacks` random /20 networks that
/// are found but not dropped by the default categories.
fn synthetic_database(path: &Path) -> usize {
    const UNROUTABLE: &str = "0.0.0.0/8\n10.0.0.0/8\n100.64.0.0/10\n127.0.0.0/8\n169.254.0.0/16\n\
172.16.0.0/12\n192.0.0.0/24\n192.0.2.0/24\n192.168.0.0/16\n198.18.0.0/15\n198.51.100.0/24\n\
203.0.113.0/24\n224.0.0.0/4\n240.0.0.0/4\n";
    let mut rng = SplitMix64::new(0x5EED_F1EE);
    let mut random_networks = |count: usize, prefix: u32| -> String {
        let mask = u32::MAX << (32 - prefix);
        let mut seen = HashSet::with_capacity(count);
        let mut text = String::with_capacity(count * 19);
        while seen.len() < count {
            let address = (rng.next_u64() as u32) & mask;
            if seen.insert(address) {
                let ip = std::net::Ipv4Addr::from(address);
                if prefix == 32 {
                    text.push_str(&format!("{ip}\n"));
                } else {
                    text.push_str(&format!("{ip}/{prefix}\n"));
                }
            }
        }
        text
    };
    let lists = [
        (
            "synthetic_unroutable.netset",
            "unroutable",
            UNROUTABLE.to_owned(),
        ),
        (
            "synthetic_abuse.ipset",
            "abuse",
            random_networks(100_000, 32),
        ),
        (
            "synthetic_abuse_nets.netset",
            "abuse",
            random_networks(20_000, 24),
        ),
        (
            "synthetic_attacks.netset",
            "attacks",
            random_networks(20_000, 20),
        ),
    ];
    let ipsets: Vec<ipset::Ipset> = lists
        .iter()
        .map(|(file_name, category, entries)| {
            let content = format!(
                "# Maintainer      : spoa-bench\n# Maintainer URL  : https://example.invalid\n\
# List source URL : https://example.invalid/{file_name}\n\
# Source File Date: Thu Sep 10 23:59:49 UTC 2026\n#\n# Category        : {category}\n#\n{entries}"
            );
            ipset::parse(file_name, &content).expect("synthetic ipset")
        })
        .collect();
    let networks = ipsets.iter().map(ipset::Ipset::network_count).sum();
    builder::write_mmdb(
        &ipsets,
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        path,
    )
    .expect("write synthetic mmdb");
    networks
}
