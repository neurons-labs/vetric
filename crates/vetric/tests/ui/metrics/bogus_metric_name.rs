use vetric::{Counter, Metrics};

#[derive(Debug, Metrics)]
struct TestMetrics {
    /// Test counter.
    счетчик: Counter,
}

fn main() {}
