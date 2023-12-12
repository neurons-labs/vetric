//! `MetricsExporter` and closely related types.

use std::{
    collections::HashSet,
    fmt::{self, Write as _},
    future::{self, Future},
    net::SocketAddr,
    pin::Pin,
    str,
    sync::Arc,
    time::{Duration, Instant},
};

use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode, Uri,
};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
};
use tokio::net::TcpListener;

type FullBody = Full<Bytes>;

#[cfg(test)]
mod tests;

use vetric::{Format, MetricsCollection, Registry};

use crate::metrics::{Facade, EXPORTER_METRICS};

#[derive(Clone)]
struct MetricsExporterInner {
    registry: Arc<Registry>,
    format: Format,
}

impl MetricsExporterInner {
    fn render_body(&self) -> FullBody {
        let latency = EXPORTER_METRICS.scrape_latency[&Facade::Vetric].start();
        let mut buffer = String::with_capacity(1_024);
        // ^ `unwrap()` is safe; writing to a string never fails.
        self.registry.encode(&mut buffer, self.format).unwrap();

        let latency = latency.observe();
        let scraped_size = buffer.len();
        EXPORTER_METRICS.scraped_size[&Facade::Vetric].observe(scraped_size);
        tracing::debug!(
            latency_sec = latency.as_secs_f64(),
            scraped_size,
            "Scraped metrics using `vetric` façade in {latency:?} (scraped size: {scraped_size}B)"
        );
        FullBody::from(buffer)
    }

    // TODO: consider using a streaming response?
    fn render(&self) -> Response<FullBody> {
        let content_type = if matches!(self.format, Format::Prometheus) {
            Format::PROMETHEUS_CONTENT_TYPE
        } else {
            Format::OPEN_METRICS_CONTENT_TYPE
        };
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .body(self.render_body())
            .unwrap()
    }
}

/// Metrics exporter to Prometheus.
///
/// An exporter scrapes metrics from a [`Registry`]. A [`Default`] exporter will use the registry
/// of all metrics auto-registered in an app and all its (transitive) dependencies, i.e. one
/// created using [`Registry::collect()`]. To have more granular control over the registry, you can
/// provide it explicitly using [`Self::new()`].
///
/// # Examples
///
/// See crate-level docs for the examples of usage.
pub struct MetricsExporter<'a> {
    inner: MetricsExporterInner,
    shutdown_future: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
}

impl fmt::Debug for MetricsExporter<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MetricsExporter")
            .field("registry", &self.inner.registry)
            .finish_non_exhaustive()
    }
}

/// Creates an exporter based on [`MetricsCollection`]`::default().collect()` output (i.e., with all
/// metrics registered by the app and libs it depends on).
impl Default for MetricsExporter<'_> {
    fn default() -> Self {
        Self::new(MetricsCollection::default().collect().into())
    }
}

impl<'a> MetricsExporter<'a> {
    /// Creates an exporter based on the provided metrics [`Registry`]. Note that the registry
    /// is in `Arc`, meaning it can be used elsewhere (e.g., to export data in another format).
    pub fn new(registry: Arc<Registry>) -> Self {
        Self::log_metrics_stats(&registry);
        Self {
            inner: MetricsExporterInner {
                registry,
                format: Format::OpenMetricsForPrometheus,
            },
            shutdown_future: Box::pin(future::pending()),
        }
    }

    fn log_metrics_stats(registry: &Registry) {
        const SAMPLED_CRATE_COUNT: usize = 5;

        let groups = registry.descriptors().groups();
        let group_count = groups.len();
        let metric_count = registry.descriptors().metric_count();

        let mut unique_crates = HashSet::new();
        for group in groups {
            let crate_info = (group.crate_name, group.crate_version);
            if unique_crates.insert(crate_info) && unique_crates.len() >= SAMPLED_CRATE_COUNT {
                break;
            }
        }
        let mut crates = String::with_capacity(unique_crates.len() * 16);
        // ^ 16 chars looks like a somewhat reasonable estimate for crate name + version
        for (crate_name, crate_version) in unique_crates {
            write!(crates, "{crate_name} {crate_version}, ").unwrap();
        }
        crates.push_str("...");

        tracing::info!(
            "Created metrics exporter with {metric_count} metrics in {group_count} groups from crates {crates}"
        );
    }

