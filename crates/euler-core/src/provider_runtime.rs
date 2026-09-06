//! Content-free live provider-attempt observation.
//!
//! This is a process-local host attachment, not session configuration or a
//! second event stream. It gives interactive projections attempt/retry state
//! without allowing transport controls into provenance, transcript, or model
//! context.

use euler_provider::{ProviderAttemptEvent, ProviderErrorCategory};
use std::sync::Arc;

/// Runtime owner of a physical provider attempt.
///
/// Only `Root` is the foreground model turn. The other scopes may be shown by
/// a deliberately separate projection, but cannot replace or terminalize the
/// foreground activity merely because they share its provider and model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRuntimeScope {
    Root,
    Companion,
    ParallelReviewer,
    Compaction,
}

impl ProviderRuntimeScope {
    pub fn is_foreground(self) -> bool {
        matches!(self, Self::Root)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRuntimeTarget {
    pub scope: ProviderRuntimeScope,
    pub provider: String,
    pub model: String,
}

impl ProviderRuntimeTarget {
    pub(crate) fn new(scope: ProviderRuntimeScope, provider: &str, model: &str) -> Self {
        Self {
            scope,
            provider: provider.to_owned(),
            model: model.to_owned(),
        }
    }
}

/// One content-free provider control transition observed by the live host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderRuntimeEvent {
    Attempt {
        target: ProviderRuntimeTarget,
        event: ProviderAttemptEvent,
    },
    RetryScheduled {
        target: ProviderRuntimeTarget,
        failed_attempt_id: Option<String>,
        category: ProviderErrorCategory,
        retry_ordinal: u64,
        backoff_ms: u64,
    },
}

/// Cloneable process-local callback installed on a live [`crate::Session`].
///
/// The callback receives no provider bytes, model content, event writer, or
/// canvas handle. Callers that need durable evidence use the canonical model
/// call/result/error events and content-free diagnostics instead.
#[derive(Clone)]
pub struct ProviderRuntimeObserver {
    observe: Arc<dyn Fn(ProviderRuntimeEvent) + Send + Sync>,
}

impl ProviderRuntimeObserver {
    pub fn new(observe: impl Fn(ProviderRuntimeEvent) + Send + Sync + 'static) -> Self {
        Self {
            observe: Arc::new(observe),
        }
    }

    pub(crate) fn emit(&self, event: ProviderRuntimeEvent) {
        (self.observe)(event);
    }
}

impl Default for ProviderRuntimeObserver {
    fn default() -> Self {
        Self::new(|_| {})
    }
}

impl std::fmt::Debug for ProviderRuntimeObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRuntimeObserver")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_scope_is_foreground() {
        assert!(ProviderRuntimeScope::Root.is_foreground());
        for scope in [
            ProviderRuntimeScope::Companion,
            ProviderRuntimeScope::ParallelReviewer,
            ProviderRuntimeScope::Compaction,
        ] {
            assert!(!scope.is_foreground());
            assert_ne!(scope, ProviderRuntimeScope::Root);
        }
    }
}
