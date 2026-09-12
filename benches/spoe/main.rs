//! Criterion benchmark of the SPOE agent, reproducing `tests/benchmark_spoa.py` in-process.
//!
//! The agent (`spoa::serve_listener`, `IpFilter`, `Mmdb`, `MmdbWatcher`) runs on a Tokio runtime
//! inside the benchmark process; HAProxy is played by `client::Connection`, which sends real SPOP
//! frames over loopback TCP. Benchmarks:
//!
//! * `🔍 lookup/should_drop/*`: in-process reputation lookups (no network), ns per lookup;
//! * `💾 mmdb/load`: loading the database snapshot from disk;
//! * `🔁 spoe/roundtrip/1`: one connection, one outstanding NOTIFY, latency per request;
//! * `🔀 spoe/pipelined/32`: one connection, 32 NOTIFY frames in flight (`option pipelining`);
//! * `🌐 spoe/concurrent/{N}`: N connections with one outstanding request each (the Python
//!   client's model), requests per second and latency percentiles;
//! * `🔄 spoe/concurrent/db_hot_reload/{N}`: same, while the database file is touched every
//!   second so the watcher reloads it during the load.
//!
//! Environment: `BENCH_MMDB`, `BENCH_CATEGORIES`, `BENCH_IPS`, `BENCH_CONNECTIONS` (default
//! `4,32`), `BENCH_LOG_LEVEL`, `SPOA_BENCH_PROFILER=pprof` (see `profiler.rs`). Criterion flags
//! such as `--measurement-time`, `--warm-up-time` or a name filter are accepted after `--`.

mod client;
mod fixture;
mod profiler;
mod workload;

use crate::client::Connection;
use crate::fixture::{Counters, Fixture};
use crate::workload::{SplitMix64, Workload, random_ip, random_ipv4, random_ipv6};
use criterion::{BenchmarkId, Criterion, SamplingMode, Throughput};
use haproxy_spoa_ip_reputation_firehol::display::{HumanCount, HumanSize};
use haproxy_spoa_ip_reputation_firehol::mmdb::Mmdb;
use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Per-request timeout, like `--timeout` in the Python client.
const TIMEOUT: Duration = Duration::from_secs(5);
const PIPELINE_DEPTH: usize = 32;
const DEFAULT_CONNECTIONS: &[usize] = &[4, 32];
/// Latency samples kept per worker (uniform reservoir, like the Python client).
const RESERVOIR: usize = 100_000;
const TOUCH_INTERVAL: Duration = Duration::from_secs(1);

fn main() {
    let fixture = Fixture::start();
    let mut criterion = configure().configure_from_args();
    lookup_benchmarks(&mut criterion, &fixture);
    mmdb_benchmarks(&mut criterion, &fixture);
    spoe_roundtrip(&mut criterion, &fixture);
    spoe_pipelined(&mut criterion, &fixture);
    spoe_concurrent(&mut criterion, &fixture, false);
    spoe_concurrent(&mut criterion, &fixture, true);
    criterion.final_summary();
}

fn configure() -> Criterion {
    let criterion = Criterion::default();
    if std::env::var_os("SPOA_BENCH_PROFILER").is_some_and(|value| value == "pprof") {
        criterion.with_profiler(profiler::PprofProfiler::from_env())
    } else {
        criterion
    }
}

fn connections_from_env() -> Vec<usize> {
    match std::env::var("BENCH_CONNECTIONS") {
        Ok(list) if !list.trim().is_empty() => list
            .split(',')
            .map(|item| {
                item.trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .unwrap_or_else(|| panic!("BENCH_CONNECTIONS: invalid value {item:?}"))
            })
            .collect(),
        _ => DEFAULT_CONNECTIONS.to_vec(),
    }
}

// ---------------------------------------------------------------------------------------------
// In-process lookups and database loading
// ---------------------------------------------------------------------------------------------

