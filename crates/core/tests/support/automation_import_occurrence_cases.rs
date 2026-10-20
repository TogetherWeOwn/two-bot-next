//! PROTECTED: failing-first acceptance vectors. Test-author-owned; consume unchanged
//! with the implementation. Expectations are hand-derived, not produced by the planner.

#![allow(dead_code)]

use serde_json::{json, Value};
use two_bot_core::custom_commands::StoredCommand;

pub const GUILD: &str = "900000000000000101";
pub const DESCRIPTION: &str = "Fixture command";

pub struct Case {
    pub name: &'static str,
    pub existing: Vec<StoredCommand>,
    pub document: Value,
    pub creates: &'static [&'static str],
    pub updates: &'static [&'static str],
    pub unchanged: &'static [&'static str],
    pub rejected: &'static [(&'static str, &'static str)],
    pub imported: usize,
    pub skipped: usize,
    pub conflicts: &'static [&'static str],
    pub final_rows: Vec<StoredCommand>,
    // Original per-name occurrence ordinal, including refused parsed occurrences.
    // The action distinguishes fresh rows from rewrites, including convergence.
    pub audits: &'static [(&'static str, usize, &'static str)],
}

pub fn command(name: &str, template: &str, trigger: Option<&str>) -> Value {
    json!({"name": name, "description": DESCRIPTION, "template": template,
        "text_trigger": trigger})
}

pub fn document(commands: Vec<Value>, overwrite: bool) -> Value {
    json!({"version": 1, "commands": commands, "schedules": [], "overwrite": overwrite})
}

