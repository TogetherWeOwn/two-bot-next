#![cfg(feature = "db")]

//! Acceptance tests: apply only individually accepted v1 import occurrences.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file or its shared vectors. Do not merge these files alone while red.
//! Uses the shared disposable-database guard; no application connection settings.

#[path = "support/automation_import_occurrence_cases.rs"]
mod cases;

use sqlx::PgPool;
use two_bot_core::automation_transfer::{max_import_entries, parse_import_document, ImportOutcome};
use two_bot_core::custom_command_service::{self as service, ImportServiceError};
use two_bot_core::custom_command_store as store;
use two_bot_core::custom_commands::{max_custom_commands, reserved_command_names};
use two_bot_testsupport::TestDatabase;

const CREATOR: &str = "900000000000000102";
const ACTOR: &str = "900000000000000103";
const SEED_AT: &str = "2026-10-01T00:00:00Z";
const AT: &str = "2026-10-02T00:00:00Z";
const ID: &str = "occurrence-import";

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("guarded test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create disposable fixture")
}

fn parse(document: &serde_json::Value) -> two_bot_core::automation_transfer::ParsedImport {
    let wire = serde_json::to_vec(document).expect("serialize v1 wire document");
    let decoded = serde_json::from_slice(&wire).expect("decode v1 wire document");
    parse_import_document(&decoded, max_import_entries()).expect("parse v1 wire document")
}

fn applied(result: Result<ImportOutcome, ImportServiceError>) -> ImportOutcome {
    match result {
        Ok(outcome) => outcome,
        Err(ImportServiceError::Storage(sqlx::Error::Database(error))) => {
            panic!(
                "accepted occurrences must commit; SQLSTATE {}",
                error.code().as_deref().unwrap_or("unknown")
            );
        }
        Err(_) => panic!("accepted occurrences must commit without an import error"),
    }
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct Audit {
    id: String,
    guild_id: String,
    actor_id: Option<String>,
    action: String,
    target_key: Option<String>,
    outcome: String,
    reason: Option<String>,
    at_matches: bool,
}

async fn audits(pool: &PgPool) -> Vec<Audit> {
    sqlx::query_as(
        "SELECT id, guild_id, actor_id, action, target_key, outcome, reason,
        created_at = $1::timestamptz AS at_matches FROM automation_audit_log ORDER BY id",
    )
    .bind(AT)
    .fetch_all(pool)
    .await
    .expect("read exact audit facts")
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct Provenance {
    name: String,
    created_by: String,
    updated_by: String,
    created_at_is_seed: bool,
    created_at_is_import: bool,
    updated_at_is_seed: bool,
    updated_at_is_import: bool,
}

async fn provenance(pool: &PgPool) -> Vec<Provenance> {
    sqlx::query_as(
        "SELECT name, created_by, updated_by,
        created_at = $1::timestamptz AS created_at_is_seed,
        created_at = $2::timestamptz AS created_at_is_import,
        updated_at = $1::timestamptz AS updated_at_is_seed,
        updated_at = $2::timestamptz AS updated_at_is_import
        FROM automation_commands WHERE guild_id = $3 ORDER BY name",
    )
    .bind(SEED_AT)
    .bind(AT)
    .bind(cases::GUILD)
    .fetch_all(pool)
    .await
    .expect("read provenance")
}

async fn seed(pool: &PgPool, rows: &[two_bot_core::custom_commands::StoredCommand]) {
    for row in rows {
        store::put_command(pool, row, CREATOR, CREATOR, SEED_AT, SEED_AT)
            .await
            .expect("seed fixture row without import audits");
    }
}

async fn assert_case(name: &str) {
    let case = cases::cases()
        .into_iter()
        .find(|case| case.name == name)
        .expect("named acceptance vector exists");
    let db = database().await;
    seed(db.pool(), &case.existing).await;
    let result = service::import(
        db.pool(),
        cases::GUILD,
        ACTOR,
        &parse(&case.document),
        true,
        ID,
        AT,
    )
    .await;
    // Capture and close before assertions, including expected transaction failures.
    let rows = store::list_commands(db.pool(), cases::GUILD)
        .await
        .expect("read final rows");
    let facts = audits(db.pool()).await;
    let origins = provenance(db.pool()).await;
    db.close()
        .await
        .expect("verify disposable database teardown");

    assert_eq!(
        rows, case.final_rows,
        "{}: no hidden overwrite or missing accepted write",
        case.name
    );
    assert_eq!(
        applied(result),
        ImportOutcome {
            imported: case.imported,
            skipped: case.skipped,
            conflicts: case
                .conflicts
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        },
        "{}: outcome must match individually authorized occurrences",
        case.name
    );

    let mut expected_audits: Vec<_> = case
        .audits
        .iter()
        .map(|(name, occurrence, action)| Audit {
            id: format!("{ID}#cmd:{name}#{occurrence}"),
            guild_id: cases::GUILD.into(),
            actor_id: Some(ACTOR.into()),
            action: (*action).into(),
            target_key: Some((*name).into()),
            outcome: "ok".into(),
            reason: None,
            at_matches: true,
        })
        .collect();
    expected_audits.push(Audit {
        id: format!("{ID}#summary"),
        guild_id: cases::GUILD.into(),
        actor_id: Some(ACTOR.into()),
        action: "automations.import".into(),
        target_key: None,
        outcome: format!(
            "imported:{},skipped:{},conflicts:{}",
            case.imported,
            case.skipped,
            case.conflicts.len()
        ),
        reason: Some(serde_json::to_string(case.conflicts).expect("literal conflict list")),
        at_matches: true,
    });
    expected_audits.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(
        facts, expected_audits,
        "{}: audit IDs/content must match the accepted occurrences only",
        case.name
    );

    let expected_origins: Vec<_> = case
        .final_rows
        .iter()
        .map(|row| {
            let seeded = case.existing.iter().any(|old| old.name == row.name);
            let written = case.audits.iter().any(|(name, _, _)| *name == row.name);
            Provenance {
                name: row.name.clone(),
                created_by: if seeded { CREATOR } else { ACTOR }.into(),
                updated_by: if written { ACTOR } else { CREATOR }.into(),
                created_at_is_seed: seeded,
                created_at_is_import: !seeded,
                updated_at_is_seed: !written,
                updated_at_is_import: written,
            }
        })
        .collect();
    assert_eq!(
        origins, expected_origins,
        "{}: preserve creator and untouched rows",
        case.name
    );
}

macro_rules! service_case {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            assert_case(stringify!($name)).await;
        }
    };
}

