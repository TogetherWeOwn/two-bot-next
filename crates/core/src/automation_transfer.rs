//! Versioned custom-command export plus dry-run import planning for the
//! `automations.export` / `automations.import` internal actions.
//!
//! Ports legacy `AutomationService::importMee6` / `exportCommands`
//! (`src/automations/service.ts`, TOG-1648) and the `automationsImport` /
//! `automationsExport` envelopes (`src/internal/actions.ts`). No Discord I/O,
//! no SQL, no clock: export reads caller-supplied rows, import planning is a
//! pure diff over caller-supplied rows, and the transactional apply lives in
//! `custom_command_service::import` (db-gated). The HTTP receiver (route card)
//! owns authentication, capability flags, idempotency claims and audit commit.
//!
//! Export is a stable versioned document (`EXPORT_VERSION`). Import accepts
//! that document or a raw MEE6 payload (array, or object with a `commands`
//! array) translated with [`crate::mee6`]. Dry-run diff is the default path:
//! nothing here writes. Scheduled-message import is a typed extension point
//! ([`PendingSchedule`]) until the schedules slice lands.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::custom_commands::{
    error_code, max_custom_commands, reserved_command_names, validate_put_input, CommandError,
    PutCommandInput, StoredCommand,
};
use super::mee6::{parse_mee6_entry, translate_export, Mee6CommandInput};

/// Version of [`ExportDocument`]. Import refuses anything else.
pub const EXPORT_VERSION: u32 = 1;

/// One exported definition. Field names are the stable wire contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedCommand {
    pub name: String,
    pub description: String,
    pub template: String,
    pub text_trigger: Option<String>,
}

/// Scheduled-message payload carried for a future importer. Opaque on purpose:
/// the schedules slice owns its schema, and import refuses a non-empty list
/// until that slice defines validation and storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSchedule(pub serde_json::Value);

/// Stable export envelope. `schedules` serializes only when non-empty, so
/// current exports are exactly `{version, commands}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportDocument {
    pub version: u32,
    pub commands: Vec<ExportedCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<PendingSchedule>,
}

/// Serialize the guild's definitions, ascending by name regardless of input
/// order, so exports are byte-stable for a given state.
#[must_use]
pub fn export_document(rows: &[StoredCommand]) -> ExportDocument {
    let mut commands: Vec<ExportedCommand> = rows
        .iter()
        .map(|row| ExportedCommand {
            name: row.name.clone(),
            description: row.description.clone(),
            template: row.template.clone(),
            text_trigger: row.text_trigger.clone(),
        })
        .collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    ExportDocument {
        version: EXPORT_VERSION,
        commands,
        schedules: Vec::new(),
    }
}

/// Malformed import envelope (legacy `automationsImport` pre-checks). Messages
/// are admin-facing; they never echo entry content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImportParseError {
    #[error("import document must be an array or an object with a commands array")]
    NotAnImportDocument,
    #[error("\"commands\" must be an array of command objects")]
    EntriesNotArray,
    #[error("\"commands\" may contain at most {max} entries")]
    TooManyEntries { count: usize, max: usize },
    #[error("\"overwrite\" must be a boolean")]
    BadOverwrite,
    #[error("unsupported import version {0}, expected 1")]
    UnsupportedVersion(String),
    #[error(
        "scheduled-message import is not supported yet; remove schedules and import commands only"
    )]
    SchedulesNotSupported,
}

/// A parsed, unvalidated import: normalized definitions in document order,
/// MEE6 collision names (kept, still imported slash-only), malformed-entry
/// count, and the requested overwrite mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedImport {
    pub entries: Vec<PutCommandInput>,
    pub translation_conflicts: Vec<String>,
    pub invalid_entries: usize,
    pub overwrite: bool,
}

