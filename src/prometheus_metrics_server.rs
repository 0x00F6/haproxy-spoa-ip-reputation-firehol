use anyhow::{Context, Result};
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use prometheus::{Encoder, TextEncoder};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{error, info};

type BoxedErr = Box<dyn std::error::Error + Send + Sync + 'static>;

pub struct PrometheusMetricsServer {
    addr: SocketAddr,
}

impl PrometheusMetricsServer {
    pub fn new(addr: &str) -> Result<Self> {
        let addr = addr
            .parse()
            .with_context(|| format!("{}:{} error parsing address: {addr}", file!(), line!()))?;
        Ok(Self { addr })
    }

    pub async fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(self.addr).await.with_context(|| {
            format!(
                "{}:{} error binding to address: {}",
                file!(),
                line!(),
                self.addr
            )
        })?;

        info!("prometheus metrics listening on http://{}", self.addr);

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(result) => result,
                Err(e) => {
                    error!("prometheus metrics accept error: {:?}", e);
                    continue;
                }
            };
            let io = TokioIo::new(stream);

            tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new()
                    .serve_connection(io, service_fn(serve_metrics_prometheus_req))
                    .await
                {
                    error!("prometheus metrics server error: {:?}", err);
                }
            });
        }
    }
}

async fn serve_metrics_prometheus_req(
    _req: Request<Incoming>,
) -> Result<Response<String>, BoxedErr> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let body = encoder.encode_to_string(&metric_families)?;

    let response = Response::builder()
        .status(200)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(body)?;

    Ok(response)
}
