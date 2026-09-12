//! Binary entry point of the HAProxy SPOE agent; the implementation lives in the library crate.
//!
//! Start-up sequence:
//! 1. load the existing MMDB (if any) and start watching it for changes;
//! 2. expose Prometheus metrics;
//! 3. synchronise the FireHOL repository and (re)build the MMDB, then schedule periodic updates;
//! 4. serve the SPOE protocol until a termination signal arrives.

use anyhow::{Context, Result};
use clap::Parser;
use haproxy_spoa_ip_reputation_firehol::cli::Cli;
use haproxy_spoa_ip_reputation_firehol::firehol::FireholUpdater;
use haproxy_spoa_ip_reputation_firehol::metrics;
use haproxy_spoa_ip_reputation_firehol::metrics_server::MetricsServer;
use haproxy_spoa_ip_reputation_firehol::mmdb::Mmdb;
use haproxy_spoa_ip_reputation_firehol::mmdb_watcher::MmdbWatcher;
use haproxy_spoa_ip_reputation_firehol::spoa::{self, IpFilter};
use haproxy_spoa_ip_reputation_firehol::version_span;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{Instrument, error, info};

/// Grace period for in-flight blocking work (e.g. an mmdb rebuild) when shutting down.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(cli.log_level)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .with_thread_names(true)
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the tokio runtime")?;
    let result = runtime.block_on(run(cli).instrument(version_span()));
    // Do not wait forever for a rebuild that may be running on a blocking thread.
    runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
    result
}

async fn run(cli: Cli) -> Result<()> {
    info!(?cli, "configuration loaded");
    metrics::init();

    let mmdb_path: Arc<Path> = std::path::absolute(&cli.mmdb_path)
        .with_context(|| format!("invalid mmdb path {}", cli.mmdb_path.display()))?
        .into();
    let mmdb = Arc::new(Mmdb::new());
    if mmdb_path.is_file() {
        mmdb.load(&mmdb_path)?;
    } else {
        info!("mmdb file {} does not exist yet", mmdb_path.display());
    }
    let watcher = MmdbWatcher::start(Arc::clone(&mmdb), Arc::clone(&mmdb_path)).await?;

    // Expose metrics right away: the first database build below can take minutes.
    let metrics_server = MetricsServer::bind(&cli.spoa_listen_adress_metrics_prometheus).await?;
    let metrics_task = tokio::spawn(metrics_server.serve());

    let updater = Arc::new(FireholUpdater::new(
        cli.firehol_repo_path,
        cli.firehol_repo_url,
        cli.firehol_repo_branch,
        cli.firehol_ignore_country,
    ));
    run_update(&updater, &mmdb, &mmdb_path)
        .await
        .context("initial Firehol database update failed")?;

    let mut scheduler = JobScheduler::new()
        .await
        .context("failed to create the job scheduler")?;
    info!(
        "scheduling automatic Firehol update: {}",
        cli.firehol_update_cron_job
    );
    let job = {
        let (updater, mmdb, mmdb_path) = (
            Arc::clone(&updater),
            Arc::clone(&mmdb),
            Arc::clone(&mmdb_path),
        );
        Job::new_async(
            cli.firehol_update_cron_job.as_str(),
            move |_id, _scheduler| {
                let (updater, mmdb, mmdb_path) = (
                    Arc::clone(&updater),
                    Arc::clone(&mmdb),
                    Arc::clone(&mmdb_path),
                );
                Box::pin(
                    async move {
                        if let Err(err) = run_update(&updater, &mmdb, &mmdb_path).await {
                            error!("scheduled Firehol database update failed: {err:#}");
                        }
                    }
                    .instrument(version_span()),
                )
            },
        )
        .with_context(|| format!("invalid cron expression {:?}", cli.firehol_update_cron_job))?
    };
    scheduler
        .add(job)
        .await
        .context("failed to add the update job to the scheduler")?;
    scheduler
        .start()
        .await
        .context("failed to start the job scheduler")?;

    let filter = IpFilter::new(
        Arc::clone(&mmdb),
        cli.drop_by_category.into_iter().collect(),
        cli.drop_by_file_names.into_iter().collect(),
    );
    let outcome = tokio::select! {
        result = spoa::serve(&cli.spoa_listen_adress, filter) => result,
        result = metrics_task => match result {
            Ok(result) => result.context("prometheus metrics server stopped"),
            Err(err) => Err(err).context("prometheus metrics server task failed"),
        },
        received = shutdown_signal() => received.map(|name| info!("received {name}, shutting down...")),
    };

    if let Err(err) = scheduler.shutdown().await {
        error!("failed to stop the job scheduler: {err}");
    }
    watcher.stop().await;
    outcome
}

/// Synchronises the repository and rebuilds the mmdb on a blocking thread, then loads the new
/// file so lookups switch to it without waiting for the file watcher.
async fn run_update(
    updater: &Arc<FireholUpdater>,
    mmdb: &Arc<Mmdb>,
    mmdb_path: &Arc<Path>,
) -> Result<()> {
    let (updater, mmdb, mmdb_path) = (Arc::clone(updater), Arc::clone(mmdb), Arc::clone(mmdb_path));
    tokio::task::spawn_blocking(move || {
        let _span = version_span().entered();
        if updater.update_and_build_mmdb(&mmdb_path)? {
            mmdb.reload_if_changed(&mmdb_path)
                .context("failed to load the rebuilt mmdb")?;
        }
        Ok(())
    })
    .await
    .context("firehol update task panicked")?
}

/// Resolves with the name of the first termination signal received.
async fn shutdown_signal() -> Result<&'static str> {
    let mut sigterm = signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
    let mut sigquit = signal(SignalKind::quit()).context("failed to listen for SIGQUIT")?;
    let mut sigint = signal(SignalKind::interrupt()).context("failed to listen for SIGINT")?;
    Ok(tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigquit.recv() => "SIGQUIT",
        _ = sigint.recv() => "SIGINT",
    })
}
