//! Command-line / environment configuration.

use clap::Parser;
use std::path::PathBuf;
use tracing::Level;

/// Version string produced by `build.rs` (git tag, short commit hash or crate version).
pub const BUILD_VERSION: &str = env!("BUILD_VERSION");

#[derive(Parser, Debug)]
#[command(name = "haproxy-spoa-ip-reputation-firehol")]
#[command(author, version = BUILD_VERSION, about, long_about = None)]
pub struct Cli {
    /// Log verbosity: error, warn, info, debug or trace (unknown values fall back to info).
    #[arg(long, env = "LOG_LEVEL", default_value = "info", value_parser = parse_log_level)]
    pub log_level: Level,

    /// Listen address of the Prometheus metrics HTTP endpoint.
    #[arg(
        long,
        env = "SPOA_LISTEN_ADRESS_METRICS_PROMETHEUS",
        default_value = "0.0.0.0:8405"
    )]
    pub spoa_listen_adress_metrics_prometheus: String,

    /// Path of the FireHOL MMDB database (created by the updater if missing).
    #[arg(long, env = "MMDB_PATH", default_value = "firehol.mmdb")]
    pub mmdb_path: PathBuf,

    /// Listen address of the SPOE agent.
    #[arg(long, env = "SPOA_LISTEN_ADRESS", default_value = "0.0.0.0:9000")]
    pub spoa_listen_adress: String,

    /// Comma-separated FireHOL categories whose IPs are dropped (e.g. `abuse,attacks`).
    #[arg(long, env = "DROP_BY_CATEGORY", value_delimiter = ',')]
    pub drop_by_category: Vec<String>,

    /// Comma-separated FireHOL list file names whose IPs are dropped (e.g. `abuseipdb_1d.ipset`).
    #[arg(long, env = "DROP_BY_FILE_NAMES", value_delimiter = ',')]
    pub drop_by_file_names: Vec<String>,

    /// Local checkout of the FireHOL blocklist repository.
    #[arg(
        long,
        env = "FIREHOL_REPO_PATH",
        default_value = "firehol-blocklist-ipsets"
    )]
    pub firehol_repo_path: PathBuf,

    /// Git URL of the FireHOL blocklist repository.
    #[arg(
        long,
        env = "FIREHOL_REPO_URL",
        default_value = "https://github.com/firehol/blocklist-ipsets.git"
    )]
    pub firehol_repo_url: String,

    /// Skip the per-country lists (`*_country/` directories).
    #[arg(
        long,
        env = "FIREHOL_IGNORE_COUNTRY",
        alias = "firehol-ignoire-country",
        default_value = "true"
    )]
    pub firehol_ignore_country: bool,

    /// Cron expression (or `@hourly`, `@daily`, ...) of the automatic update.
    #[arg(long, env = "FIREHOL_UPDATE_CRON_JOB", default_value = "@hourly")]
    pub firehol_update_cron_job: String,

    /// Branch of the FireHOL repository to follow.
    #[arg(long, env = "FIREHOL_REPO_BRANCH", default_value = "master")]
    pub firehol_repo_branch: String,
}

/// Lenient level parser: unknown values fall back to `info` instead of aborting start-up.
fn parse_log_level(value: &str) -> Result<Level, String> {
    Ok(match value.trim().to_ascii_lowercase().as_str() {
        "error" => Level::ERROR,
        "warn" | "warning" => Level::WARN,
        "debug" => Level::DEBUG,
        "trace" => Level::TRACE,
        _ => Level::INFO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_parsing_is_case_insensitive_and_lenient() {
        assert_eq!(parse_log_level("DEBUG"), Ok(Level::DEBUG));
        assert_eq!(parse_log_level(" warning "), Ok(Level::WARN));
        assert_eq!(parse_log_level("trace"), Ok(Level::TRACE));
        assert_eq!(parse_log_level("error"), Ok(Level::ERROR));
        assert_eq!(parse_log_level("bogus"), Ok(Level::INFO));
    }
}
