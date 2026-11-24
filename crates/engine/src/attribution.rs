//! Cost attribution (ADR-0039): which workload each statement's usage
//! counts towards.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use interlaken_ir::security::{SecValue, SecurityContext};

use crate::telemetry;

/// What labels usage metrics with an attribution key (ADR-0039).
///
/// With attribution on, each query and DML statement's usage is also added
/// to the `interlaken_usage_*` metrics under its session's key. The keys
/// reach the metrics store, with whatever access control it has, so turn
/// this on only for identifiers you are content to expose there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Attribution {
    /// No attribution: the `interlaken_usage_*` metrics aren't recorded.
    #[default]
    Off,
    /// The session's principal, such as `User::"42"`.
    Principal,
    /// The value of the named application context field (`context.app`),
    /// such as `tenant`. A session without the field counts as `none`, and
    /// one whose value isn't a string or integer as `other`.
    Context(String),
}

/// Resolves sessions' attribution keys, keeping at most a limit of distinct keys.
#[derive(Debug)]
pub(crate) struct Attributor {
    /// How keys are chosen.
    mode: Attribution,
    /// The most distinct keys.
    limit: usize,
    /// The keys seen so far.
    keys: Mutex<HashSet<Arc<str>>>,
}

/// The key of sessions beyond the limit of distinct keys, or with a context
/// value that isn't a string or integer.
const OTHER: &str = "other";

impl Attributor {
    /// Construct an attributor choosing keys by `mode`, keeping at most `limit`.
    pub(crate) fn new(mode: Attribution, limit: usize) -> Self {
        Self {
            mode,
            limit,
            keys: Mutex::new(HashSet::new()),
        }
    }

    /// Return the attribution key of a session with `context`, or `None`
    /// with attribution off. A new key beyond the limit becomes `other`,
    /// and is counted.
    pub(crate) fn key(&self, context: &SecurityContext) -> Option<Arc<str>> {
        let raw: String = match &self.mode {
            Attribution::Off => return None,
            Attribution::Principal => context.principal.to_string(),
            Attribution::Context(name) => context
                .application
                .iter()
                .find(|(field, _)| field.as_str() == name)
                .map_or_else(
                    || "none".to_owned(),
                    |(_, value)| match value {
                        SecValue::Str(s) => s.clone(),
                        SecValue::Long(n) => n.to_string(),
                        _ => OTHER.to_owned(),
                    },
                ),
        };
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = keys.get(raw.as_str()) {
            return Some(Arc::clone(key));
        }
        if keys.len() >= self.limit {
            metrics::counter!(telemetry::USAGE_OVERFLOW).increment(1);
            return Some(Arc::from(OTHER));
        }
        let key: Arc<str> = Arc::from(raw);
        keys.insert(Arc::clone(&key));
        Some(key)
    }
}