/// Parse one own-format (`version: 1`) entry into a definition. Returns `None`
/// for a malformed entry; the caller counts it invalid and continues, exactly
/// like legacy `importMee6` skips non-conforming rows instead of failing.
fn parse_own_entry(value: &serde_json::Value) -> Option<PutCommandInput> {
    let entry = value.as_object()?;
    let name = entry.get("name")?.as_str()?.to_owned();
    let template = entry.get("template")?.as_str()?.to_owned();
    let description = entry
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Imported via automations.import")
        .to_owned();
    let text_trigger = match entry.get("text_trigger") {
        None | Some(serde_json::Value::Null) => None,
        Some(trigger) => Some(trigger.as_str()?.to_owned()),
    };
    Some(PutCommandInput {
        name,
        description,
        template,
        text_trigger,
    })
}

fn parse_own_entries(raw: &[serde_json::Value]) -> (Vec<PutCommandInput>, usize) {
    let mut entries = Vec::with_capacity(raw.len());
    let mut invalid = 0;
    for value in raw {
        match parse_own_entry(value) {
            Some(entry) => entries.push(entry),
            None => invalid += 1,
        }
    }
    (entries, invalid)
}

fn parse_mee6_entries(raw: &[serde_json::Value]) -> (Vec<PutCommandInput>, usize, Vec<String>) {
    let mut inputs: Vec<Mee6CommandInput> = Vec::with_capacity(raw.len());
    let mut invalid = 0;
    for value in raw {
        match parse_mee6_entry(value) {
            Some(input) => inputs.push(input),
            None => invalid += 1,
        }
    }
    let translated = translate_export(&inputs);
    let entries = translated
        .commands
        .into_iter()
        .map(|command| PutCommandInput {
            name: command.name,
            description: command.description,
            template: command.template,
            text_trigger: command.text_trigger,
        })
        .collect();
    (entries, invalid, translated.conflicts)
}

fn parse_overwrite(
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<bool, ImportParseError> {
    match body.get("overwrite") {
        None => Ok(false),
        Some(serde_json::Value::Bool(overwrite)) => Ok(*overwrite),
        Some(_) => Err(ImportParseError::BadOverwrite),
    }
}

/// Parse an import body with at most `max_entries` raw entries. Accepts the
/// versioned [`ExportDocument`] shape or an MEE6 payload (bare array, or
/// object with a `commands` array). Entry-level problems are counted, never
/// fatal; envelope problems fail the whole import before any planning.
pub fn parse_import_document(
    body: &serde_json::Value,
    max_entries: usize,
) -> Result<ParsedImport, ImportParseError> {
    let raw: &[serde_json::Value] = match body {
        serde_json::Value::Array(entries) => entries,
        serde_json::Value::Object(map) => {
            if let Some(version) = map.get("version") {
                let supported = version.as_u64() == Some(u64::from(EXPORT_VERSION));
                if !supported {
                    let rendered = match version {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    return Err(ImportParseError::UnsupportedVersion(rendered));
                }
                let has_schedules = map.get("schedules").is_some_and(|schedules| {
                    !schedules.is_null()
                        && schedules.as_array().is_some_and(|list| !list.is_empty())
                });
                if has_schedules {
                    return Err(ImportParseError::SchedulesNotSupported);
                }
                map.get("commands")
                    .and_then(serde_json::Value::as_array)
                    .ok_or(ImportParseError::EntriesNotArray)?
                    .as_slice()
            } else {
                map.get("commands")
                    .and_then(serde_json::Value::as_array)
                    .ok_or(ImportParseError::NotAnImportDocument)?
                    .as_slice()
            }
        }
        _ => return Err(ImportParseError::NotAnImportDocument),
    };
    if raw.len() > max_entries {
        return Err(ImportParseError::TooManyEntries {
            count: raw.len(),
            max: max_entries,
        });
    }
    let overwrite = match body {
        serde_json::Value::Object(map) => parse_overwrite(map)?,
        serde_json::Value::Array(_) => false,
        _ => false,
    };
    let is_own_format =
        matches!(body, serde_json::Value::Object(map) if map.contains_key("version"));
    if is_own_format {
        let (entries, invalid_entries) = parse_own_entries(raw);
        Ok(ParsedImport {
            entries,
            translation_conflicts: Vec::new(),
            invalid_entries,
            overwrite,
        })
    } else {
        let (entries, invalid_entries, translation_conflicts) = parse_mee6_entries(raw);
        Ok(ParsedImport {
            entries,
            translation_conflicts,
            invalid_entries,
            overwrite,
        })
    }
}

/// Maximum import entries for this guild: definitions past the remaining
/// command budget are refused before planning.
#[must_use]
pub fn max_import_entries() -> usize {
    max_custom_commands(reserved_command_names().len())
}

/// One dry-run rejection: the definition plus its stable reason code
/// (`error_code` for validation, `would_overwrite` for a refused destructive
/// update, `trigger_in_use` for a trigger owned by another command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRejection {
    pub name: String,
    pub code: String,
}

/// Dry-run diff of `parsed` against stored rows, in document order. `to_create`
/// and `to_update` apply cleanly; `unchanged` already matches; `rejected`
/// explains the rest. The union capacity check fails the whole diff (legacy
/// `CommandCapacityError` aborts before any write), so a refused diff never
/// partially applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDiff {
    pub to_create: Vec<String>,
    pub to_update: Vec<String>,
    pub unchanged: Vec<String>,
    pub rejected: Vec<ImportRejection>,
    pub invalid_entries: usize,
}

