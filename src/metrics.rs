//! Prometheus metrics exported by the agent.
//!
//! Metrics are registered lazily on first use; [`init`] forces registration at start-up so they
//! are exported (with zero values) before the first request or database load.

use prometheus::{
    IntCounter, IntCounterVec, IntGauge, register_int_counter, register_int_counter_vec,
    register_int_gauge,
};
use std::sync::LazyLock;

/// Requests whose IP was blocked, by list file, maintainer and category.
pub static IP_BLOCKED_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "firehol_ip_blocked_requests",
        "number of blocked ip requests",
        &["file_name", "maintainer", "category"]
    )
    .expect("firehol_ip_blocked_requests registration")
});

/// Requests whose IP was allowed.
pub static IP_ALLOWED_REQUESTS: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "firehol_ip_allowed_requests",
        "number of allowed ip requests"
    )
    .expect("firehol_ip_allowed_requests registration")
});

/// Search-tree node count of the currently loaded database.
pub static MMDB_NODE_COUNT: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "firehol_mmdb_node_count",
        "number of nodes in the loaded mmdb search tree"
    )
    .expect("firehol_mmdb_node_count registration")
});

/// Successful database loads, by file path.
pub static MMDB_FILE_LOADED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "firehol_mmdb_file_loaded",
        "number of successful mmdb loads",
        &["file"]
    )
    .expect("firehol_mmdb_file_loaded registration")
});

/// Registers every metric with the default registry.
pub fn init() {
    LazyLock::force(&IP_BLOCKED_REQUESTS);
    LazyLock::force(&IP_ALLOWED_REQUESTS);
    LazyLock::force(&MMDB_NODE_COUNT);
    LazyLock::force(&MMDB_FILE_LOADED);
}
