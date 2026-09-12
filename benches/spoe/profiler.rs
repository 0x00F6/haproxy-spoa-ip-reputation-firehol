//! In-process sampling profiler for Criterion's `--profile-time` mode, used when `perf` is not
//! available to `cargo flamegraph` (e.g. `kernel.perf_event_paranoid` above 2 and no root).
//! Enabled with `SPOA_BENCH_PROFILER=pprof`; writes `flamegraph.svg` next to Criterion's data and
//! to `SPOA_BENCH_FLAMEGRAPH` when that variable names a file.

use criterion::profiler::Profiler;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

const SAMPLING_HZ: i32 = 997;

pub struct PprofProfiler {
    guard: Option<pprof::ProfilerGuard<'static>>,
    copy_to: Option<PathBuf>,
}

impl PprofProfiler {
    pub fn from_env() -> Self {
        Self {
            guard: None,
            copy_to: std::env::var_os("SPOA_BENCH_FLAMEGRAPH").map(PathBuf::from),
        }
    }
}

impl Profiler for PprofProfiler {
    fn start_profiling(&mut self, benchmark_id: &str, _benchmark_dir: &Path) {
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(SAMPLING_HZ)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("start pprof sampling");
        eprintln!("pprof: sampling {benchmark_id} at {SAMPLING_HZ} Hz");
        self.guard = Some(guard);
    }

    fn stop_profiling(&mut self, benchmark_id: &str, benchmark_dir: &Path) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        let report = guard.report().build().expect("build pprof report");
        drop(guard);
        fs::create_dir_all(benchmark_dir).expect("create profile directory");
        let svg = benchmark_dir.join("flamegraph.svg");
        report
            .flamegraph(File::create(&svg).expect("create flamegraph.svg"))
            .expect("write flamegraph.svg");
        eprintln!(
            "pprof: {benchmark_id} flamegraph written to {}",
            svg.display()
        );
        if let Some(copy) = &self.copy_to {
            if let Some(parent) = copy.parent() {
                fs::create_dir_all(parent).expect("create flamegraph output directory");
            }
            fs::copy(&svg, copy).expect("copy flamegraph.svg");
            eprintln!("pprof: copied to {}", copy.display());
        }
    }
}