    /// Sets the export [`Format`]. By default, [`Format::OpenMetricsForPrometheus`] is used
    /// (i.e., OpenMetrics text format with minor changes so that it is fully parsed by Prometheus).
    ///
    /// See `Format` docs for more details on differences between export formats. Note that using
    /// [`Format::OpenMetrics`] is not fully supported by Prometheus at the time of writing.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.inner.format = format;
        self
    }

    /// Configures graceful shutdown for the exporter server.
    #[must_use]
    pub fn with_graceful_shutdown<F>(mut self, shutdown: F) -> Self
    where
        F: Future<Output = ()> + Send + 'a,
    {
        self.shutdown_future = Box::pin(shutdown);
        self
    }

    // TODO(VJJ):update this when hyper_utils get update with the server.

    /// Starts the server on the specified address. This future resolves when the server is shut
    /// down.
    ///
    /// The server will expose the following endpoints:
    ///
    /// - `GET` on any path: serves the metrics in the text format configured using
    ///   [`Self::with_format()`]
    ///
    /// # Errors
    ///
    /// Returns an error if binding to the specified address fails.
    pub async fn start(self, bind_address: SocketAddr) -> hyper::Result<()> {
        tracing::info!("Starting Prometheus exporter web server on {bind_address}");

        let listener = TcpListener::bind(bind_address).await.unwrap();
        let mut shutdown = self.shutdown_future;
        loop {
            let (socket, _) = tokio::select! {
                // Either accept a new connection...
                result = listener.accept() => {
                    result.unwrap()
                }
                // ...or wait a shutdown future and stop the accept loop.
                _ =  &mut shutdown=> {
                    tracing::info!("Stop signal received, Prometheus metrics exporter is shutting down");
                    break;
                }
            };

            let exporter_inner = self.inner.clone();
            tokio::task::spawn(async move {
                let socket = TokioIo::new(socket);
                let service = service_fn(move |_| {
                    let resp = exporter_inner.render();
                    async move { Ok::<_, hyper::Error>(resp) }
                });

                if let Err(err) = http1::Builder::new()
                    .serve_connection(socket, service)
                    .await
                {
                    tracing::error!("Error serving connection: {:?}", err);
                }
            });
        }
        tracing::info!("Prometheus metrics exporter server shut down");
        Ok(())
    }

    /// Starts pushing metrics to the `endpoint` with the specified `interval` between pushes.
    pub async fn push_to_gateway(self, endpoint: Uri, interval: Duration) {
        /// Minimum interval between error logs. Prevents spanning logs at `WARN` / `ERROR` level
        /// too frequently if `interval` is low (e.g., 1s).
        const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);

        tracing::info!(
            "Starting push-based Prometheus exporter to `{endpoint}` with push interval {interval:?}"
        );

        let client = Client::builder(TokioExecutor::new()).build_http();
        let mut shutdown = self.shutdown_future;
        let mut last_error_log_timestamp = None::<Instant>;
        loop {
            let mut shutdown_requested = false;
            if tokio::time::timeout(interval, &mut shutdown).await.is_ok() {
                tracing::info!(
                    "Stop signal received, Prometheus metrics exporter is shutting down"
                );
                shutdown_requested = true;
            }

            let request = Request::builder()
                .method(Method::PUT)
                .uri(endpoint.clone())
                .header(header::CONTENT_TYPE, Format::OPEN_METRICS_CONTENT_TYPE)
                .body(self.inner.render_body())
                .expect("Failed creating Prometheus push gateway request");

            match client.request(request).await {
                Ok(response) => {
                    if !response.status().is_success() {
                        let should_log_error = last_error_log_timestamp
                            .map_or(true, |timestamp| timestamp.elapsed() >= ERROR_LOG_INTERVAL);
                        if should_log_error {
                            // Do not block further pushes during error handling.
                            tokio::spawn(report_erroneous_response(endpoint.clone(), response));
                            last_error_log_timestamp = Some(Instant::now());
                            // ^ This timestamp is somewhat imprecise (we don't wait to handle the
                            // response), but it seems fine for rate-limiting purposes.
                        }
                    }
                }
                Err(err) => {
                    let should_log_error = last_error_log_timestamp
                        .map_or(true, |timestamp| timestamp.elapsed() >= ERROR_LOG_INTERVAL);
                    if should_log_error {
                        tracing::error!(
                            %err,
                            %endpoint,
                            "Error submitting metrics to Prometheus push gateway"
                        );
                        last_error_log_timestamp = Some(Instant::now());
                    }
                }
            }
            if shutdown_requested {
                break;
            }
        }
    }
}

async fn report_erroneous_response(endpoint: Uri, response: Response<Incoming>) {
    let status = response.status();
    let body = match response.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(err) => {
            tracing::error!(
                %err,
                %status,
                %endpoint,
                "Failed reading erroneous response from Prometheus push gateway"
            );
            return;
        }
    };

    let err_body: String;
    let body = match str::from_utf8(&body) {
        Ok(body) => body,
        Err(err) => {
            let body_length = body.len();
            err_body = format!("(Non UTF-8 body with length {body_length}B: {err})");
            &err_body
        }
    };
    tracing::warn!(
        %status,
        %body,
        %endpoint,
        "Error pushing metrics to Prometheus push gateway"
    );
}