fn same_content(existing: &StoredCommand, desired: &PutCommandInput) -> bool {
    existing.description == desired.description
        && existing.template == desired.template
        && existing.text_trigger == desired.text_trigger
}

/// Plan the import without touching storage. Trigger ownership is simulated in
/// document order (first claimant wins), mirroring the sequential legacy loop
/// where earlier rows of the same import already hold their triggers.
pub fn diff_import(
    existing: &[StoredCommand],
    parsed: &ParsedImport,
    builtins: &HashSet<String>,
    overwrite: bool,
) -> Result<ImportDiff, CommandError> {
    let mut valid: Vec<&PutCommandInput> = Vec::with_capacity(parsed.entries.len());
    let mut rejected: Vec<ImportRejection> = Vec::new();
    for entry in &parsed.entries {
        match validate_put_input(entry, builtins) {
            Ok(()) => valid.push(entry),
            Err(err) => rejected.push(ImportRejection {
                name: entry.name.clone(),
                code: error_code(&err).to_owned(),
            }),
        }
    }
    let max = max_custom_commands(builtins.len());
    let mut union: HashSet<&str> = existing.iter().map(|row| row.name.as_str()).collect();
    for entry in &valid {
        union.insert(entry.name.as_str());
    }
    if union.len() > max {
        return Err(CommandError::OverCapacity(max));
    }
    let by_name: HashMap<&str, &StoredCommand> = existing
        .iter()
        .map(|row| (row.name.as_str(), row))
        .collect();
    let mut trigger_owner: HashMap<String, String> = HashMap::new();
    for row in existing {
        if let Some(trigger) = row.text_trigger.as_deref() {
            trigger_owner.insert(trigger.to_lowercase(), row.name.clone());
        }
    }
    let mut diff = ImportDiff {
        to_create: Vec::new(),
        to_update: Vec::new(),
        unchanged: Vec::new(),
        rejected,
        invalid_entries: parsed.invalid_entries,
    };
    let reject = |diff: &mut ImportDiff, name: &str, code: &str| {
        diff.rejected.push(ImportRejection {
            name: name.to_owned(),
            code: code.to_owned(),
        });
    };
    for entry in valid {
        let trigger_taken = entry
            .text_trigger
            .as_deref()
            .and_then(|trigger| trigger_owner.get(&trigger.to_lowercase()))
            .is_some_and(|owner| owner != &entry.name);
        match by_name.get(entry.name.as_str()) {
            None => {
                if trigger_taken {
                    reject(&mut diff, &entry.name, "trigger_in_use");
                    continue;
                }
                if let Some(trigger) = entry.text_trigger.as_deref() {
                    trigger_owner.insert(trigger.to_lowercase(), entry.name.clone());
                }
                diff.to_create.push(entry.name.clone());
            }
            Some(stored) => {
                if same_content(stored, entry) {
                    diff.unchanged.push(entry.name.clone());
                    continue;
                }
                if !overwrite {
                    reject(&mut diff, &entry.name, "would_overwrite");
                    continue;
                }
                if trigger_taken {
                    reject(&mut diff, &entry.name, "trigger_in_use");
                    continue;
                }
                if let Some(trigger) = entry.text_trigger.as_deref() {
                    trigger_owner.insert(trigger.to_lowercase(), entry.name.clone());
                }
                diff.to_update.push(entry.name.clone());
            }
        }
    }
    Ok(diff)
}

