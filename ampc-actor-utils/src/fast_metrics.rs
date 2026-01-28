use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::Instant,
};

const FLUSH_AFTER_COUNT: u64 = 1000;

/// Per-metric aggregated statistics, stored with atomic counters for thread-safety.
#[derive(Debug, Default)]
struct GlobalMetricEntry {
    count: AtomicU64,
    /// Sum stored as bits of f64 for atomic operations.
    sum_bits: AtomicU64,
    /// Min stored as bits; uses compare-exchange on the bit representation.
    min_bits: AtomicU64,
    /// Max stored as bits; uses compare-exchange on the bit representation.
    max_bits: AtomicU64,
}

impl GlobalMetricEntry {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_bits: AtomicU64::new(0f64.to_bits()),
            min_bits: AtomicU64::new(f64::INFINITY.to_bits()),
            max_bits: AtomicU64::new(f64::NEG_INFINITY.to_bits()),
        }
    }

    fn accumulate(&self, count: u64, sum: f64, min: f64, max: f64) {
        self.count.fetch_add(count, Ordering::Relaxed);

        // Atomically add to sum using CAS loop
        loop {
            let current = self.sum_bits.load(Ordering::Relaxed);
            let current_f64 = f64::from_bits(current);
            let new_f64 = current_f64 + sum;
            match self.sum_bits.compare_exchange_weak(
                current,
                new_f64.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        // Atomically update min using CAS loop
        loop {
            let current = self.min_bits.load(Ordering::Relaxed);
            let current_f64 = f64::from_bits(current);
            if min >= current_f64 {
                break;
            }
            match self.min_bits.compare_exchange_weak(
                current,
                min.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        // Atomically update max using CAS loop
        loop {
            let current = self.max_bits.load(Ordering::Relaxed);
            let current_f64 = f64::from_bits(current);
            if max <= current_f64 {
                break;
            }
            match self.max_bits.compare_exchange_weak(
                current,
                max.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
    }

    fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
        self.sum_bits.store(0f64.to_bits(), Ordering::Relaxed);
        self.min_bits
            .store(f64::INFINITY.to_bits(), Ordering::Relaxed);
        self.max_bits
            .store(f64::NEG_INFINITY.to_bits(), Ordering::Relaxed);
    }

    fn snapshot(&self) -> MetricSnapshot {
        MetricSnapshot {
            count: self.count.load(Ordering::Relaxed),
            sum: f64::from_bits(self.sum_bits.load(Ordering::Relaxed)),
            min: f64::from_bits(self.min_bits.load(Ordering::Relaxed)),
            max: f64::from_bits(self.max_bits.load(Ordering::Relaxed)),
        }
    }
}

/// A snapshot of metric values.
#[derive(Debug, Clone)]
pub struct MetricSnapshot {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl MetricSnapshot {
    /// Returns the average value, or 0.0 if count is 0.
    pub fn avg(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
}

impl fmt::Display for MetricSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.count == 0 {
            write!(f, "count=0")
        } else {
            write!(
                f,
                "count={}, sum={:.4}, min={:.4}, max={:.4}, avg={:.4}",
                self.count,
                self.sum,
                self.min,
                self.max,
                self.avg()
            )
        }
    }
}

/// Global metrics collector that aggregates metrics from all FastHistogram instances.
/// Thread-safe and accessible as a static singleton.
pub struct GlobalMetricsCollector {
    metrics: Mutex<HashMap<String, GlobalMetricEntry>>,
}

impl GlobalMetricsCollector {
    fn new() -> Self {
        Self {
            metrics: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the global singleton instance.
    pub fn instance() -> &'static GlobalMetricsCollector {
        static INSTANCE: OnceLock<GlobalMetricsCollector> = OnceLock::new();
        INSTANCE.get_or_init(GlobalMetricsCollector::new)
    }

    /// Accumulate metrics from a FastHistogram flush.
    pub fn accumulate(&self, name: &str, count: u64, sum: f64, min: f64, max: f64) {
        let metrics = self.metrics.lock().unwrap();
        if let Some(entry) = metrics.get(name) {
            entry.accumulate(count, sum, min, max);
        } else {
            drop(metrics);
            // Need to insert a new entry
            let mut metrics = self.metrics.lock().unwrap();
            // Double-check after re-acquiring lock
            let entry = metrics
                .entry(name.to_string())
                .or_insert_with(GlobalMetricEntry::new);
            entry.accumulate(count, sum, min, max);
        }
    }

    /// Reset all metrics. Call this at the start of a new job.
    pub fn reset(&self) {
        let metrics = self.metrics.lock().unwrap();
        for entry in metrics.values() {
            entry.reset();
        }
    }

    /// Get a snapshot of all metrics.
    pub fn snapshot(&self) -> HashMap<String, MetricSnapshot> {
        let metrics = self.metrics.lock().unwrap();
        metrics
            .iter()
            .map(|(name, entry)| (name.clone(), entry.snapshot()))
            .collect()
    }

    /// Format all metrics as a log-friendly string.
    pub fn format_summary(&self) -> String {
        let snapshot = self.snapshot();
        if snapshot.is_empty() {
            return "No metrics collected".to_string();
        }

        let mut names: Vec<_> = snapshot.keys().collect();
        names.sort();

        let mut lines = Vec::new();
        for name in names {
            let m = &snapshot[name];
            if m.count > 0 {
                lines.push(format!("  {}: {}", name, m));
            }
        }

        if lines.is_empty() {
            "No metrics recorded".to_string()
        } else {
            format!("Job metrics summary:\n{}", lines.join("\n"))
        }
    }
}

pub struct FastHistogram {
    name: String,
    metrics_count: metrics::Counter,
    metrics_count_rate: metrics::Histogram,

    metrics_sum: metrics::Histogram,
    metrics_sum_rate: metrics::Histogram,

    metrics_min: metrics::Histogram,
    metrics_max: metrics::Histogram,
    metrics_avg: metrics::Histogram,

    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    start: Instant,
}

impl FastHistogram {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),

            metrics_count: metrics::counter!(format!("{}.count", name)),
            metrics_count_rate: metrics::histogram!(format!("{}.count_rate", name)),

            metrics_sum: metrics::histogram!(format!("{}.sum", name)),
            metrics_sum_rate: metrics::histogram!(format!("{}.sum_rate", name)),

            metrics_min: metrics::histogram!(format!("{}.min", name)),
            metrics_max: metrics::histogram!(format!("{}.max", name)),
            metrics_avg: metrics::histogram!(format!("{}.avg", name)),

            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            start: Instant::now(),
        }
    }

    pub fn record(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }

        if self.count >= FLUSH_AFTER_COUNT {
            self.flush();
        }
    }

    pub fn flush(&mut self) {
        if self.count == 0 {
            return;
        }
        let elapsed = self.start.elapsed().as_secs_f64().max(1e-9);

        self.metrics_count.increment(self.count);
        self.metrics_count_rate.record(self.count as f64 / elapsed);

        self.metrics_sum.record(self.sum);
        self.metrics_sum_rate.record(self.sum / elapsed);

        self.metrics_min.record(self.min);
        self.metrics_max.record(self.max);
        self.metrics_avg.record(self.sum / self.count as f64);

        // Accumulate into the global collector
        GlobalMetricsCollector::instance()
            .accumulate(&self.name, self.count, self.sum, self.min, self.max);

        self.count = 0;
        self.sum = 0.0;
        self.min = f64::INFINITY;
        self.max = f64::NEG_INFINITY;
        self.start = Instant::now();
    }
}

impl Clone for FastHistogram {
    fn clone(&self) -> Self {
        Self::new(&self.name)
    }
}

// Flush on drop.
impl Drop for FastHistogram {
    fn drop(&mut self) {
        self.flush();
    }
}

impl fmt::Debug for FastHistogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FastHistogram")
            .field("count", &self.count)
            .field("sum", &self.sum)
            .field("min", &self.min)
            .field("max", &self.max)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::{
        with_local_recorder, Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key,
        KeyName, Metadata, Recorder, SharedString, Unit,
    };
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::Arc;

    #[test]
    fn test_fast_histogram() {
        let recorder = TestRecorder::new();

        let mut hist = with_local_recorder(&recorder, || FastHistogram::new("test_histogram"));

        hist.record(1.0);
        hist.record(2.0);
        hist.record(3.0);
        assert_eq!(hist.count, 3);
        assert_eq!(hist.sum, 6.0);
        assert_eq!(hist.min, 1.0);
        assert_eq!(hist.max, 3.0);
        hist.flush();

        assert_eq!(hist.count, 0);
        assert_eq!(hist.sum, 0.0);
        assert_eq!(hist.min, f64::INFINITY);
        assert_eq!(hist.max, f64::NEG_INFINITY);
        hist.record(4.0);
        drop(hist); // Flush.

        let mut events = recorder.events();

        // Handle time-dependent values.
        const RATE: f64 = 123.0;
        for i in [1, 3, 8, 10] {
            assert!(events[i].2 > 0.0);
            events[i].2 = RATE;
        }

        assert_eq!(
            events,
            vec![
                // Records: 1, 2, 3.
                (COUNT, "test_histogram.count".to_string(), 3.0),
                (HISTO, "test_histogram.count_rate".to_string(), RATE),
                (HISTO, "test_histogram.sum".to_string(), 6.0),
                (HISTO, "test_histogram.sum_rate".to_string(), RATE),
                (HISTO, "test_histogram.min".to_string(), 1.0),
                (HISTO, "test_histogram.max".to_string(), 3.0),
                (HISTO, "test_histogram.avg".to_string(), 2.0),
                // Records: 4.
                (COUNT, "test_histogram.count".to_string(), 1.0),
                (HISTO, "test_histogram.count_rate".to_string(), RATE),
                (HISTO, "test_histogram.sum".to_string(), 4.0),
                (HISTO, "test_histogram.sum_rate".to_string(), RATE),
                (HISTO, "test_histogram.min".to_string(), 4.0),
                (HISTO, "test_histogram.max".to_string(), 4.0),
                (HISTO, "test_histogram.avg".to_string(), 4.0),
            ]
        );
    }

    struct TestRecorder {
        tx: Sender<(bool, String, f64)>,
        rx: Receiver<(bool, String, f64)>,
    }

    impl TestRecorder {
        fn new() -> Self {
            let (tx, rx) = mpsc::channel();
            Self { tx, rx }
        }

        fn events(&self) -> Vec<(bool, String, f64)> {
            self.rx.try_iter().collect()
        }
    }

    impl Recorder for TestRecorder {
        fn describe_counter(
            &self,
            _name: KeyName,
            _unit: Option<Unit>,
            _description: SharedString,
        ) {
        }

        fn describe_gauge(&self, _name: KeyName, _unit: Option<Unit>, _description: SharedString) {}

        fn describe_histogram(
            &self,
            _name: KeyName,
            _unit: Option<Unit>,
            _description: SharedString,
        ) {
        }

        fn register_counter(&self, key: &Key, _meta: &Metadata<'_>) -> Counter {
            Counter::from_arc(Arc::new(TestCounter {
                is_histogram: false,
                name: key.name().to_string(),
                events: self.tx.clone(),
            }))
        }

        fn register_gauge(&self, key: &Key, _meta: &Metadata<'_>) -> Gauge {
            Gauge::from_arc(Arc::new(TestCounter {
                is_histogram: false,
                name: key.name().to_string(),
                events: self.tx.clone(),
            }))
        }

        fn register_histogram(&self, key: &Key, _meta: &Metadata<'_>) -> Histogram {
            Histogram::from_arc(Arc::new(TestCounter {
                is_histogram: true,
                name: key.name().to_string(),
                events: self.tx.clone(),
            }))
        }
    }

    struct TestCounter {
        is_histogram: bool,
        name: String,
        events: Sender<(bool, String, f64)>,
    }
    const COUNT: bool = false;
    const HISTO: bool = true;

    impl CounterFn for TestCounter {
        fn increment(&self, value: u64) {
            self.events
                .send((self.is_histogram, self.name.clone(), value as f64))
                .unwrap();
        }

        fn absolute(&self, _value: u64) {
            unimplemented!()
        }
    }

    impl GaugeFn for TestCounter {
        fn set(&self, value: f64) {
            self.events
                .send((self.is_histogram, self.name.clone(), value))
                .unwrap();
        }

        fn increment(&self, _value: f64) {
            unimplemented!();
        }

        fn decrement(&self, _value: f64) {
            unimplemented!();
        }
    }

    impl HistogramFn for TestCounter {
        fn record(&self, value: f64) {
            self.events
                .send((self.is_histogram, self.name.clone(), value))
                .unwrap();
        }
    }
}
