//! Fixed-vocabulary record of why the durable gateway task stopped (TOG-13044).
//!
//! The container's stderr never reaches Workers Logs, so a failing gateway
//! used to be diagnosable only by the Operator. The failing step is recorded
//! here as an enum, never as text, and served on `/readyz`. Because the only
//! values that can reach the field are these variants, no SQLx error, URL,
//! credential or other service output can leak through it.

use std::sync::{Arc, Mutex};

/// Phase reported next to the class. One phase today; kept as a type so a
/// later phase cannot smuggle in free-form text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePhase {
    DurableGateway,
}

impl FailurePhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DurableGateway => "durable_gateway",
        }
    }
}

/// One variant per fallible step of the gateway task. Every token matches
/// `[a-z0-9_]{1,32}`, the shape `scripts/staging_rollout.py` and the Worker
/// accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The shared store was not handed to the task (defensive; prerequisites
    /// are validated before the task is spawned).
    StoreUnavailable,
    /// Opening the dedicated single-connection checkpoint pool.
    GatewayPoolConnectFailed,
    /// Reading the durable gateway checkpoint.
    CheckpointLoadFailed,
    /// Onboarding gate (mode) parsing.
    OnboardingGatesInvalid,
    /// Onboarding runtime initialisation (identity probe over REST).
    OnboardingInitFailed,
    /// Custom-command registry bootstrap (store read and command publication).
    CustomCommandsInitFailed,
    /// Reading the persisted leveling milestones.
    MilestonesLoadFailed,
    /// Automod environment rejected by `automod_gateway::resolve`.
    AutomodConfigInvalid,
    /// Building the automod REST executor.
    AutomodExecutorFailed,
    /// Building the raid-watch REST executor (join-burst observer).
    RaidExecutorFailed,
    /// Anything that fails once the shard is running.
    GatewayRuntimeFailed,
    /// The gateway task panicked.
    GatewayTaskPanicked,
}

impl FailureClass {
    /// Every variant, for the vocabulary tests.
    #[cfg(test)]
    pub const ALL: [Self; 12] = [
        Self::StoreUnavailable,
        Self::GatewayPoolConnectFailed,
        Self::CheckpointLoadFailed,
        Self::OnboardingGatesInvalid,
        Self::OnboardingInitFailed,
        Self::CustomCommandsInitFailed,
        Self::MilestonesLoadFailed,
        Self::AutomodConfigInvalid,
        Self::AutomodExecutorFailed,
        Self::RaidExecutorFailed,
        Self::GatewayRuntimeFailed,
        Self::GatewayTaskPanicked,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StoreUnavailable => "store_unavailable",
            Self::GatewayPoolConnectFailed => "gateway_pool_connect_failed",
            Self::CheckpointLoadFailed => "checkpoint_load_failed",
            Self::OnboardingGatesInvalid => "onboarding_gates_invalid",
            Self::OnboardingInitFailed => "onboarding_init_failed",
            Self::CustomCommandsInitFailed => "custom_commands_init_failed",
            Self::MilestonesLoadFailed => "milestones_load_failed",
            Self::AutomodConfigInvalid => "automod_config_invalid",
            Self::AutomodExecutorFailed => "automod_executor_failed",
            Self::RaidExecutorFailed => "raid_executor_failed",
            Self::GatewayRuntimeFailed => "gateway_runtime_failed",
            Self::GatewayTaskPanicked => "gateway_task_panicked",
        }
    }
}

/// `/readyz` `gateway_failure` value: `{"phase":"durable_gateway","class":"…"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct GatewayFailure {
    pub phase: FailurePhase,
    pub class: FailureClass,
}

impl GatewayFailure {
    #[must_use]
    pub const fn durable_gateway(class: FailureClass) -> Self {
        Self {
            phase: FailurePhase::DurableGateway,
            class,
        }
    }
}

/// The last failure, shared between the gateway task (writer) and `/readyz`
/// (reader). A plain mutex over a `Copy` value: held for a copy, never across
/// an await.
#[derive(Debug, Clone, Default)]
pub struct FailureSlot(Arc<Mutex<Option<GatewayFailure>>>);

impl FailureSlot {
    pub fn record(&self, failure: GatewayFailure) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(failure);
    }

    #[must_use]
    pub fn get(&self) -> Option<GatewayFailure> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A failed gateway step: the fixed class plus the underlying error, which is
/// passed on to the supervisor but never formatted.
pub struct StepFailure {
    pub class: FailureClass,
    pub error: sqlx::Error,
}

