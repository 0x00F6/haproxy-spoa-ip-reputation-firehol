mod file_watcher;
mod firehol_git_repository;
mod firehol_ip_reputation;
mod haproxy_spoa_server;
mod prometheus_metrics_server;
mod utils;

use anyhow::{Context, Result};
use clap::Parser;
use file_watcher::FileWatcher;
use firehol_git_repository::FireholGitRepository;
use firehol_ip_reputation::Mmdb;
use haproxy_spoa_server::HaproxySpoaServer;
use prometheus_metrics_server::PrometheusMetricsServer;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix;
use tokio::sync::oneshot;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info};

#[derive(Parser, Debug)]
#[command(name = "haproxy-spoa-ip-reputation-firehol")]
#[command(author, version = BUILD_VERSION, about, long_about = None)]
struct Cli {
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,

    #[arg(
        long,
        env = "SPOA_LISTEN_ADRESS_METRICS_PROMETHEUS",
        default_value = "0.0.0.0:8405"
    )]
    spoa_listen_adress_metrics_prometheus: String,

    #[arg(long, env = "MMDB_PATH", default_value = "firehol.mmdb")]
    mmdb_path: String,

    #[arg(long, env = "SPOA_LISTEN_ADRESS", default_value = "0.0.0.0:9000")]
    spoa_listen_adress: String,

    #[arg(long, env = "DROP_BY_CATEGORY", value_delimiter = ',')]
    drop_by_category: Vec<String>,

    #[arg(long, env = "DROP_BY_FILE_NAMES", value_delimiter = ',')]
    drop_by_file_names: Vec<String>,

    #[arg(
        long,
        env = "FIREHOL_REPO_PATH",
        default_value = "firehol-blocklist-ipsets"
    )]
    firehol_repo_path: String,
    #[arg(
        long,
        env = "FIREHOL_REPO_URL",
        default_value = "https://github.com/firehol/blocklist-ipsets.git"
    )]
    firehol_repo_url: String,

    #[arg(
        long,
        env = "FIREHOL_IGNORE_COUNTRY",
        alias = "firehol-ignoire-country",
        default_value = "true"
    )]
    firehol_ignore_country: bool,

    #[arg(long, env = "FIREHOL_UPDATE_CRON_JOB", default_value = "@hourly")]
    firehol_update_cron_job: String,

    #[arg(long, env = "FIREHOL_REPO_BRANCH", default_value = "master")]
    firehol_repo_branch: String,
}

const BUILD_VERSION: &str = env!("BUILD_VERSION");