fn lookup_benchmarks(c: &mut Criterion, fixture: &Fixture) {
    const SAMPLE: usize = 4096;
    let filter = fixture.filter();
    let mut rng = SplitMix64::new(1);
    let cases: [(&str, Vec<IpAddr>); 4] = [
        (
            "random_ipv4",
            (0..SAMPLE)
                .map(|_| IpAddr::V4(random_ipv4(&mut rng)))
                .collect(),
        ),
        (
            "random_ipv6",
            (0..SAMPLE)
                .map(|_| IpAddr::V6(random_ipv6(&mut rng)))
                .collect(),
        ),
        (
            "random_mixed",
            (0..SAMPLE).map(|_| random_ip(&mut rng)).collect(),
        ),
        (
            // 10.0.0.0/8 is listed as `unroutable` in FireHOL's bogon lists and in the synthetic
            // database, so these lookups decode a record and match a dropped category.
            "listed_10_0_0_0_8",
            (0..SAMPLE)
                .map(|_| IpAddr::V4(Ipv4Addr::from(0x0A00_0000 | (rng.next_u64() as u32 >> 8))))
                .collect(),
        ),
    ];

    let mut group = c.benchmark_group("🔍 lookup/should_drop (in-process IP reputation)");
    group.throughput(Throughput::Elements(1));
    for (name, ips) in &cases {
        group.bench_function(*name, |b| {
            let mut index = 0usize;
            b.iter(|| {
                let ip = ips[index % SAMPLE];
                index += 1;
                black_box(filter.should_drop(black_box(ip)))
            });
        });
    }
    group.finish();
}