service_case!(accepted_then_invalid_same_name);
service_case!(unchanged_does_not_authorize_refused_overwrite);
service_case!(accepted_then_trigger_conflicting_duplicate);
service_case!(duplicates_observe_prior_accepted_content);
service_case!(fresh_duplicate_requires_overwrite_for_changed_content);
service_case!(accepted_update_releases_trigger_for_next_first_claimant);
service_case!(rejected_update_does_not_release_existing_trigger);
service_case!(refused_overwrite_does_not_acquire_trigger);
service_case!(invalid_first_does_not_claim_trigger_or_shift_occurrence_identity);
service_case!(valid_control_same_content_converges_without_overwrite);

#[tokio::test]
async fn capacity_refusal_rolls_back_earlier_update_and_every_occurrence_audit() {
    let db = database().await;
    let initial = vec![
        cases::row("keeper", "K", None, true),
        cases::row("seed", "S", Some("!seed"), false),
    ];
    seed(db.pool(), &initial).await;
    let mut commands = vec![cases::command("seed", "Changed", Some("!changed"))];
    commands.extend(
        (0..max_custom_commands(reserved_command_names().len()) - 1)
            .map(|i| cases::command(&format!("fresh-{i}"), "T", None)),
    );
    let result = service::import(
        db.pool(),
        cases::GUILD,
        ACTOR,
        &parse(&cases::document(commands, true)),
        true,
        ID,
        AT,
    )
    .await;
    let rows = store::list_commands(db.pool(), cases::GUILD)
        .await
        .expect("read rows after capacity refusal");
    let facts = audits(db.pool()).await;
    let origins = provenance(db.pool()).await;
    db.close().await.expect("verify teardown");
    assert!(
        matches!(result, Err(ImportServiceError::OverCapacity(_))),
        "whole import must refuse capacity"
    );
    assert_eq!(
        rows, initial,
        "earlier update cannot leak out of refused transaction"
    );
    assert!(origins.iter().all(|row| row.created_by == CREATOR
        && row.updated_by == CREATOR
        && row.created_at_is_seed
        && row.updated_at_is_seed));
    assert_eq!(
        facts,
        [Audit {
            id: ID.into(),
            guild_id: cases::GUILD.into(),
            actor_id: Some(ACTOR.into()),
            action: "automations.import".into(),
            target_key: None,
            outcome: "rejected".into(),
            reason: Some("over_capacity".into()),
            at_matches: true
        }]
    );
}