/// Runs the Firehol update + mmdb rebuild on a dedicated blocking thread,
/// so it never stalls the Tokio runtime (Git I/O + disk writes can be slow).
async fn run_update(repo: Arc<FireholGitRepository>, mmdb_path: String) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let _span = tracing::info_span!(BUILD_VERSION).entered();
        repo.update_and_build_mmdb(&mmdb_path)
    })
    .await
    .with_context(|| format!("{}:{} firehol update task panicked", file!(), line!()))?
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.log_level.trim().to_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "info" => tracing::Level::INFO,
        "warn" | "warning" => tracing::Level::WARN,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .with_thread_names(true)
        .init();

    let _span = tracing::info_span!(BUILD_VERSION).entered();

    info!(?cli, "configuration loaded");

    let mmdb = Arc::new(Mmdb::new());
    let mmdb_path_str = cli.mmdb_path.clone();
    let mmdb_path = Path::new(&mmdb_path_str);
    let mmdb_path = if !mmdb_path.exists() {
        info!("mmdb file {mmdb_path:?} does not exist yet");
        PathBuf::from(mmdb_path)
    } else {
        let mmdb_path = tokio::fs::canonicalize(&mmdb_path_str)
            .await
            .with_context(|| format!("failed to canonicalize mmdb path: {mmdb_path_str}"))?;
        mmdb.load(&mmdb_path)?;
        mmdb_path
    };

    let drop_categories = cli.drop_by_category.into_iter().collect::<HashSet<_>>();
    let drop_file_names = cli.drop_by_file_names.into_iter().collect::<HashSet<_>>();

    let file_watcher = FileWatcher::new(mmdb_path, Arc::clone(&mmdb));
    let _debouncer = file_watcher
        .start()
        .await
        .with_context(|| format!("{}:{} file watch error", file!(), line!()))?;

    let server = HaproxySpoaServer::new(
        Arc::clone(&mmdb),
        cli.spoa_listen_adress,
        drop_categories,
        drop_file_names,
    );

    let firehol_repo = Arc::new(FireholGitRepository::new(
        cli.firehol_repo_path,
        cli.firehol_repo_url,
        cli.firehol_ignore_country,
        cli.firehol_repo_branch,
    ));

    let prometheus_server =
        PrometheusMetricsServer::new(&cli.spoa_listen_adress_metrics_prometheus)?;
    tokio::spawn(async move {
        if let Err(err) = prometheus_server.run().await {
            error!("prometheus metrics server error: {:?}", err);
            std::process::exit(1);
        }
    });

    let (mmdb_ready_tx, mmdb_ready_rx) = oneshot::channel::<()>();
    let repo = firehol_repo.clone();
    let mmdb_path = mmdb_path_str.clone();
    tokio::spawn(async move {
        let span = tracing::info_span!(BUILD_VERSION);
        span.in_scope(async move || {
            if let Err(err) = run_update(repo, mmdb_path).await {
                error!("initial Firehol database update failed: {err:#}");
                std::process::exit(1);
            }
            if let Err(err) = mmdb_ready_tx.send(()) {
                error!("send signal error: {err:?}");
                std::process::exit(1);
            }
        })
        .await;
    });

    let mut scheduler = JobScheduler::new().await?;
    debug!(
        "scheduling automatic Firehol update: {}",
        &cli.firehol_update_cron_job
    );

    let job_repo = firehol_repo.clone();
    let job_mmdb_path = mmdb_path_str.clone();
    scheduler
        .add(Job::new_async(
            cli.firehol_update_cron_job.as_str(),
            move |_uuid, _l| {
                let repo = job_repo.clone();
                let mmdb_path = job_mmdb_path.clone();
                Box::pin(async move {
                    let result = run_update(repo, mmdb_path).await;
                    if let Err(err) = result {
                        let _span = tracing::info_span!(BUILD_VERSION);
                        error!("scheduled Firehol database update failed: {err:#}");
                    }
                })
            },
        )?)
        .await
        .with_context(|| format!("{}:{} failed to add job to scheduler", file!(), line!()))?;

    scheduler
        .start()
        .await
        .with_context(|| format!("{}:{} failed to start job scheduler", file!(), line!()))?;

    tokio::spawn(async move {
        let mut sigterm = match unix::signal(unix::SignalKind::terminate()) {
            Ok(sigterm) => sigterm,
            Err(err) => {
                error!("unix signal failed: {err:#}");
                std::process::exit(1);
            }
        };
        let mut sigquit = match unix::signal(unix::SignalKind::quit()) {
            Ok(sigquit) => sigquit,
            Err(err) => {
                error!("unix signal failed: {err:#}");
                std::process::exit(1);
            }
        };
        let mut sigint = match unix::signal(unix::SignalKind::interrupt()) {
            Ok(sigint) => sigint,
            Err(err) => {
                error!("unix signal failed: {err:#}");
                std::process::exit(1);
            }
        };
        let signal = tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigquit.recv() => "SIGQUIT",
            _ = sigint.recv() => "SIGINT",
        };
        info!("received {signal}, shutting down...");
        scheduler.shutdown().await.unwrap();
        std::process::exit(0);
    });

    mmdb_ready_rx.await?;

    server
        .start()
        .await
        .with_context(|| format!("{}:{} failed to start server", file!(), line!()))?;

    Ok(())
}
