//! An in-memory span subscriber for tests.

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// A span recorded by [`TestSubscriber`], with every field it was given.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CapturedSpan {
    /// The span's name.
    pub name: String,
    /// Its fields, as names and debug-formatted values, in the order recorded.
    pub fields: Vec<(String, String)>,
}

impl CapturedSpan {
    /// Return the last value recorded for the field `name`, if any.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .rev()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Collects field values into a list.
struct Fields<'a>(&'a mut Vec<(String, String)>);

impl Visit for Fields<'_> {
    /// Record a field's value, unquoted if it is a string.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }

    /// Record a field's value in its debug form.
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

/// What the subscriber has recorded.
#[derive(Debug, Default)]
struct Recorded {
    /// Spans, indexed by their ID minus one.
    spans: Vec<CapturedSpan>,
    /// Every event's fields.
    events: Vec<Vec<(String, String)>>,
}

/// A tracing subscriber that keeps every span and event in memory, for tests.
#[derive(Clone, Debug, Default)]
pub struct TestSubscriber {
    recorded: Arc<Mutex<Recorded>>,
}

impl TestSubscriber {
    /// Lock what has been recorded.
    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.recorded.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Return every span named `name` recorded so far.
    #[must_use]
    pub fn spans(&self, name: &str) -> Vec<CapturedSpan> {
        self.lock()
            .spans
            .iter()
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    }

    /// Return every field value of every span and event recorded so far.
    #[must_use]
    pub fn field_values(&self) -> Vec<String> {
        let recorded = self.lock();
        recorded
            .spans
            .iter()
            .flat_map(|span| span.fields.iter())
            .chain(recorded.events.iter().flatten())
            .map(|(name, value)| format!("{name}={value}"))
            .collect()
    }
}

impl Subscriber for TestSubscriber {
    /// Enable everything.
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    /// Record a new span and its initial fields.
    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let mut span = CapturedSpan {
            name: attributes.metadata().name().to_owned(),
            fields: Vec::new(),
        };
        attributes.record(&mut Fields(&mut span.fields));
        let mut recorded = self.lock();
        recorded.spans.push(span);
        Id::from_u64(u64::try_from(recorded.spans.len()).expect("span count fits u64"))
    }

    /// Record fields set on a span after it was created.
    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut recorded = self.lock();
        let index = usize::try_from(span.into_u64() - 1).expect("span index fits usize");
        if let Some(span) = recorded.spans.get_mut(index) {
            values.record(&mut Fields(&mut span.fields));
        }
    }

    /// Ignore links between spans.
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    /// Record an event's fields.
    fn event(&self, event: &Event<'_>) {
        let mut fields = Vec::new();
        event.record(&mut Fields(&mut fields));
        self.lock().events.push(fields);
    }

    /// Ignore entering a span.
    fn enter(&self, _span: &Id) {}

    /// Ignore leaving a span.
    fn exit(&self, _span: &Id) {}
}

/// Install a [`TestSubscriber`] as this process's tracing subscriber, once,
/// and return it. Later calls return the same subscriber.
///
/// # Panics
///
/// Panics if another global tracing subscriber was installed first.
///
/// # Examples
///
/// ```
/// let subscriber = interlaken_telemetry::install_test_subscriber();
/// tracing::info_span!("example", kind = "a").in_scope(|| {});
/// assert_eq!(subscriber.spans("example")[0].field("kind"), Some("a"));
/// ```
pub fn install_test_subscriber() -> &'static TestSubscriber {
    static SUBSCRIBER: OnceLock<TestSubscriber> = OnceLock::new();
    SUBSCRIBER.get_or_init(|| {
        let subscriber = TestSubscriber::default();
        tracing::subscriber::set_global_default(subscriber.clone())
            .expect("no other tracing subscriber is installed");
        subscriber
    })
}