#[tokio::test]
async fn capacity_boundary_convergence_keeps_one_row_and_one_audit_per_accepted_occurrence() {
    let db = database().await;
    seed(db.pool(), &[cases::row("seed", "S", None, false)]).await;
    let max = max_custom_commands(reserved_command_names().len());
    let mut commands: Vec<_> = (0..max - 1)
        .map(|i| cases::command(&format!("fresh-{i}"), "T", None))
        .collect();
    commands.push(cases::command("fresh-0", "T", None));
    let result = service::import(
        db.pool(),
        cases::GUILD,
        ACTOR,
        &parse(&cases::document(commands, false)),
        false,
        ID,
        AT,
    )
    .await;
    let rows = store::list_commands(db.pool(), cases::GUILD)
        .await
        .expect("read capacity boundary rows");
    let facts = audits(db.pool()).await;
    db.close().await.expect("verify teardown");
    assert_eq!(
        applied(result),
        ImportOutcome {
            imported: max,
            skipped: 0,
            conflicts: vec![]
        }
    );
    let mut expected = vec![cases::row("seed", "S", None, false)];
    expected.extend((0..max - 1).map(|i| cases::row(&format!("fresh-{i}"), "T", None, true)));
    expected.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(rows, expected);
    assert_eq!(
        facts.len(),
        max + 1,
        "every accepted occurrence and summary audited once"
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.action == "command.create")
            .count(),
        max - 1
    );
    assert!(facts
        .iter()
        .any(|fact| fact.id == format!("{ID}#cmd:fresh-0#1")
            && fact.action == "command.update"
            && fact.outcome == "ok"));
}

#[tokio::test]
async fn overwrite_document_still_requires_separately_enabled_capability() {
    let db = database().await;
    let initial = vec![cases::row("faq", "A", Some("!faq"), false)];
    seed(db.pool(), &initial).await;
    let parsed = parse(&cases::document(
        vec![cases::command("faq", "B", Some("!other"))],
        true,
    ));
    let result = service::import(db.pool(), cases::GUILD, ACTOR, &parsed, false, ID, AT).await;
    let rows = store::list_commands(db.pool(), cases::GUILD)
        .await
        .expect("read capability refusal");
    let facts = audits(db.pool()).await;
    db.close().await.expect("verify teardown");
    assert!(matches!(
        result,
        Err(ImportServiceError::OverwriteNotAllowed)
    ));
    assert_eq!(rows, initial);
    assert!(
        facts.is_empty(),
        "capability refusal may not create an occurrence or summary audit"
    );
}

#[tokio::test]
async fn summary_readback_preserves_first_decisions_after_admin_edits() {
    let db = database().await;
    let parsed = parse(&cases::document(
        vec![
            serde_json::Value::Null,
            cases::command("faq", "A", Some("!faq")),
            cases::command("other", "B", Some("!faq")),
        ],
        false,
    ));
    assert_eq!(
        parsed.invalid_entries, 1,
        "malformed wire entry contributes to skipped count"
    );
    let first = service::import(db.pool(), cases::GUILD, ACTOR, &parsed, false, ID, AT).await;
    let first_facts = audits(db.pool()).await;
    let edited = vec![
        cases::row("faq", "Admin edit", None, false),
        cases::row("other", "Admin owned trigger", Some("!faq"), false),
    ];
    for row in &edited {
        store::put_command(db.pool(), row, CREATOR, CREATOR, SEED_AT, SEED_AT)
            .await
            .expect("simulate later admin edit in disposable fixture");
    }
    // The receiver replays from the committed summary rather than invoking
    // service::import a second time. Exercise that durable read boundary.
    let replay = store::load_audit(db.pool(), &format!("{ID}#summary"))
        .await
        .expect("load committed replay summary");
    let rows = store::list_commands(db.pool(), cases::GUILD)
        .await
        .expect("read after replay");
    let replay_facts = audits(db.pool()).await;
    db.close().await.expect("verify teardown");
    let expected = ImportOutcome {
        imported: 1,
        skipped: 2,
        conflicts: vec!["other".into()],
    };
    assert_eq!(applied(first), expected);
    assert_eq!(
        replay,
        Some((
            "imported:1,skipped:2,conflicts:1".into(),
            Some("[\"other\"]".into())
        ))
    );
    assert_eq!(
        rows, edited,
        "replay must not replan against or overwrite admin edits"
    );
    assert_eq!(
        replay_facts, first_facts,
        "summary replay must not mint new occurrence audits"
    );
    assert_eq!(
        replay_facts.len(),
        2,
        "one accepted occurrence plus immutable summary"
    );
}