/// Terminal import counts (legacy `importMee6` result shape). `conflicts`
/// starts with MEE6 collision names (still imported, slash-only) followed by
/// refused-update and trigger-conflict names in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    pub imported: usize,
    pub skipped: usize,
    pub conflicts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn builtins() -> HashSet<String> {
        reserved_command_names()
    }

    fn stored(name: &str, template: &str, trigger: Option<&str>) -> StoredCommand {
        StoredCommand {
            guild_id: "1545644954272137297".to_owned(),
            name: name.to_owned(),
            description: format!("The {name} command"),
            template: template.to_owned(),
            text_trigger: trigger.map(str::to_owned),
            enabled: true,
        }
    }

    fn parse(body: &serde_json::Value) -> ParsedImport {
        parse_import_document(body, max_import_entries()).expect("test body parses")
    }

    #[test]
    fn export_is_stable_sorted_and_versioned() {
        let doc = export_document(&[
            stored("welcome", "hi {user}", Some("!welcome")),
            stored("faq", "a", Some("!faq")),
        ]);
        assert_eq!(doc.version, EXPORT_VERSION);
        assert_eq!(
            doc.commands
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["faq", "welcome"]
        );
        // Current exports carry no schedules key at all.
        assert_eq!(
            serde_json::to_value(&doc).expect("serializes")["schedules"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn export_then_import_is_a_no_op() {
        let rows = vec![
            stored("faq", "a", Some("!faq")),
            stored("rules", "b", None),
            stored("welcome", "hi {user}", Some("!welcome")),
        ];
        let exported = serde_json::to_value(export_document(&rows)).expect("serializes");
        let parsed = parse(&exported);
        assert_eq!(parsed.invalid_entries, 0);
        assert!(parsed.translation_conflicts.is_empty());
        assert!(!parsed.overwrite);
        let diff = diff_import(&rows, &parsed, &builtins(), false).expect("diff plans");
        assert!(diff.to_create.is_empty());
        assert!(diff.to_update.is_empty());
        assert!(diff.rejected.is_empty());
        assert_eq!(diff.unchanged, ["faq", "rules", "welcome"]);
        // Fixed point: re-exporting the parsed definitions changes nothing.
        let reparsed = parse(&serde_json::to_value(export_document(&rows)).expect("serializes"));
        assert_eq!(parsed.entries, reparsed.entries);
    }

    #[test]
    fn export_import_round_trip_through_json_text() {
        let rows = vec![stored("faq", "See {channel}, {username}!", Some("!faq"))];
        let text = serde_json::to_string(&export_document(&rows)).expect("serializes");
        let body: serde_json::Value = serde_json::from_str(&text).expect("re-parses");
        let parsed = parse(&body);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].template, "See {channel}, {username}!");
    }

    #[test]
    fn mee6_payload_translates_like_the_legacy_goldens() {
        // Legacy `importMee6 preserves first text trigger` vectors.
        let parsed = parse(&json!([
            {"command": "faq", "response": "Read {server} rules"},
            {"command": "FAQ", "response": "second faq"},
            {"command": "welcome", "description": "says hi", "response": "hi {user}"},
            "",
        ]));
        assert_eq!(parsed.invalid_entries, 1);
        assert_eq!(parsed.translation_conflicts, ["faq"]);
        assert_eq!(
            parsed
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["faq", "faq-2", "welcome"]
        );
        assert_eq!(parsed.entries[0].template, "Read {server} rules");
        assert_eq!(
            parsed.entries[1].text_trigger, None,
            "later collisions import slash-only"
        );
        assert_eq!(parsed.entries[2].text_trigger.as_deref(), Some("!welcome"));
        let diff = diff_import(&[], &parsed, &builtins(), false).expect("diff plans");
        assert_eq!(diff.to_create, ["faq", "faq-2", "welcome"]);
        assert_eq!(diff.rejected, []);
    }

    #[test]
    fn dry_run_diff_splits_create_update_unchanged_and_refusals() {
        let existing = vec![
            stored("faq", "live answer", Some("!faq")),
            stored("same", "A", Some("!same")),
            stored("holder", "H", Some("!held")),
        ];
        let parsed = parse(&json!({
            "version": 1,
            "commands": [
                {"name": "faq", "description": "MEE6 FAQ", "template": "imported answer", "text_trigger": "!faq"},
                {"name": "same", "description": "The same command", "template": "A", "text_trigger": "!same"},
                {"name": "new", "description": "New", "template": "n", "text_trigger": "!held"},
                {"name": "rank", "description": "Spoof", "template": "spoofed rank", "text_trigger": null},
                {"name": "BAD NAME", "description": "x", "template": "x", "text_trigger": null},
            ],
        }));
        assert_eq!(parsed.invalid_entries, 0);
        let diff = diff_import(&existing, &parsed, &builtins(), false).expect("diff plans");
        assert!(
            diff.to_create.is_empty(),
            "`new` wants a trigger the live `holder` row owns, so it is refused, not created"
        );
        assert!(
            diff.to_update.is_empty(),
            "overwrite off refuses the live row"
        );
        assert_eq!(diff.unchanged, ["same"]);
        // Validation refusals (`rank` builtin, `BAD NAME` shape) come first;
        // then planning refusals in document order: `faq` needs overwrite,
        // and `new` wants a trigger another row owns.
        assert_eq!(
            diff.rejected
                .iter()
                .map(|r| (r.name.as_str(), r.code.as_str()))
                .collect::<Vec<_>>(),
            [
                ("rank", "reserved_name"),
                ("BAD NAME", "invalid_name"),
                ("faq", "would_overwrite"),
                ("new", "trigger_in_use"),
            ]
        );
        let forced = diff_import(&existing, &parsed, &builtins(), true).expect("diff plans");
        assert_eq!(forced.to_update, ["faq"]);
    }

    #[test]
    fn envelope_problems_fail_before_planning() {
        assert_eq!(
            parse_import_document(&json!({"nope": true}), max_import_entries()),
            Err(ImportParseError::NotAnImportDocument)
        );
        assert_eq!(
            parse_import_document(&json!({"commands": {}}), max_import_entries()),
            Err(ImportParseError::NotAnImportDocument)
        );
        assert_eq!(
            parse_import_document(&json!({"version": 1, "commands": {}}), max_import_entries()),
            Err(ImportParseError::EntriesNotArray)
        );
        assert_eq!(
            parse_import_document(
                &json!({"commands": [], "overwrite": "yes"}),
                max_import_entries()
            ),
            Err(ImportParseError::BadOverwrite)
        );
        assert_eq!(
            parse_import_document(&json!({"version": 2, "commands": []}), max_import_entries()),
            Err(ImportParseError::UnsupportedVersion("2".to_owned()))
        );
        assert_eq!(
            parse_import_document(
                &json!({"version": 1, "commands": [], "schedules": [{"at": "soon"}]}),
                max_import_entries()
            ),
            Err(ImportParseError::SchedulesNotSupported)
        );
        assert!(matches!(
            parse_import_document(&json!([{"command": "faq", "response": "a"}]), 0),
            Err(ImportParseError::TooManyEntries { .. })
        ));
    }

    #[test]
    fn union_capacity_fails_the_whole_diff() {
        let builtins = builtins();
        let max = max_custom_commands(builtins.len());
        let desired: Vec<PutCommandInput> = (0..=max)
            .map(|i| PutCommandInput {
                name: format!("fresh-{i}"),
                description: "Fresh".to_owned(),
                template: "t".to_owned(),
                text_trigger: None,
            })
            .collect();
        let parsed = ParsedImport {
            entries: desired,
            translation_conflicts: Vec::new(),
            invalid_entries: 0,
            overwrite: true,
        };
        assert_eq!(
            diff_import(&[], &parsed, &builtins, true),
            Err(CommandError::OverCapacity(max))
        );
    }
}
