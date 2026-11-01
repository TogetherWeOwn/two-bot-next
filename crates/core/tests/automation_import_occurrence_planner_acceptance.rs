//! Protected failing-first acceptance tests for versioned command imports.
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file or its shared vectors. Do not merge these files alone while red.
//! Catches name-set authorization, stale content/trigger snapshots, and ownership
//! changes from rejected entries. No database, network, or implementation doubles.

#[path = "support/automation_import_occurrence_cases.rs"]
mod cases;

use two_bot_core::automation_transfer::{diff_import, max_import_entries, parse_import_document};
use two_bot_core::custom_commands::{max_custom_commands, reserved_command_names};

fn assert_case(name: &str) {
    let case = cases::cases()
        .into_iter()
        .find(|case| case.name == name)
        .expect("named acceptance vector exists");
    // Serialize then decode the wire document: do not construct ParsedImport.
    let bytes = serde_json::to_vec(&case.document).expect("encode versioned fixture");
    let wire = serde_json::from_slice(&bytes).expect("decode versioned fixture");
    let parsed = parse_import_document(&wire, max_import_entries()).expect("parse v1 document");
    assert_eq!(
        parsed.entries.len(),
        case.document["commands"]
            .as_array()
            .expect("commands")
            .len()
    );
    assert_eq!(
        parsed.invalid_entries, 0,
        "invalid templates remain occurrence-level decisions"
    );
    assert!(
        parsed.translation_conflicts.is_empty(),
        "v1 does not rename duplicates"
    );
    let diff = diff_import(
        &case.existing,
        &parsed,
        &reserved_command_names(),
        parsed.overwrite,
    )
    .expect("bounded acceptance vector plans");
    assert_eq!(diff.to_create, case.creates, "{}: creates", case.name);
    assert_eq!(diff.to_update, case.updates, "{}: updates", case.name);
    assert_eq!(diff.unchanged, case.unchanged, "{}: convergence", case.name);
    let rejections: Vec<(&str, &str)> = diff
        .rejected
        .iter()
        .map(|refusal| (refusal.name.as_str(), refusal.code.as_str()))
        .collect();
    assert_eq!(
        rejections, case.rejected,
        "{}: exact occurrence refusals",
        case.name
    );
    assert_eq!(diff.invalid_entries, 0);
}

macro_rules! planner_case {
    ($name:ident) => {
        #[test]
        fn $name() {
            assert_case(stringify!($name));
        }
    };
}

planner_case!(accepted_then_invalid_same_name);
planner_case!(unchanged_does_not_authorize_refused_overwrite);
planner_case!(accepted_then_trigger_conflicting_duplicate);
planner_case!(duplicates_observe_prior_accepted_content);
planner_case!(fresh_duplicate_requires_overwrite_for_changed_content);
planner_case!(accepted_update_releases_trigger_for_next_first_claimant);
planner_case!(rejected_update_does_not_release_existing_trigger);
planner_case!(refused_overwrite_does_not_acquire_trigger);
planner_case!(invalid_first_does_not_claim_trigger_or_shift_occurrence_identity);
planner_case!(valid_control_same_content_converges_without_overwrite);

#[test]
fn capacity_counts_unique_accepted_names_not_duplicate_occurrences() {
    let existing = vec![cases::row("seed", "S", None, true)];
    let max = max_custom_commands(reserved_command_names().len());
    // One existing row + max-1 new names fits. The last occurrence converges
    // with fresh-0 instead of consuming another command slot.
    let mut commands: Vec<_> = (0..max - 1)
        .map(|i| cases::command(&format!("fresh-{i}"), "T", None))
        .collect();
    commands.push(cases::command("fresh-0", "T", None));
    let parsed =
        parse_import_document(&cases::document(commands, false), max).expect("parse at cap");
    let diff = diff_import(&existing, &parsed, &reserved_command_names(), false)
        .expect("duplicate occurrence must not consume capacity twice");
    assert_eq!(diff.to_create.len(), max - 1);
    assert_eq!(diff.unchanged, ["fresh-0"]);
    assert!(diff.to_update.is_empty());
    assert!(diff.rejected.is_empty());
}

#[test]
fn capacity_refuses_one_more_unique_name_at_the_reserved_boundary() {
    let existing = vec![cases::row("seed", "S", None, true)];
    let max = max_custom_commands(reserved_command_names().len());
    let commands: Vec<_> = (0..max)
        .map(|i| cases::command(&format!("fresh-{i}"), "T", None))
        .collect();
    let parsed = parse_import_document(&cases::document(commands, false), max)
        .expect("one-over-cap planner fixture fits the import-entry limit");

    assert!(
        diff_import(&existing, &parsed, &reserved_command_names(), false).is_err(),
        "one more unique command than the reserved capacity must be refused"
    );
}
