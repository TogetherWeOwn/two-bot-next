//! Bounded, definition-free CLI reports. Classification always uses raw identities;
//! display sanitization and truncation can never grant permission to publish.

use std::collections::HashSet;

use serde::Serialize;
use twilight_model::application::command::Command;
use two_bot_discord::command_registry::{command_identity, RegistryDiff};

const MAX_REMOVALS: usize = 100;
const MAX_NAME_CHARS: usize = 64;
pub const REGISTRY_OWNER_ACTION: &str =
    "Use the complete registry owner/cutover workflow to reconcile custom commands; this CLI has no deletion override.";
pub const REQUEST_ACTION: &str =
    "Check REST access and shared send admission, then inspect a fresh diff before any approved apply.";

#[derive(Serialize)]
pub struct Removal {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: u8,
    pub name: String,
    pub name_truncated: bool,
    pub removal_class: &'static str,
}

#[derive(Serialize)]
pub struct ReportError {
    pub code: &'static str,
    pub next_action: &'static str,
}

#[derive(Serialize)]
pub struct Report {
    pub schema_version: u8,
    pub ok: bool,
    pub applied: bool,
    pub current_hash: Option<String>,
    pub desired_hash: Option<String>,
    pub added_count: Option<usize>,
    pub removed_count: Option<usize>,
    pub changed_count: Option<usize>,
    pub removals: Vec<Removal>,
    pub removals_truncated: bool,
    pub has_unrecognized_removals: bool,
    pub next_action: &'static str,
    pub error: Option<ReportError>,
}

impl Report {
    pub fn failure(code: &'static str, next_action: &'static str) -> Self {
        let mut report = Self {
            schema_version: 1,
            ok: false,
            applied: false,
            current_hash: None,
            desired_hash: None,
            added_count: None,
            removed_count: None,
            changed_count: None,
            removals: Vec::new(),
            removals_truncated: false,
            has_unrecognized_removals: false,
            next_action,
            error: None,
        };
        report.reject(code, next_action);
        report
    }

    pub fn snapshot(current: &[Command], diff: &RegistryDiff) -> Self {
        let known_commands = known_builtin_commands();
        let known: HashSet<_> = known_commands
            .iter()
            .map(|command| (u8::from(command.kind), command.name.as_str()))
            .collect();
        let mut report = Self::failure("registry_request_failed", REQUEST_ACTION);
        report.ok = true;
        report.error = None;
        report.current_hash = Some(diff.current_hash.clone());
        report.desired_hash = Some(diff.compiled_hash.clone());
        report.added_count = Some(diff.added.len());
        report.removed_count = Some(diff.removed.len());
        report.changed_count = Some(diff.changed.len());
        // Inspect ALL removals even when reporting is truncated. The key uses
        // the raw type/name exactly as registry_snapshot does, never its label.
        let mut removed: Vec<_> = current
            .iter()
            .filter(|command| diff.removed.contains_key(&command_identity(command)))
            .collect();
        removed.sort_by(|a, b| (u8::from(a.kind), &a.name).cmp(&(u8::from(b.kind), &b.name)));
        for command in removed {
            let is_known = known.contains(&(u8::from(command.kind), command.name.as_str()));
            report.has_unrecognized_removals |= !is_known;
            if report.removals.len() == MAX_REMOVALS {
                report.removals_truncated = true;
                continue;
            }
            report.removals.push(Removal {
                id: command.id.map(|id| id.to_string()),
                kind: command.kind.into(),
                name: display_name(&command.name),
                name_truncated: command.name.chars().count() > MAX_NAME_CHARS,
                removal_class: if is_known {
                    "known_builtin"
                } else {
                    "unrecognized"
                },
            });
        }
        report.next_action = if report.has_unrecognized_removals {
            REGISTRY_OWNER_ACTION
        } else {
            "Review the diff before an approved publish --apply; a matching hash needs no PUT."
        };
        report
    }

    pub fn reject(&mut self, code: &'static str, next_action: &'static str) {
        self.ok = false;
        self.next_action = next_action;
        self.error = Some(ReportError { code, next_action });
    }
}

fn known_builtin_commands() -> Vec<Command> {
    // Same authoritative catalogs as the router, without environment gates.
    // Convert through the publisher rather than assuming future command types.
    let definitions: Vec<_> = two_bot_core::commands::core_commands()
        .into_iter()
        .chain(two_bot_core::feature_commands::feature_commands())
        .chain(two_bot_core::moderation::moderation_commands())
        .chain(two_bot_core::voice_rooms::voice_commands())
        .collect();
    two_bot_discord::publish_commands(&definitions)
}

pub fn display_name(name: &str) -> String {
    name.chars()
        .take(MAX_NAME_CHARS)
        .map(|c| if c.is_control() || c == '@' { '�' } else { c })
        .collect()
}

