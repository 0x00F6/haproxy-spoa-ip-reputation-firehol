//! Minimal HTTP/1 endpoint exposing the Prometheus registry.

use anyhow::{Context, Result};
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use prometheus::{Encoder, TextEncoder};
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Pause before retrying `accept` after a failure, so a persistent error (e.g. `EMFILE`) does
/// not turn into a busy loop.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

pub struct MetricsServer {
    listener: TcpListener,
}

impl MetricsServer {
    /// Binds the endpoint; fails early if the address is unusable.
    pub async fn bind(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind metrics listener on {addr}"))?;
        info!("prometheus metrics listening on http://{addr}");
        Ok(Self { listener })
    }

    /// Serves scrapes forever; one task per connection.
    pub async fn serve(self) -> Result<()> {
        loop {
            let (stream, _) = match self.listener.accept().await {
                Ok(connection) => connection,
                Err(err) => {
                    error!("prometheus metrics accept error: {err}");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    continue;
                }
            };
            tokio::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(metrics_response))
                    .await
                {
                    debug!("prometheus metrics connection error: {err}");
                }
            });
        }
    }
}

async fn metrics_response(_req: Request<Incoming>) -> Result<Response<String>, BoxError> {
    let encoder = TextEncoder::new();
    let body = encoder.encode_to_string(&prometheus::gather())?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(body)?)
}
