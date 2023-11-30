//! Tests for metrics exporter.

use std::{
    net::Ipv4Addr,
    str,
    sync::atomic::{AtomicU32, Ordering},
};

use http_body_util::BodyExt;
use hyper::{body::Bytes, HeaderMap};
use tokio::sync::{
    mpsc::{self, UnboundedSender},
    Mutex,
};
use tracing::subscriber::Subscriber;
use tracing_capture::{CaptureLayer, SharedStorage};
use tracing_subscriber::layer::SubscriberExt;
use vetric::{Counter, EncodeLabelSet, EncodeLabelValue, Family, Gauge, Global, Metrics};

use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(3);
// Since all tests access global state (metrics), we shouldn't run them in parallel
static TEST_MUTEX: Mutex<()> = Mutex::const_new(());

static REQUEST_COUNTER: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(label = "label")]
struct Label(&'static str);

impl fmt::Display for Label {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "modern")]
struct TestMetrics {
    /// Counter.
    counter: Counter,
    /// Gauge with a label defined using the modern approach.
    gauge: Family<Label, Gauge<f64>>,
}

#[vetric::register]
static TEST_METRICS: Global<TestMetrics> = Global::new();

fn report_metrics() {
    TEST_METRICS.counter.inc();
    TEST_METRICS.gauge[&Label("value")].set(42.0);
}

fn assert_scraped_payload_is_valid(payload: &Bytes) {
    let payload = str::from_utf8(payload).unwrap();
    let payload_lines: Vec<_> = payload.lines().collect();

    assert!(payload_lines.iter().all(|line| !line.is_empty()));

    let expected_lines = [
        "# TYPE modern_counter counter",
        "# TYPE modern_gauge gauge",
        r#"modern_gauge{label="value"} 42.0"#,
    ];
    for line in expected_lines {
        assert!(payload_lines.contains(&line), "{payload_lines:#?}");
    }

    // Check counter reporting.
    let expected_prefixes = ["modern_counter "];
    for prefix in expected_prefixes {
        assert!(
            payload_lines.iter().any(|line| line.starts_with(prefix)),
            "{payload_lines:#?}"
        );
    }

    let lines_count = payload_lines.len();
    assert_eq!(*payload_lines.last().unwrap(), "# EOF");
    for &line in &payload_lines[..lines_count - 1] {
        assert_ne!(line, "# EOF");
    }
}

#[derive(Debug)]
enum MockServerBehavior {
    Ok,
    Error,
    Panic,
}

impl MockServerBehavior {
    fn from_counter(counter: &AtomicU32) -> Self {
        match counter.fetch_add(1, Ordering::SeqCst) % 3 {
            1 => Self::Error,
            2 => Self::Panic,
            _ => Self::Ok,
        }
    }

    fn response(self) -> Response<FullBody> {
        match self {
            Self::Ok => Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(FullBody::default())
                .unwrap(),
            Self::Error => Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(FullBody::from(b"Mistake!" as &[u8]))
                .unwrap(),
            Self::Panic => panic!("oops"),
        }
    }
}

fn tracing_subscriber(storage: &SharedStorage) -> impl Subscriber {
    tracing_subscriber::fmt()
        .pretty()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .finish()
        .with(CaptureLayer::new(storage))
}

#[tokio::test]
async fn using_push_gateway() {
    let _guard = TEST_MUTEX.lock().await;
    let tracing_storage = SharedStorage::default();
    let _subscriber_guard = tracing::subscriber::set_default(tracing_subscriber(&tracing_storage));
    // ^ **NB.** `set_default()` only works because tests use a single-threaded Tokio runtime

    let bind_address: SocketAddr = (Ipv4Addr::LOCALHOST, 14670).into();
    let (req_sender, mut req_receiver) = mpsc::unbounded_channel::<(HeaderMap, Bytes)>();

    let exporter = MetricsExporter::default();
    report_metrics();
    tokio::spawn(mock_server(bind_address, req_sender));

    let endpoint = format!("http://{bind_address}/").parse().unwrap();
    tokio::spawn(exporter.push_to_gateway(endpoint, Duration::from_millis(50)));

    // Test that the push logic doesn't stop after an error received from the gateway
    for _ in 0..4 {
        let (request_headers, request_body) =
            tokio::time::timeout(TEST_TIMEOUT, req_receiver.recv())
                .await
                .expect("timed out waiting for metrics push")
                .unwrap();
        assert_eq!(
            request_headers[&header::CONTENT_TYPE],
            Format::OPEN_METRICS_CONTENT_TYPE
        );
        assert_scraped_payload_is_valid(&request_body);
    }

    assert_logs(&tracing_storage.lock());
}

async fn mock_server(bind_address: SocketAddr, req_sender: UnboundedSender<(HeaderMap, Bytes)>) {
    let listener = TcpListener::bind(bind_address).await.unwrap();
    loop {
        let req_sender1 = req_sender.clone();
        let (tcp, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(tcp);
        tokio::task::spawn(async move {
            // Handle the connection from the client using HTTP1 and pass any
            // HTTP requests received on that connection to the `hello` function
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req: Request<Incoming>| {
                        assert_eq!(*req.method(), Method::PUT);

                        let behavior = MockServerBehavior::from_counter(&REQUEST_COUNTER);
                        let sender = req_sender1.clone();

                        async move {
                            let headers = req.headers().clone();
                            let collected = req.into_body().collect().await.unwrap();
                            let body = collected.to_bytes();
                            sender.send((headers, body)).ok();
                            Ok::<_, hyper::Error>(behavior.response())
                        }
                    }),
                )
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}

fn assert_logs(tracing_storage: &tracing_capture::Storage) {
    let warnings = tracing_storage.all_events().filter(|event| {
        event
            .metadata()
            .target()
            .starts_with(env!("CARGO_CRATE_NAME"))
            && *event.metadata().level() <= tracing::Level::WARN
    });
    let warnings: Vec<_> = warnings.collect();
    // Check that we don't spam the error messages.
    assert_eq!(warnings.len(), 1);

    // Check warning contents. We should log the first encountered error (i.e., "Service
    // unavailable").
    let warning: &tracing_capture::CapturedEvent = &warnings[0];
    assert!(
        warning
            .message()
            .unwrap()
            .contains("Error pushing metrics to Prometheus push gateway")
    );
    assert_eq!(
        warning["status"].as_debug_str().unwrap(),
        StatusCode::SERVICE_UNAVAILABLE.to_string()
    );
    assert_eq!(warning["body"].as_debug_str().unwrap(), "Mistake!");
    assert!(
        warning["endpoint"]
            .as_debug_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
}
