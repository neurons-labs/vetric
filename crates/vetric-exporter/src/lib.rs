//! Metric exporter based on the `hyper` web server / client.
//!
//! An exporter scrapes metrics from a [`Registry`] and allows exporting them to Prometheus by
//! either running a web server or pushing to the Prometheus push gateway. An exporter should only
//! be initialized in applications, not libraries.
//!
//! # Examples
//!
//! Running a pull-based exporter with graceful shutdown:
//!
//! ```
//! use tokio::sync::watch;
//! use vetric_exporter::MetricsExporter;
//!
//! async fn my_app() {
//!     let (shutdown_sender, mut shutdown_receiver) = watch::channel(());
//!     let exporter = MetricsExporter::default()
//!         .with_graceful_shutdown(async move {
//!             shutdown_receiver.changed().await.ok();
//!         });
//!     let bind_address = "0.0.0.0:3312".parse().unwrap();
//!     tokio::spawn(exporter.start(bind_address));
//!
//!     // Then, once the app is shutting down:
//!     shutdown_sender.send_replace(());
//! }
//! ```
//!
//! Running a push-based exporter that scrapes metrics each 10 seconds:
//!
//! ```
//! # use std::time::Duration;
//! # use tokio::sync::watch;
//! # use vetric_exporter::MetricsExporter;
//! async fn my_app() {
//!     let exporter = MetricsExporter::default();
//!     let exporter_task = exporter.push_to_gateway(
//!         "http://prom-gateway/job/pushgateway/instance/my_app".parse().unwrap(),
//!         Duration::from_secs(10),
//!     );
//!     tokio::spawn(exporter_task);
//! }
//! ```

mod exporter;
mod metrics;

pub use crate::exporter::MetricsExporter;
