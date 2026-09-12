//! HAProxy SPOE agent that flags client IPs listed in the FireHOL blocklists.
//!
//! The crate is split into this library and a thin binary (`src/main.rs`) so that the
//! benchmarks under `benches/` exercise exactly the code the production agent runs:
//!
//! - [`cli`]: command-line / environment configuration;
//! - [`firehol`]: git synchronisation of the blocklists, list parsing and MMDB generation;
//! - [`mmdb`] and [`mmdb_watcher`]: hot-swappable database snapshot, reloaded on file changes;
//! - [`metrics`] and [`metrics_server`]: Prometheus counters and their HTTP endpoint;
//! - [`spoa`]: the SPOE handler answering `check-client-ip` with the `ip_bad` variable.

pub mod cli;
pub mod display;
pub mod firehol;
pub mod metrics;
pub mod metrics_server;
pub mod mmdb;
pub mod mmdb_watcher;
pub mod spoa;
#[cfg(test)]
mod test_util;

use crate::cli::BUILD_VERSION;

/// Root span carrying the build version, attached to long-lived tasks so their log lines can
/// be correlated with a release. Always a root so nested tasks do not repeat the prefix.
pub fn version_span() -> tracing::Span {
    tracing::info_span!(parent: None, BUILD_VERSION)
}
