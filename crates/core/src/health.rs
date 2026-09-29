//! Health model shared by /health and /readyz.
//!
//! `/health` is liveness (the process answers). `/readyz` is readiness: every
//! listed component must report [`ComponentStatus::Ready`], otherwise the
//! endpoint returns 503 and the Container supervisor / DO keepalive treats
//! the instance as not serving.

use serde::Serialize;

/// Readiness of one component (database pool, gateway shard, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentStatus {
    Ready,
    Starting,
    Down,
}

/// Point-in-time readiness snapshot served as JSON on /readyz.
#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub components: Vec<(String, ComponentStatus)>,
}

impl HealthReport {
    #[must_use]
    pub fn new(components: Vec<(String, ComponentStatus)>) -> Self {
        Self { components }
    }

    /// Ready only when every component is ready (vacuous truth: an empty
    /// report — S1 skeleton before any shard exists — counts as live but the
    /// bot crate always attaches at least the process component).
    #[must_use]
    pub fn ready(&self) -> bool {
        self.components
            .iter()
            .all(|(_, status)| *status == ComponentStatus::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_ready_when_any_component_down() {
        let report = HealthReport::new(vec![
            ("process".to_owned(), ComponentStatus::Ready),
            ("gateway".to_owned(), ComponentStatus::Down),
        ]);
        assert!(!report.ready());
    }

    #[test]
    fn ready_when_all_ready() {
        let report = HealthReport::new(vec![("process".to_owned(), ComponentStatus::Ready)]);
        assert!(report.ready());
    }
}