fn mmdb_benchmarks(c: &mut Criterion, fixture: &Fixture) {
    let mut group = c.benchmark_group("💾 mmdb/load (database snapshot from disk)");
    group.sample_size(20).sampling_mode(SamplingMode::Flat);
    group.bench_function("load", |b| {
        b.iter(|| {
            let mmdb = Mmdb::new();
            mmdb.load(&fixture.mmdb_path).expect("load database");
            black_box(mmdb)
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------------------------
// SPOE over loopback TCP
// ---------------------------------------------------------------------------------------------

fn spoe_roundtrip(c: &mut Criterion, fixture: &Fixture) {
    let workload = Workload::from_env();
    let mut connection = Connection::connect(fixture.addr, 1, TIMEOUT).expect("connect to agent");
    let mut rng = SplitMix64::new(2);
    let mut sequence = 0u64;
    let mut blocked = 0u64;

    let mut group = c.benchmark_group("🔁 spoe/roundtrip (1 connection, 1 outstanding NOTIFY)");
    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::from_parameter(1), |b| {
        b.iter(|| {
            sequence += 1;
            let ip = workload.next_ip(&mut rng, sequence);
            let reply = connection.check(ip).expect("NOTIFY round trip");
            blocked += u64::from(reply.blocked);
            black_box(reply)
        });
    });
    group.finish();
    println!(
        "spoe/roundtrip: {} requests on one connection, {} answered ip_bad=1\n",
        HumanCount(sequence),
        HumanCount(blocked)
    );
}

fn spoe_pipelined(c: &mut Criterion, fixture: &Fixture) {
    let workload = Workload::from_env();
    let mut connection = Connection::connect(fixture.addr, 1, TIMEOUT).expect("connect to agent");
    let mut rng = SplitMix64::new(3);
    let mut sequence = 0u64;
    let mut batch = vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED); PIPELINE_DEPTH];

    let mut group = c.benchmark_group("🔀 spoe/pipelined (32 NOTIFY frames in flight)");
    group.throughput(Throughput::Elements(PIPELINE_DEPTH as u64));
    group.bench_function(BenchmarkId::from_parameter(PIPELINE_DEPTH), |b| {
        b.iter(|| {
            for ip in &mut batch {
                sequence += 1;
                *ip = workload.next_ip(&mut rng, sequence);
            }
            black_box(connection.check_pipelined(&batch).expect("pipelined batch"))
        });
    });
    group.finish();
}

fn spoe_concurrent(c: &mut Criterion, fixture: &Fixture, hot_reload: bool) {
    let group_name = if hot_reload {
        "🔄 spoe/concurrent/db_hot_reload (N connections, DB reloaded every second)"
    } else {
        "🌐 spoe/concurrent (N connections, 1 outstanding request each)"
    };
    let workload = Workload::from_env();
    let toucher = hot_reload.then(|| Toucher::start(fixture));

    let mut group = c.benchmark_group(group_name);
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(1));
    for connections in connections_from_env() {
        let mut workers = Workers::start(fixture.addr, connections, workload.clone())
            .expect("connect load generator");
        let counters_before = Counters::snapshot();
        let cpu_before = process_cpu_seconds();
        let touches_before = toucher.as_ref().map_or(0, Toucher::touches);
        let wall = Instant::now();

        group.bench_with_input(
            BenchmarkId::from_parameter(connections),
            &connections,
            |b, _| b.iter_custom(|iters| workers.run(iters)),
        );

        let elapsed = wall.elapsed();
        let cpu = process_cpu_seconds()
            .zip(cpu_before)
            .map(|(after, before)| after - before);
        let summary = workers.finish();
        if summary.count == 0 {
            // Filtered out by a Criterion name filter (or --list): nothing ran, nothing to report.
            continue;
        }
        let touches = toucher.as_ref().map_or(0, Toucher::touches) - touches_before;
        let counters = if hot_reload {
            wait_for_reloads(counters_before, touches)
        } else {
            Counters::snapshot()
        }
        .since(counters_before);
        summary.print(
            group_name,
            connections,
            &fixture.database,
            &workload,
            elapsed,
            cpu,
            counters,
            hot_reload.then_some(touches),
        );
    }
    group.finish();
    drop(toucher);
}

/// Gives the watcher up to `TIMEOUT` to finish the reloads triggered by the touches.
fn wait_for_reloads(before: Counters, touches: u64) -> Counters {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = Counters::snapshot();
        if now.since(before).loads >= touches || Instant::now() >= deadline {
            return now;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------------------------
// Load generator: one thread per connection, one outstanding request each
// ---------------------------------------------------------------------------------------------

/// Per-worker statistics for one `run`, merged into [`Summary`].
#[derive(Debug, Default)]
struct WorkerReport {
    count: u64,
    blocked: u64,
    errors: u64,
    first_error: Option<String>,
    tx: u64,
    rx: u64,
    total_latency: Duration,
    min_latency: Option<Duration>,
    max_latency: Duration,
    /// Uniform reservoir of individual request latencies.
    samples: Vec<Duration>,
}

struct Worker {
    connection: Connection,
    workload: Workload,
    rng: SplitMix64,
    reservoir_rng: SplitMix64,
    sequence: u64,
    report: WorkerReport,
}

impl Worker {
    fn record(&mut self, latency: Duration, blocked: bool, tx: usize, rx: usize) {
        let report = &mut self.report;
        report.count += 1;
        report.blocked += u64::from(blocked);
        report.tx += tx as u64;
        report.rx += rx as u64;
        report.total_latency += latency;
        report.min_latency = Some(report.min_latency.map_or(latency, |min| min.min(latency)));
        report.max_latency = report.max_latency.max(latency);
        if report.samples.len() < RESERVOIR {
            report.samples.push(latency);
        } else {
            let index = self.reservoir_rng.below(report.count) as usize;
            if index < RESERVOIR {
                report.samples[index] = latency;
            }
        }
    }

    /// Sends `requests` NOTIFY frames one at a time; stops at the first error like the Python
    /// worker does.
    fn run(&mut self, requests: u64) {
        for _ in 0..requests {
            self.sequence += 1;
            let ip = self.workload.next_ip(&mut self.rng, self.sequence);
            let started = Instant::now();
            match self.connection.check(ip) {
                Ok(reply) => self.record(started.elapsed(), reply.blocked, reply.tx, reply.rx),
                Err(err) => {
                    self.report.errors += 1;
                    self.report.first_error.get_or_insert_with(|| {
                        format!("{err} (request {} of worker)", self.sequence)
                    });
                    break;
                }
            }
        }
    }
}

struct Workers {
    commands: Vec<mpsc::SyncSender<u64>>,
    reports: mpsc::Receiver<WorkerReport>,
    handles: Vec<JoinHandle<()>>,
    summary: Summary,
}

impl Workers {
    fn start(addr: SocketAddr, count: usize, workload: Workload) -> client::Result<Self> {
        let (report_tx, reports) = mpsc::channel();
        let mut commands = Vec::with_capacity(count);
        let mut handles = Vec::with_capacity(count);
        for index in 0..count {
            let stream_id = index as u64 + 1;
            let connection = Connection::connect(addr, stream_id, TIMEOUT)?;
            let (command_tx, command_rx) = mpsc::sync_channel::<u64>(1);
            let report_tx = report_tx.clone();
            let mut worker = Worker {
                connection,
                workload: workload.clone(),
                rng: SplitMix64::new(0x1000 + stream_id),
                reservoir_rng: SplitMix64::new(0x2000 + stream_id),
                sequence: 0,
                report: WorkerReport::default(),
            };
            let handle = thread::Builder::new()
                .name(format!("spoe-client-{stream_id}"))
                .spawn(move || {
                    while let Ok(requests) = command_rx.recv() {
                        worker.run(requests);
                        let report = std::mem::take(&mut worker.report);
                        if report_tx.send(report).is_err() {
                            break;
                        }
                    }
                })
                .expect("spawn load generator thread");
            commands.push(command_tx);
            handles.push(handle);
        }
        Ok(Self {
            commands,
            reports,
            handles,
            summary: Summary::default(),
        })
    }

    /// Spreads `iters` requests over the connections (the Python client splits connections over
    /// workers the same way), runs them concurrently and returns the wall time of the batch.
    fn run(&mut self, iters: u64) -> Duration {
        let count = self.commands.len() as u64;
        let started = Instant::now();
        for (index, command) in self.commands.iter().enumerate() {
            let requests = iters / count + u64::from((index as u64) < iters % count);
            command
                .send(requests)
                .expect("load generator thread stopped");
        }
        for _ in 0..count {
            let report = self.reports.recv().expect("load generator thread stopped");
            self.summary.merge(report);
        }
        let elapsed = started.elapsed();
        self.summary.measured += elapsed;
        elapsed
    }

    fn finish(self) -> Summary {
        drop(self.commands);
        for handle in self.handles {
            let _ = handle.join();
        }
        self.summary
    }
}

/// Aggregate of every `Workers::run` call of one benchmark (warm-up and measurement included).
#[derive(Debug, Default)]
struct Summary {
    count: u64,
    blocked: u64,
    errors: u64,
    first_error: Option<String>,
    tx: u64,
    rx: u64,
    total_latency: Duration,
    min_latency: Option<Duration>,
    max_latency: Duration,
    /// `(latency, weight)`: each worker reservoir represents `count / samples` requests.
    weighted_samples: Vec<(Duration, f64)>,
    /// Sum of the batch wall times returned to Criterion.
    measured: Duration,
}

impl Summary {
    fn merge(&mut self, report: WorkerReport) {
        self.count += report.count;
        self.blocked += report.blocked;
        self.errors += report.errors;
        if self.first_error.is_none() {
            self.first_error = report.first_error;
        }
        self.tx += report.tx;
        self.rx += report.rx;
        self.total_latency += report.total_latency;
        if let Some(min) = report.min_latency {
            self.min_latency = Some(self.min_latency.map_or(min, |current| current.min(min)));
        }
        self.max_latency = self.max_latency.max(report.max_latency);
        if !report.samples.is_empty() {
            let weight = report.count as f64 / report.samples.len() as f64;
            self.weighted_samples
                .extend(report.samples.into_iter().map(|latency| (latency, weight)));
        }
    }

    fn percentile(&self, sorted: &[(Duration, f64)], fraction: f64) -> Duration {
        let total: f64 = sorted.iter().map(|(_, weight)| weight).sum();
        let target = fraction * total;
        let mut cumulative = 0.0;
        for (latency, weight) in sorted {
            cumulative += weight;
            if cumulative >= target {
                return *latency;
            }
        }
        sorted
            .last()
            .map_or(Duration::ZERO, |(latency, _)| *latency)
    }

    #[allow(clippy::too_many_arguments)]
    fn print(
        &self,
        group: &str,
        connections: usize,
        database: &str,
        workload: &Workload,
        elapsed: Duration,
        cpu: Option<f64>,
        counters: Counters,
        touches: Option<u64>,
    ) {
        let micros = |d: Duration| d.as_secs_f64() * 1e6;
        let mut sorted = self.weighted_samples.clone();
        sorted.sort_by_key(|(latency, _)| *latency);
        let requests_per_second = if self.measured.is_zero() {
            0.0
        } else {
            self.count as f64 / self.measured.as_secs_f64()
        };

        println!("---- {group}: {connections} connections, one outstanding request each ----");
        println!("  Database       : {database}");
        println!("  IP workload    : {}", workload.describe());
        println!(
            "  Requests       : {} successful / {} errors, over {:.1} s of measured batches ({:.1} s wall incl. Criterion warm-up and analysis)",
            HumanCount(self.count),
            HumanCount(self.errors),
            self.measured.as_secs_f64(),
            elapsed.as_secs_f64()
        );
        println!(
            "  Throughput     : {} requests/s (all batches; Criterion's thrpt above is the estimate over the measured samples)",
            HumanCount(requests_per_second.round() as u64)
        );
        println!(
            "  ip_bad = 1     : {} ({:.1}%)   ip_bad = 0: {}",
            HumanCount(self.blocked),
            if self.count == 0 {
                0.0
            } else {
                100.0 * self.blocked as f64 / self.count as f64
            },
            HumanCount(self.count - self.blocked)
        );
        println!(
            "  Agent metrics  : +{} lookups (+{} blocked, +{} allowed)",
            HumanCount(counters.allowed + counters.blocked),
            HumanCount(counters.blocked),
            HumanCount(counters.allowed)
        );
        println!(
            "  SPOE traffic   : TX {} / RX {} ({} / {} per request)",
            HumanSize(self.tx),
            HumanSize(self.rx),
            HumanSize(self.tx.checked_div(self.count).unwrap_or(0)),
            HumanSize(self.rx.checked_div(self.count).unwrap_or(0))
        );
        if self.count > 0 {
            println!(
                "  Latency (us)   : min {:.1} / mean {:.1} / p50 {:.1} / p95 {:.1} / p99 {:.1} / max {:.1}",
                micros(self.min_latency.unwrap_or_default()),
                micros(self.total_latency) / self.count as f64,
                micros(self.percentile(&sorted, 0.50)),
                micros(self.percentile(&sorted, 0.95)),
                micros(self.percentile(&sorted, 0.99)),
                micros(self.max_latency)
            );
        }
        if let Some(touches) = touches {
            println!(
                "  MMDB touches   : {touches} (one per second), reloads confirmed by the agent: {}",
                counters.loads
            );
        }
        if let Some(cpu) = cpu {
            println!(
                "  Process CPU    : {:.0}% of one core (client threads + agent runtime)",
                100.0 * cpu / elapsed.as_secs_f64().max(f64::EPSILON)
            );
        }
        if let Some(error) = &self.first_error {
            println!("  First error    : {error}");
        }
        println!();
    }
}

// ---------------------------------------------------------------------------------------------
// Hot reload: touch the database file every second while the load runs
// ---------------------------------------------------------------------------------------------

struct Toucher {
    stop: Arc<AtomicBool>,
    touches: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl Toucher {
    fn start(fixture: &Fixture) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let touches = Arc::new(AtomicU64::new(0));
        let path = Arc::clone(&fixture.mmdb_path);
        let handle = {
            let (stop, touches) = (Arc::clone(&stop), Arc::clone(&touches));
            thread::Builder::new()
                .name("mmdb-toucher".into())
                .spawn(move || {
                    let mut next = Instant::now() + TOUCH_INTERVAL;
                    while !stop.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(20));
                        if Instant::now() < next {
                            continue;
                        }
                        next = Instant::now() + TOUCH_INTERVAL;
                        match fixture::touch_database(&path) {
                            Ok(()) => {
                                touches.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(err) => {
                                eprintln!("cannot touch {}: {err}", path.display());
                                return;
                            }
                        }
                    }
                })
                .expect("spawn toucher thread")
        };
        Self {
            stop,
            touches,
            handle: Some(handle),
        }
    }

    fn touches(&self) -> u64 {
        self.touches.load(Ordering::Relaxed)
    }
}

impl Drop for Toucher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// CPU time consumed by this process (all threads), from `/proc/self/stat`.
fn process_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    // Linux reports these fields in clock ticks; sysconf(_SC_CLK_TCK) is 100 on every mainstream
    // distribution.
    Some((utime + stime) / 100.0)
}