pub fn row(name: &str, template: &str, trigger: Option<&str>, enabled: bool) -> StoredCommand {
    StoredCommand {
        guild_id: GUILD.into(),
        name: name.into(),
        description: DESCRIPTION.into(),
        template: template.into(),
        text_trigger: trigger.map(str::to_owned),
        enabled,
    }
}

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "accepted_then_invalid_same_name",
            existing: vec![],
            document: document(
                vec![
                    command("faq", "A {user}", Some("!faq")),
                    command("faq", "{usre}", Some("!faq")),
                ],
                false,
            ),
            creates: &["faq"],
            updates: &[],
            unchanged: &[],
            rejected: &[("faq", "invalid_template")],
            imported: 1,
            skipped: 1,
            conflicts: &[],
            final_rows: vec![row("faq", "A {user}", Some("!faq"), true)],
            audits: &[("faq", 0, "command.create")],
        },
        Case {
            name: "unchanged_does_not_authorize_refused_overwrite",
            existing: vec![row("faq", "A", Some("!faq"), false)],
            document: document(
                vec![
                    command("faq", "A", Some("!faq")),
                    command("faq", "B", Some("!faq")),
                ],
                false,
            ),
            creates: &[],
            updates: &[],
            unchanged: &["faq"],
            rejected: &[("faq", "would_overwrite")],
            imported: 1,
            skipped: 1,
            conflicts: &["faq"],
            final_rows: vec![row("faq", "A", Some("!faq"), false)],
            audits: &[("faq", 0, "command.update")],
        },
        Case {
            name: "accepted_then_trigger_conflicting_duplicate",
            existing: vec![row("owner", "Owned", Some("!taken"), false)],
            document: document(
                vec![
                    command("faq", "A", Some("!faq")),
                    command("faq", "B", Some("!taken")),
                ],
                true,
            ),
            creates: &["faq"],
            updates: &[],
            unchanged: &[],
            rejected: &[("faq", "trigger_in_use")],
            imported: 1,
            skipped: 1,
            conflicts: &["faq"],
            final_rows: vec![
                row("faq", "A", Some("!faq"), true),
                row("owner", "Owned", Some("!taken"), false),
            ],
            audits: &[("faq", 0, "command.create")],
        },
        Case {
            name: "duplicates_observe_prior_accepted_content",
            existing: vec![],
            document: document(
                vec![
                    command("faq", "A", Some("!first")),
                    command("faq", "B", Some("!second")),
                    command("faq", "B", Some("!second")),
                ],
                true,
            ),
            creates: &["faq"],
            updates: &["faq"],
            unchanged: &["faq"],
            rejected: &[],
            imported: 3,
            skipped: 0,
            conflicts: &[],
            final_rows: vec![row("faq", "B", Some("!second"), true)],
            audits: &[
                ("faq", 0, "command.create"),
                ("faq", 1, "command.update"),
                ("faq", 2, "command.update"),
            ],
        },
        Case {
            name: "fresh_duplicate_requires_overwrite_for_changed_content",
            existing: vec![],
            document: document(
                vec![
                    command("faq", "A", Some("!faq")),
                    command("faq", "B", Some("!faq")),
                    command("faq", "A", Some("!faq")),
                ],
                false,
            ),
            creates: &["faq"],
            updates: &[],
            unchanged: &["faq"],
            rejected: &[("faq", "would_overwrite")],
            imported: 2,
            skipped: 1,
            conflicts: &["faq"],
            final_rows: vec![row("faq", "A", Some("!faq"), true)],
            audits: &[("faq", 0, "command.create"), ("faq", 2, "command.update")],
        },
        Case {
            name: "accepted_update_releases_trigger_for_next_first_claimant",
            existing: vec![row("faq", "A", Some("!old"), false)],
            document: document(
                vec![
                    command("faq", "B", Some("!new")),
                    command("other", "C", Some("!old")),
                    command("third", "D", Some("!old")),
                ],
                true,
            ),
            creates: &["other"],
            updates: &["faq"],
            unchanged: &[],
            rejected: &[("third", "trigger_in_use")],
            imported: 2,
            skipped: 1,
            conflicts: &["third"],
            final_rows: vec![
                row("faq", "B", Some("!new"), false),
                row("other", "C", Some("!old"), true),
            ],
            audits: &[("faq", 0, "command.update"), ("other", 0, "command.create")],
        },
        Case {
            name: "rejected_update_does_not_release_existing_trigger",
            existing: vec![
                row("holder", "Held", Some("!taken"), true),
                row("owner", "Owned", Some("!old"), false),
            ],
            document: document(
                vec![
                    command("owner", "B", Some("!taken")),
                    command("other", "C", Some("!old")),
                    command("fresh", "D", Some("!free")),
                ],
                true,
            ),
            creates: &["fresh"],
            updates: &[],
            unchanged: &[],
            rejected: &[("owner", "trigger_in_use"), ("other", "trigger_in_use")],
            imported: 1,
            skipped: 2,
            conflicts: &["owner", "other"],
            final_rows: vec![
                row("fresh", "D", Some("!free"), true),
                row("holder", "Held", Some("!taken"), true),
                row("owner", "Owned", Some("!old"), false),
            ],
            audits: &[("fresh", 0, "command.create")],
        },
        Case {
            name: "refused_overwrite_does_not_acquire_trigger",
            existing: vec![row("faq", "A", Some("!faq"), false)],
            document: document(
                vec![
                    command("faq", "B", Some("!unused")),
                    command("other", "C", Some("!unused")),
                ],
                false,
            ),
            creates: &["other"],
            updates: &[],
            unchanged: &[],
            rejected: &[("faq", "would_overwrite")],
            imported: 1,
            skipped: 1,
            conflicts: &["faq"],
            final_rows: vec![
                row("faq", "A", Some("!faq"), false),
                row("other", "C", Some("!unused"), true),
            ],
            audits: &[("other", 0, "command.create")],
        },
        Case {
            name: "invalid_first_does_not_claim_trigger_or_shift_occurrence_identity",
            existing: vec![],
            document: document(
                vec![
                    command("faq", "{usre}", Some("!shared")),
                    command("other", "C", Some("!shared")),
                    command("faq", "A", None),
                ],
                false,
            ),
            creates: &["other", "faq"],
            updates: &[],
            unchanged: &[],
            rejected: &[("faq", "invalid_template")],
            imported: 2,
            skipped: 1,
            conflicts: &[],
            final_rows: vec![
                row("faq", "A", None, true),
                row("other", "C", Some("!shared"), true),
            ],
            audits: &[("other", 0, "command.create"), ("faq", 1, "command.create")],
        },
        Case {
            name: "valid_control_same_content_converges_without_overwrite",
            existing: vec![row("faq", "A", Some("!faq"), false)],
            document: document(
                vec![
                    command("faq", "A", Some("!faq")),
                    command("fresh", "Hi {server}", None),
                ],
                false,
            ),
            creates: &["fresh"],
            updates: &[],
            unchanged: &["faq"],
            rejected: &[],
            imported: 2,
            skipped: 0,
            conflicts: &[],
            final_rows: vec![
                row("faq", "A", Some("!faq"), false),
                row("fresh", "Hi {server}", None, true),
            ],
            audits: &[("faq", 0, "command.update"), ("fresh", 0, "command.create")],
        },
    ]
}
