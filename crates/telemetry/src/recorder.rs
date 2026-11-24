//! An in-memory metrics recorder for tests.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use metrics::{
    Counter, Gauge, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};

/// A histogram's recorded samples.
#[derive(Debug, Default)]
struct Samples(Mutex<Vec<f64>>);

impl HistogramFn for Samples {
    /// Append a sample.
    fn record(&self, value: f64) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(value);
    }
}

/// Every series recorded, by key.
#[derive(Debug, Default)]
struct Series {
    counters: HashMap<Key, Arc<AtomicU64>>,
    gauges: HashMap<Key, Arc<AtomicU64>>,
    histograms: HashMap<Key, Arc<Samples>>,
    described: BTreeSet<String>,
}

/// A metrics recorder that keeps every series in memory, for tests.
///
/// Counters, gauges and histograms are read back by name and a subset of
/// their labels: [`TestRecorder::counter`] sums every series of the name
/// whose labels include the ones given. Values are cumulative for the whole
/// process, so tests compare readings before and after the work they check.
#[derive(Debug, Default)]
pub struct TestRecorder {
    series: Arc<Mutex<Series>>,
}

/// Return whether `key` is named `name` and carries every label in `labels`.
fn matches(key: &Key, name: &str, labels: &[(&str, &str)]) -> bool {
    key.name() == name
        && labels
            .iter()
            .all(|(k, v)| key.labels().any(|l| l.key() == *k && l.value() == *v))
}

impl TestRecorder {
    /// Lock the series.
    fn lock(&self) -> std::sync::MutexGuard<'_, Series> {
        self.series.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Return the sum of the counters named `name` whose labels include `labels`.
    #[must_use]
    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        self.lock()
            .counters
            .iter()
            .filter(|(key, _)| matches(key, name, labels))
            .map(|(_, value)| value.load(Ordering::Relaxed))
            .sum()
    }

    /// Return the value of the gauge named `name` with labels including
    /// `labels`, if one has been set. If several match, any one is returned.
    #[must_use]
    pub fn gauge(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.lock()
            .gauges
            .iter()
            .find(|(key, _)| matches(key, name, labels))
            .map(|(_, value)| f64::from_bits(value.load(Ordering::Relaxed)))
    }

    /// Return every sample of the histograms named `name` whose labels include `labels`.
    #[must_use]
    pub fn histogram(&self, name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
        self.lock()
            .histograms
            .iter()
            .filter(|(key, _)| matches(key, name, labels))
            .flat_map(|(_, samples)| {
                samples
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            })
            .collect()
    }

    /// Return the names of every series recorded so far.
    #[must_use]
    pub fn names(&self) -> BTreeSet<String> {
        let series = self.lock();
        series
            .counters
            .keys()
            .chain(series.gauges.keys())
            .chain(series.histograms.keys())
            .map(|key| key.name().to_owned())
            .collect()
    }

    /// Return the names of every metric described so far.
    #[must_use]
    pub fn described(&self) -> BTreeSet<String> {
        self.lock().described.clone()
    }

    /// Return every label value of every series recorded so far, with the
    /// metric name and label key, as `name{key=value}`.
    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        let series = self.lock();
        series
            .counters
            .keys()
            .chain(series.gauges.keys())
            .chain(series.histograms.keys())
            .flat_map(|key| {
                key.labels()
                    .map(|l| format!("{}{{{}={}}}", key.name(), l.key(), l.value()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Return the number of distinct series named `name`.
    #[must_use]
    pub fn series_count(&self, name: &str) -> usize {
        let series = self.lock();
        series
            .counters
            .keys()
            .chain(series.gauges.keys())
            .chain(series.histograms.keys())
            .filter(|key| key.name() == name)
            .count()
    }
}

/// The recorder installed in this process; it delegates to the shared series.
struct Installed(Arc<Mutex<Series>>);

impl Installed {
    /// Lock the series.
    fn lock(&self) -> std::sync::MutexGuard<'_, Series> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record that the metric `key` has been described.
    fn describe(&self, key: &KeyName) {
        self.lock().described.insert(key.as_str().to_owned());
    }
}

impl Recorder for Installed {
    /// Note the counter's description.
    fn describe_counter(&self, key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        self.describe(&key);
    }

    /// Note the gauge's description.
    fn describe_gauge(&self, key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        self.describe(&key);
    }

    /// Note the histogram's description.
    fn describe_histogram(&self, key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        self.describe(&key);
    }

    /// Return the counter for `key`, creating it at zero.
    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        let value = Arc::clone(self.lock().counters.entry(key.clone()).or_default());
        Counter::from_arc(value)
    }

    /// Return the gauge for `key`, creating it at zero.
    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        let value = Arc::clone(self.lock().gauges.entry(key.clone()).or_default());
        Gauge::from_arc(value)
    }

    /// Return the histogram for `key`, creating it empty.
    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        let value = Arc::clone(self.lock().histograms.entry(key.clone()).or_default());
        Histogram::from_arc(value)
    }
}

/// Install a [`TestRecorder`] as this process's metrics recorder, once, and
/// return it. Later calls return the same recorder.
///
/// # Panics
///
/// Panics if another metrics recorder was installed first.
///
/// # Examples
///
/// ```
/// let recorder = interlaken_telemetry::install_test_recorder();
/// metrics::counter!("example_total", "kind" => "a").increment(2);
/// assert_eq!(recorder.counter("example_total", &[("kind", "a")]), 2);
/// ```
pub fn install_test_recorder() -> &'static TestRecorder {
    static RECORDER: OnceLock<TestRecorder> = OnceLock::new();
    RECORDER.get_or_init(|| {
        let recorder = TestRecorder::default();
        metrics::set_global_recorder(Installed(Arc::clone(&recorder.series)))
            .expect("no other metrics recorder is installed");
        recorder
    })
}
