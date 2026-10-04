//! Health model shared by /health and /readyz.
//!
//! `/health` is liveness (the process answers). `/readyz` is readiness: every
//! listed component must report [`ComponentStatus::Ready`], otherwise the
//! endpoint returns 503 and the Container supervisor / DO keepalive treats
//! the instance as not serving.

use serde::Serialize;

/// Readiness of one component (database pool, gateway shard, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
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

/// Fixed component identities for the offline voice-readiness contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceComponent {
    Gateway,
    Store,
    VoiceCore,
}

impl VoiceComponent {
    fn name(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Store => "store",
            Self::VoiceCore => "voice_core",
        }
    }
}

/// Synthetic or adapter-supplied state; constructing this never probes a service.
/// All three components are required, including a voice core not yet configured.
#[derive(Debug, Clone, Copy)]
pub struct VoiceReadiness {
    pub gateway: ComponentStatus,
    pub store: ComponentStatus,
    pub voice_core: ComponentStatus,
}

impl VoiceReadiness {
    /// Uses the existing all-components-ready rule, not the liveness of /health.
    #[must_use]
    pub fn report(self) -> VoiceHealthReport {
        let components = [
            (VoiceComponent::Gateway, self.gateway),
            (VoiceComponent::Store, self.store),
            (VoiceComponent::VoiceCore, self.voice_core),
        ];
        VoiceHealthReport {
            health: HealthReport::new(
                components
                    .iter()
                    .map(|(component, status)| (component.name().to_owned(), *status))
                    .collect(),
            ),
            diagnostics: components
                .into_iter()
                .filter(|(_, status)| *status != ComponentStatus::Ready)
                .map(|(component, status)| VoiceDiagnostic::ComponentNotReady { component, status })
                .collect(),
        }
    }
}

/// A sanitized snapshot for V10 health/setup consumers. No runtime is wired here.
#[derive(Debug, Clone, Serialize)]
pub struct VoiceHealthReport {
    pub health: HealthReport,
    pub diagnostics: Vec<VoiceDiagnostic>,
}

impl VoiceHealthReport {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.health.ready()
    }
}

/// Permissions named by voice health and write diagnostics, not raw Discord error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoicePermission {
    ManageChannels,
    MoveMembers,
    ManageRoles,
    ViewChannel,
    Connect,
}

/// Preserves the level causing a refusal; category names/IDs stay with the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoicePermissionScope {
    Guild,
    Category,
    Channel,
}

/// The adapter selects a kind from structured failure information, never by
/// guessing from a source message. Unknown failures must use `Unexpected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceFailureKind {
    ComponentUnavailable(VoiceComponent),
    MissingPermission {
        permission: VoicePermission,
        scope: VoicePermissionScope,
    },
    CategoryFull,
    RateLimited,
    Unexpected,
}

/// Closed diagnostic vocabulary: no source messages, URLs, names or credentials
/// can be embedded. Safe to serialize or format for a V10 error notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, thiserror::Error)]
#[serde(tag = "category", rename_all = "snake_case")]
pub enum VoiceDiagnostic {
    #[error("{component:?} is not ready ({status:?}).")]
    ComponentNotReady {
        component: VoiceComponent,
        status: ComponentStatus,
    },
    #[error("Missing {permission:?} permission at {scope:?} level.")]
    MissingPermission {
        permission: VoicePermission,
        scope: VoicePermissionScope,
    },
    #[error("Category is full; use another creator channel in a different category.")]
    CategoryFull,
    #[error("Voice action is rate limited; wait before retrying.")]
    RateLimited,
    #[error("Voice action failed unexpectedly.")]
    Unexpected,
}