/// Pair the fixed class with the source error, which is passed on to the
/// supervisor but never formatted: SQLx errors can carry a connection URL.
/// The class is logged once, by `publish_gateway_failure`.
pub fn step_failure(class: FailureClass, error: sqlx::Error) -> StepFailure {
    StepFailure { class, error }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_token(value: &str) -> bool {
        (1..=32).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    #[test]
    fn every_class_is_a_distinct_short_token() {
        let mut seen = std::collections::BTreeSet::new();
        for class in FailureClass::ALL {
            assert!(is_token(class.as_str()), "{class:?}");
            assert!(seen.insert(class.as_str()), "duplicate token {class:?}");
        }
        assert!(is_token(FailurePhase::DurableGateway.as_str()));
    }

    #[test]
    fn class_list_is_exhaustive() {
        // `index` stops compiling when a variant is added; ALL must then grow.
        fn index(class: FailureClass) -> usize {
            match class {
                FailureClass::StoreUnavailable => 0,
                FailureClass::GatewayPoolConnectFailed => 1,
                FailureClass::CheckpointLoadFailed => 2,
                FailureClass::OnboardingGatesInvalid => 3,
                FailureClass::OnboardingInitFailed => 4,
                FailureClass::CustomCommandsInitFailed => 5,
                FailureClass::MilestonesLoadFailed => 6,
                FailureClass::AutomodConfigInvalid => 7,
                FailureClass::AutomodExecutorFailed => 8,
                FailureClass::RaidExecutorFailed => 9,
                FailureClass::GatewayRuntimeFailed => 10,
                FailureClass::GatewayTaskPanicked => 11,
            }
        }
        for (position, class) in FailureClass::ALL.into_iter().enumerate() {
            assert_eq!(index(class), position);
        }
        assert_eq!(FailureClass::ALL.len(), 12);
    }

    #[test]
    fn serialized_form_matches_the_token_and_the_documented_shape() {
        for class in FailureClass::ALL {
            let value = serde_json::to_value(GatewayFailure::durable_gateway(class)).unwrap();
            assert_eq!(
                value,
                serde_json::json!({"phase": "durable_gateway", "class": class.as_str()})
            );
        }
    }

    /// `docs/startup-diagnostics.md`, read at compile time so the suite stays offline.
    const STARTUP_DIAGNOSTICS: &str = include_str!("../../../docs/startup-diagnostics.md");

    /// Variants deliberately absent from the doc's error-class table, each with
    /// the reason. Empty: every class has a row.
    const TABLE_EXEMPTIONS: [(FailureClass, &str); 0] = [];

    /// The `Error class` cell of every row in the doc's fatal-diagnostics table.
    /// Panics when the table is missing or reshaped, so a rewrite of it fails
    /// closed instead of passing with nothing to compare.
    fn documented_error_classes(doc: &str) -> Vec<&str> {
        let mut lines = doc.lines();
        assert!(
            lines.any(|line| line.trim() == "| Boundary | Error class |"),
            "docs/startup-diagnostics.md lost its `| Boundary | Error class |` table"
        );
        let classes: Vec<&str> = lines
            .skip(1) // the `| --- | --- |` separator
            .take_while(|line| line.starts_with('|'))
            .map(|line| {
                let cell = line
                    .trim_end_matches('|')
                    .rsplit('|')
                    .next()
                    .unwrap_or_default()
                    .trim();
                cell.strip_prefix('`')
                    .and_then(|cell| cell.strip_suffix('`'))
                    .unwrap_or_else(|| panic!("error class cell is not a code span: {line}"))
            })
            .collect();
        assert!(!classes.is_empty(), "the error-class table has no rows");
        classes
    }

    #[test]
    fn every_class_has_a_startup_diagnostics_row_or_an_exemption() {
        let documented = documented_error_classes(STARTUP_DIAGNOSTICS);
        let missing: Vec<&str> = FailureClass::ALL
            .into_iter()
            .filter(|class| !TABLE_EXEMPTIONS.iter().any(|(exempt, _)| exempt == class))
            .map(FailureClass::as_str)
            .filter(|token| !documented.contains(token))
            .collect();
        assert!(
            missing.is_empty(),
            "FailureClass variants with no row in the docs/startup-diagnostics.md error-class \
             table: {missing:?}. Add a `| <boundary> | `<class>` |` row, or list the variant in \
             TABLE_EXEMPTIONS with the reason."
        );
    }

    #[test]
    fn slot_keeps_the_last_failure() {
        let slot = FailureSlot::default();
        assert_eq!(slot.get(), None);
        slot.record(GatewayFailure::durable_gateway(
            FailureClass::CheckpointLoadFailed,
        ));
        let shared = slot.clone();
        shared.record(GatewayFailure::durable_gateway(
            FailureClass::GatewayRuntimeFailed,
        ));
        assert_eq!(
            slot.get(),
            Some(GatewayFailure::durable_gateway(
                FailureClass::GatewayRuntimeFailed
            ))
        );
    }

    // Synthetic red group for the nextest ejection drill (throwaway draft
    // PR, closed unmerged): fails until the `quarantine` input ejects it.
    #[test]
    fn drill_synthetic_red_group_fails_until_ejected() {
        panic!("drill: synthetic red group, eject via quarantine");
    }

    #[test]
    fn step_failure_keeps_the_error_out_of_the_class() {
        let secret = "postgres://user:hunter2@db.internal/app";
        let failure = step_failure(
            FailureClass::GatewayPoolConnectFailed,
            sqlx::Error::InvalidArgument(secret.into()),
        );
        assert_eq!(failure.class.as_str(), "gateway_pool_connect_failed");
        let serialized =
            serde_json::to_string(&GatewayFailure::durable_gateway(failure.class)).unwrap();
        assert!(!serialized.contains("hunter2") && !serialized.contains("postgres"));
    }
}