/// Keep the existing rich human diff for ordinary registries, but bound hostile
/// input and neutralize terminal escapes/mentions. JSON never includes bodies.
pub fn human_diff(diff: &RegistryDiff) -> String {
    let rendered = diff.render();
    let mut output: String = rendered
        .chars()
        .take(16_384)
        .map(|c| {
            if c == '@' || c.is_control() && c != '\n' && c != '\t' {
                '�'
            } else {
                c
            }
        })
        .collect();
    if rendered.chars().count() > 16_384 {
        output.push_str("\n[diff truncated; inspect through the registry owner]\n");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::commands::CommandDefinition;
    use two_bot_discord::{command_registry::diff_commands, publish_commands};

    fn command(name: &str) -> Command {
        publish_commands(&[CommandDefinition::new(name, "Fixture.")]).remove(0)
    }

    #[test]
    fn every_catalog_is_known_but_context_menus_and_unpublished_assistant_are_not() {
        let definitions: Vec<_> = two_bot_core::commands::core_commands()
            .into_iter()
            .chain(two_bot_core::feature_commands::feature_commands())
            .chain(two_bot_core::moderation::moderation_commands())
            .chain(two_bot_core::voice_rooms::voice_commands())
            .collect();
        for definition in definitions {
            let current = publish_commands(&[definition]);
            let diff = diff_commands(&current, &[]).unwrap();
            let report = Report::snapshot(&current, &diff);
            assert!(!report.has_unrecognized_removals);
            assert_eq!(report.removals[0].removal_class, "known_builtin");
        }
        let mut context_menu = command("help");
        context_menu.kind = 2.into();
        for current in [vec![context_menu], vec![command("templateassistant")]] {
            let diff = diff_commands(&current, &[]).unwrap();
            assert!(Report::snapshot(&current, &diff).has_unrecognized_removals);
        }
    }

    #[test]
    fn namespace_equals_the_all_enabled_router_without_a_second_name_list() {
        let vars = [
            ("TWO_AUTOMATIONS", "1"),
            ("TWO_ANNOUNCEMENTS", "1"),
            ("TWO_COMMUNITY_SCORECARD", "1"),
            ("TWO_MODERATION", "1"),
            ("TWO_OWEN_USER_ID", "123456789012345678"),
            ("TWO_VOICE", "1"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        let desired = super::super::desired_definitions(2222, &vars).unwrap();
        let desired: HashSet<_> = publish_commands(&desired)
            .iter()
            .map(command_identity)
            .collect();
        let known: HashSet<_> = known_builtin_commands()
            .iter()
            .map(command_identity)
            .collect();
        assert_eq!(known, desired);
    }

    #[test]
    fn desired_custom_identity_is_not_a_removal() {
        let current = vec![command("zz-custom")];
        let diff = diff_commands(&current, &current).unwrap();
        let report = Report::snapshot(&current, &diff);
        assert!(!report.has_unrecognized_removals);
        assert!(report.removals.is_empty());
        assert_eq!(report.removed_count, Some(0));
    }

    #[test]
    fn bounded_reports_classify_even_the_omitted_tail() {
        let mut current: Vec<_> = (0..100)
            .map(|index| command(&format!("aa-{index:03}-{}", "🦀".repeat(400))))
            .collect();
        current.push(command("zz-last"));
        let diff = diff_commands(&current, &[]).unwrap();
        let report = Report::snapshot(&current, &diff);
        assert_eq!(report.removed_count, Some(101));
        assert_eq!(report.removals.len(), 100);
        assert!(report.removals_truncated);
        assert!(report.has_unrecognized_removals);
        assert!(report.removals.iter().all(|r| r.name_truncated));
        assert!(serde_json::to_vec(&report).unwrap().len() < 65_536);
        let rendered = human_diff(&diff);
        assert!(rendered.chars().count() < 16_500);
        assert!(rendered.contains("diff truncated"));
    }

    #[test]
    fn labels_neutralize_mentions_and_terminal_controls_without_changing_classification() {
        let current = vec![command("@everyone\n\u{1b}[31m")];
        let diff = diff_commands(&current, &[]).unwrap();
        let report = Report::snapshot(&current, &diff);
        assert!(report.has_unrecognized_removals);
        assert!(!report.removals[0].name.contains(['@', '\n', '\u{1b}']));
        let text = human_diff(&diff);
        assert!(!text.contains(['@', '\u{1b}']));
        assert!(
            !text.contains("everyone\n"),
            "remote names cannot forge diff lines"
        );
        let error = Report::failure("registry_request_failed", REQUEST_ACTION);
        assert_eq!(error.current_hash, None);
        assert_eq!(error.removed_count, None);
        assert!(!error.ok);
    }
}