/// Discard untrusted failure text in its entirety, rather than regex-redacting
/// known token formats. Only the typed kind survives into notices or diagnostics.
/// Notice destinations, retry budgets and effective-permission checks are V10
/// runtime integration work (docs/voice-rooms.md §V10 and Discord API notes).
#[must_use]
pub fn classify_voice_error(kind: VoiceFailureKind, _untrusted_detail: &str) -> VoiceDiagnostic {
    match kind {
        VoiceFailureKind::ComponentUnavailable(component) => VoiceDiagnostic::ComponentNotReady {
            component,
            status: ComponentStatus::Down,
        },
        VoiceFailureKind::MissingPermission { permission, scope } => {
            VoiceDiagnostic::MissingPermission { permission, scope }
        }
        VoiceFailureKind::CategoryFull => VoiceDiagnostic::CategoryFull,
        VoiceFailureKind::RateLimited => VoiceDiagnostic::RateLimited,
        VoiceFailureKind::Unexpected => VoiceDiagnostic::Unexpected,
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

    #[test]
    fn starting_counts_as_not_ready() {
        let report = HealthReport::new(vec![
            ("process".to_owned(), ComponentStatus::Ready),
            ("gateway".to_owned(), ComponentStatus::Starting),
        ]);
        assert!(!report.ready());
    }

    #[test]
    fn degraded_report_serialization_names_the_failing_component() {
        // Pins the /readyz breakdown shape: the handler maps ready() to
        // 200/503 and serves this JSON, so the 503 body must name the
        // degraded component with a lowercase status — never a bare string.
        let report = HealthReport::new(vec![
            ("process".to_owned(), ComponentStatus::Ready),
            ("gateway".to_owned(), ComponentStatus::Down),
        ]);
        assert!(!report.ready());
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "components": [["process", "ready"], ["gateway", "down"]],
            })
        );
        let body = serde_json::to_string(&report).unwrap();
        assert!(body.contains("\"gateway\""), "503 must name {body}");
        assert!(body.contains("\"down\""));
    }

    #[test]
    fn all_ready_report_serializes_for_200() {
        let report = HealthReport::new(vec![
            ("process".to_owned(), ComponentStatus::Ready),
            ("gateway".to_owned(), ComponentStatus::Ready),
        ]);
        assert!(report.ready());
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "components": [["process", "ready"], ["gateway", "ready"]],
            })
        );
    }

    fn voice_state() -> VoiceReadiness {
        VoiceReadiness {
            gateway: ComponentStatus::Ready,
            store: ComponentStatus::Ready,
            voice_core: ComponentStatus::Ready,
        }
    }

    #[test]
    fn voice_ready_requires_all_three_components() {
        let report = voice_state().report();
        assert!(report.ready());
        assert!(report.diagnostics.is_empty());
        assert_eq!(
            report.health.components,
            vec![
                ("gateway".to_owned(), ComponentStatus::Ready),
                ("store".to_owned(), ComponentStatus::Ready),
                ("voice_core".to_owned(), ComponentStatus::Ready),
            ]
        );
    }

    #[test]
    fn voice_disconnected_gateway_is_not_ready() {
        let report = VoiceReadiness {
            gateway: ComponentStatus::Down,
            ..voice_state()
        }
        .report();
        assert!(!report.ready());
        assert_eq!(
            report.diagnostics,
            vec![VoiceDiagnostic::ComponentNotReady {
                component: VoiceComponent::Gateway,
                status: ComponentStatus::Down,
            }]
        );
    }

    #[test]
    fn voice_failed_store_is_not_ready() {
        let report = VoiceReadiness {
            store: ComponentStatus::Down,
            ..voice_state()
        }
        .report();
        assert!(!report.ready());
        assert_eq!(
            report.diagnostics,
            vec![VoiceDiagnostic::ComponentNotReady {
                component: VoiceComponent::Store,
                status: ComponentStatus::Down,
            }]
        );
    }

    #[test]
    fn voice_unavailable_core_is_not_ready() {
        let report = VoiceReadiness {
            voice_core: ComponentStatus::Down,
            ..voice_state()
        }
        .report();
        assert!(!report.ready());
        assert_eq!(
            report.diagnostics,
            vec![VoiceDiagnostic::ComponentNotReady {
                component: VoiceComponent::VoiceCore,
                status: ComponentStatus::Down,
            }]
        );
    }

    #[test]
    fn voice_readiness_all_27_snapshots_preserve_every_failure() {
        let statuses = [
            ComponentStatus::Ready,
            ComponentStatus::Starting,
            ComponentStatus::Down,
        ];
        for gateway in statuses {
            for store in statuses {
                for voice_core in statuses {
                    let report = VoiceReadiness {
                        gateway,
                        store,
                        voice_core,
                    }
                    .report();
                    let components = [
                        (VoiceComponent::Gateway, gateway),
                        (VoiceComponent::Store, store),
                        (VoiceComponent::VoiceCore, voice_core),
                    ];
                    let expected: Vec<_> = components
                        .into_iter()
                        .filter(|(_, status)| *status != ComponentStatus::Ready)
                        .map(|(component, status)| VoiceDiagnostic::ComponentNotReady {
                            component,
                            status,
                        })
                        .collect();
                    assert_eq!(report.ready(), expected.is_empty());
                    assert_eq!(report.diagnostics, expected);
                    assert_eq!(report.health.components.len(), 3);
                }
            }
        }
    }

    #[test]
    fn voice_recovery_has_no_stale_diagnostics() {
        let down = VoiceReadiness {
            gateway: ComponentStatus::Down,
            ..voice_state()
        };
        assert!(!down.report().ready());
        let recovered = VoiceReadiness {
            gateway: ComponentStatus::Ready,
            ..down
        }
        .report();
        assert!(recovered.ready());
        assert!(recovered.diagnostics.is_empty());
    }

    fn assert_sanitized(kind: VoiceFailureKind, expected: VoiceDiagnostic) {
        let detail = "Authorization: Bot SYNTHETIC_DISCORD_TOKEN_MARKER \
                      postgres://agent:SYNTHETIC_PASSWORD_MARKER@invalid/db \
                      https://invalid/?key=SYNTHETIC_QUERY_MARKER\n\
                      @everyone SYNTHETIC_UNKNOWN_SECRET_MARKER";
        let diagnostic = classify_voice_error(kind, detail);
        assert_eq!(diagnostic, expected);
        assert_eq!(diagnostic, classify_voice_error(kind, ""));
        let rendered = format!(
            "{} {:?} {}",
            diagnostic,
            diagnostic,
            serde_json::to_string(&diagnostic).unwrap()
        );
        for forbidden in [
            "SYNTHETIC_DISCORD_TOKEN_MARKER",
            "SYNTHETIC_PASSWORD_MARKER",
            "SYNTHETIC_QUERY_MARKER",
            "SYNTHETIC_UNKNOWN_SECRET_MARKER",
            "Authorization",
            "postgres://",
            "https://",
            "@everyone",
        ] {
            assert!(!rendered.contains(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn voice_component_errors_discard_all_source_text() {
        for component in [
            VoiceComponent::Gateway,
            VoiceComponent::Store,
            VoiceComponent::VoiceCore,
        ] {
            assert_sanitized(
                VoiceFailureKind::ComponentUnavailable(component),
                VoiceDiagnostic::ComponentNotReady {
                    component,
                    status: ComponentStatus::Down,
                },
            );
        }
    }

    #[test]
    fn voice_permission_errors_preserve_only_permission_and_scope() {
        for permission in [
            VoicePermission::ManageChannels,
            VoicePermission::MoveMembers,
            VoicePermission::ManageRoles,
            VoicePermission::ViewChannel,
        ] {
            for scope in [
                VoicePermissionScope::Guild,
                VoicePermissionScope::Category,
                VoicePermissionScope::Channel,
            ] {
                assert_sanitized(
                    VoiceFailureKind::MissingPermission { permission, scope },
                    VoiceDiagnostic::MissingPermission { permission, scope },
                );
            }
        }
    }

    #[test]
    fn voice_other_errors_are_closed_categories() {
        for (kind, expected) in [
            (
                VoiceFailureKind::CategoryFull,
                VoiceDiagnostic::CategoryFull,
            ),
            (VoiceFailureKind::RateLimited, VoiceDiagnostic::RateLimited),
            (VoiceFailureKind::Unexpected, VoiceDiagnostic::Unexpected),
        ] {
            assert_sanitized(kind, expected);
        }
        assert!(VoiceDiagnostic::CategoryFull
            .to_string()
            .contains("different category"));
    }

    #[test]
    fn voice_diagnostic_json_uses_stable_categories() {
        assert_eq!(
            serde_json::to_value(VoiceDiagnostic::MissingPermission {
                permission: VoicePermission::MoveMembers,
                scope: VoicePermissionScope::Category,
            })
            .unwrap(),
            serde_json::json!({
                "category": "missing_permission",
                "permission": "move_members",
                "scope": "category",
            })
        );
        assert_eq!(
            serde_json::to_value(VoiceDiagnostic::ComponentNotReady {
                component: VoiceComponent::VoiceCore,
                status: ComponentStatus::Starting,
            })
            .unwrap(),
            serde_json::json!({
                "category": "component_not_ready",
                "component": "voice_core",
                "status": "starting",
            })
        );
        assert_eq!(
            serde_json::to_value(VoiceDiagnostic::Unexpected).unwrap(),
            serde_json::json!({ "category": "unexpected" })
        );
    }
}
