//! Real dump/restore regression: moderation uses the exact member migrations;
//! empty unrelated legacy tables only make the full v3 backup API executable.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};
use two_bot_core::backup::{dump, dump_file};
use two_bot_core::member_moderation::{ClaimState, MemberModerationStore};
use two_bot_core::member_moderation_store::PgMemberModerationStore;

use super::{accepted_unban, ban_state, cleanup, database, generation, unban_state, DUE, NOW};

async fn backup_database() -> (PgPool, PgPool, String) {
    let (admin, pool, schema) = database().await;
    for table in dump_file::DUMP_TABLES {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(*table)
            .fetch_one(&pool)
            .await
            .unwrap();
        if exists {
            continue;
        }
        // Fixed production allowlist only. These empty placeholders supply the
        // unrelated tables' ordering columns, never moderation data/ownership.
        QueryBuilder::<Postgres>::new("CREATE TABLE ")
            .push(*table)
            .push(" (id BIGSERIAL PRIMARY KEY, guild_id TEXT, member_id TEXT, code TEXT, \
                   entry_id TEXT, created_at TIMESTAMPTZ, execute_at TIMESTAMPTZ, request_id TEXT, \
                   channel_id TEXT, idempotency_key TEXT, occurred_at TIMESTAMPTZ, audit_entry_id TEXT, \
                   started_at TIMESTAMPTZ, event_id TEXT, joined_at TIMESTAMPTZ, name TEXT, \
                   ticket_id TEXT, user_id TEXT, message_id TEXT, panel_id TEXT, generation BIGSERIAL)")
            .build()
            .execute(&pool)
            .await
            .unwrap();
    }
    (admin, pool, schema)
}

fn archive_path() -> PathBuf {
    let scratch = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .or_else(|| {
            (std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true"))
                .then(|| std::env::var_os("RUNNER_TEMP"))
                .flatten()
        })
        .expect("run-owned scratch or CI runner temp required");
    PathBuf::from(scratch).join(format!(
        "member-backup-{:032x}.ndjson.gz",
        rand::random::<u128>()
    ))
}

async fn seed(pool: &PgPool) {
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    // Nontrivial restored high-water mark proves insertion order is retained.
    sqlx::query("SELECT setval(pg_get_serial_sequence('moderation_member_bans', 'generation'), 9000, false)")
        .execute(pool).await.unwrap();
    accepted_unban(&store, "guild", "temporary", "temp", DUE, NOW).await;
    store
        .activate_staged_unban("guild", "temporary", "temp", NOW)
        .await
        .unwrap();
    let attempt = store
        .stage_ban("guild", "permanent", "perm", NOW)
        .await
        .unwrap();
    store
        .confirm_ban_attempt("guild", "permanent", "perm", attempt, NOW)
        .await
        .unwrap();
    store
        .stage_ban("guild", "uncertain", "prepared", NOW)
        .await
        .unwrap();
    let attempt = store
        .stage_ban("guild", "rejected", "reject", NOW)
        .await
        .unwrap();
    store
        .reject_ban_attempt("guild", "rejected", "reject", attempt, NOW)
        .await
        .unwrap();
    assert_eq!(
        store
            .claim("guild", "temp-key", "moderation.tempban", "hash", NOW)
            .await
            .unwrap(),
        ClaimState::Claimed
    );
    store
        .complete("guild", "temp-key", "banned", "{}", NOW)
        .await
        .unwrap();
}

async fn moderation_snapshot(pool: &PgPool) -> Vec<String> {
    let mut rows = Vec::new();
    for table in [
        "moderation_member_bans",
        "moderation_scheduled_unbans",
        "moderation_idempotency",
        "moderation_audit",
        "moderation_warnings",
    ] {
        let values: Vec<String> =
            QueryBuilder::<Postgres>::new("SELECT row_to_json(t)::text FROM ")
                .push(table)
                .push(" AS t ORDER BY 1")
                .build_query_scalar()
                .fetch_all(pool)
                .await
                .unwrap();
        rows.extend(values);
    }
    rows
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn ownership_round_trip_quarantines_expiries_and_preserves_destination_history() {
    let (admin, source, schema) = backup_database().await;
    seed(&source).await;
    let snapshot = moderation_snapshot(&source).await;
    let path = archive_path();
    let manifest = dump::dump(&source, &path).await.unwrap();
    assert_eq!(
        manifest
            .tables
            .iter()
            .find(|t| t.name == "moderation_member_bans")
            .expect("ownership must be in the schedule/idempotency snapshot")
            .count,
        4
    );

    for stale_destination in [false, true] {
        let (target_admin, target, target_schema) = backup_database().await;
        let store = PgMemberModerationStore::new(target.clone(), "guild");
        if stale_destination {
            // An older snapshot must not discard a newer permanent ban or
            // revive its superseded expiry. Refusal preserves the destination.
            sqlx::query("SELECT setval(pg_get_serial_sequence('moderation_member_bans', 'generation'), 50000, false)")
                .execute(&target).await.unwrap();
            let attempt = store
                .stage_ban("guild", "temporary", "post-backup", NOW)
                .await
                .unwrap();
            store
                .confirm_ban_attempt("guild", "temporary", "post-backup", attempt, NOW)
                .await
                .unwrap();
        }
        if stale_destination {
            let destination = moderation_snapshot(&target).await;
            let err = dump::restore(&target, &path).await.unwrap_err();
            assert!(err.to_string().contains("destination moderation history"));
            assert_eq!(moderation_snapshot(&target).await, destination);
            assert!(store
                .claim_due_unbans("guild", DUE, 25)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(ban_state(&target, "post-backup").await, "accepted");
            cleanup(target_admin, target, target_schema).await;
            continue;
        }
        let report = dump::restore(&target, &path).await.unwrap();
        assert!(report.ok);
        assert_eq!(report.restored["moderation_member_bans"], 4);
        assert_eq!(report.quarantined_unbans, 1);
        let quarantined_snapshot: Vec<_> = snapshot
            .iter()
            .map(|row| row.replace("\"state\":\"pending\"", "\"state\":\"quarantined\""))
            .collect();
        assert_eq!(moderation_snapshot(&target).await, quarantined_snapshot);
        // The file retains the original accepted/pending evidence; applying it
        // changes only executability, never acceptance or insertion order.
        let contents = dump_file::inspect(&path).unwrap();
        assert_eq!(
            contents.buffers["moderation_scheduled_unbans"][0]["state"],
            "pending"
        );
        assert_eq!(ban_state(&target, "temp").await, "accepted");
        assert_eq!(unban_state(&target, "temp").await, "quarantined");
        assert_eq!(ban_state(&target, "prepared").await, "prepared");
        assert_eq!(ban_state(&target, "reject").await, "rejected");
        assert_eq!(
            store
                .claim("guild", "temp-key", "moderation.tempban", "hash", NOW)
                .await
                .unwrap(),
            ClaimState::Replayed {
                outcome: "banned".into()
            }
        );
        let due = store.claim_due_unbans("guild", DUE, 25).await.unwrap();
        assert!(
            due.is_empty(),
            "snapshot acceptance is not current remote ownership"
        );
        assert!(store
            .activate_staged_unban("guild", "temporary", "temp", NOW)
            .await
            .is_err());
        assert!(store
            .claim_due_unbans("guild", DUE, 25)
            .await
            .unwrap()
            .is_empty());
        store
            .stage_ban("guild", "fresh", "next", NOW)
            .await
            .unwrap();
        assert_eq!(
            generation(&target, "next").await,
            9004,
            "sequence resumes after restored MAX, not after stale destination"
        );
        cleanup(target_admin, target, target_schema).await;
    }
    cleanup(admin, source, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn old_snapshot_cannot_revive_expiry_over_post_backup_permanent_ban() {
    let (admin, pool, schema) = backup_database().await;
    seed(&pool).await;
    let path = archive_path();
    dump::dump(&pool, &path).await.unwrap();
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    let attempt = store
        .stage_ban("guild", "temporary", "new-permanent", DUE)
        .await
        .unwrap();
    store
        .confirm_ban_attempt("guild", "temporary", "new-permanent", attempt, DUE)
        .await
        .unwrap();
    assert_eq!(unban_state(&pool, "temp").await, "superseded");
    let before = moderation_snapshot(&pool).await;
    let err = dump::restore(&pool, &path).await.unwrap_err();
    assert!(err.to_string().contains("destination moderation history"));
    assert_eq!(moderation_snapshot(&pool).await, before);
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(ban_state(&pool, "new-permanent").await, "accepted");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn restore_refuses_each_destination_evidence_kind_without_writes() {
    let (admin, source, schema) = backup_database().await;
    seed(&source).await;
    let path = archive_path();
    dump::dump(&source, &path).await.unwrap();
    for kind in [
        "prepared",
        "running",
        "orphan",
        "audit",
        "idempotency",
        "warning",
    ] {
        let (target_admin, target, target_schema) = backup_database().await;
        let store = PgMemberModerationStore::new(target.clone(), "guild");
        match kind {
            "prepared" => {
                store
                    .stage_ban("guild", "member", "uncertain-put", NOW)
                    .await
                    .unwrap();
            }
            "running" => {
                accepted_unban(&store, "guild", "member", "uncertain-delete", DUE, NOW).await;
                assert_eq!(
                    store
                        .claim_due_unbans("guild", DUE, 25)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
            }
            "orphan" => {
                sqlx::query("INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at, reason, state, created_at, dispatch_uncertain) VALUES ('orphan', 'guild', 'member', $1::text::timestamptz, 'expiry', 'quarantined', $1::text::timestamptz, TRUE)")
                    .bind(NOW).execute(&target).await.unwrap();
            }
            "audit" => {
                sqlx::query("INSERT INTO moderation_audit (request_id, guild_id, actor_id, action, reason, outcome, idempotency_key, metadata_json, created_at) VALUES ('evidence', 'guild', 'actor', 'moderation.ban', 'reason', 'banned', 'key', '{}', $1::text::timestamptz)")
                    .bind(NOW).execute(&target).await.unwrap();
            }
            "idempotency" => {
                store
                    .claim("guild", "key", "moderation.ban", "hash", NOW)
                    .await
                    .unwrap();
            }
            "warning" => {
                sqlx::query("INSERT INTO moderation_warnings (id, guild_id, user_id, actor_id, reason, request_id, created_at) VALUES ('warning', 'guild', 'member', 'actor', 'reason', 'warn', $1::text::timestamptz)")
                    .bind(NOW).execute(&target).await.unwrap();
            }
            _ => unreachable!(),
        }
        let before = moderation_snapshot(&target).await;
        let err = dump::restore(&target, &path).await.unwrap_err();
        assert!(
            err.to_string().contains("destination moderation history"),
            "{kind}: {err}"
        );
        assert_eq!(moderation_snapshot(&target).await, before, "{kind}");
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(
            events, 0,
            "refusal precedes replacement of unrelated tables"
        );
        cleanup(target_admin, target, target_schema).await;
    }
    cleanup(admin, source, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn retry_queue_ticket_round_trip_sets_the_shared_sequence_high_water_mark() {
    let (admin, source, schema) = backup_database().await;
    seed(&source).await;
    let store = PgMemberModerationStore::new(source.clone(), "guild");
    let job = store
        .claim_due_unbans("guild", DUE, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    store
        .requeue_unban(&job.request_id, &job.claim_token)
        .await
        .unwrap();
    let retry: i64 = sqlx::query_scalar(
        "SELECT retry_generation FROM moderation_scheduled_unbans WHERE request_id = 'temp'",
    )
    .fetch_one(&source)
    .await
    .unwrap();
    assert!(retry > generation(&source, "reject").await);
    let path = archive_path();
    dump::dump(&source, &path).await.unwrap();
    let (target_admin, target, target_schema) = backup_database().await;
    dump::restore(&target, &path).await.unwrap();
    let restored: i64 = sqlx::query_scalar(
        "SELECT retry_generation FROM moderation_scheduled_unbans WHERE request_id = 'temp'",
    )
    .fetch_one(&target)
    .await
    .unwrap();
    assert_eq!(restored, retry);
    assert_eq!(unban_state(&target, "temp").await, "quarantined");
    let store = PgMemberModerationStore::new(target.clone(), "guild");
    let attempt = store
        .stage_ban("guild", "fresh", "after-retry", NOW)
        .await
        .unwrap();
    assert_eq!(attempt.generation, retry + 1);
    cleanup(target_admin, target, target_schema).await;
    cleanup(admin, source, schema).await;
}

fn legacy_without_ownership(source: &Path, target: &Path) {
    let file = std::fs::File::open(source).unwrap();
    let mut lines: Vec<Value> = BufReader::new(GzDecoder::new(file))
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    lines[0]["tables"]
        .as_array_mut()
        .unwrap()
        .retain(|t| t["name"] != "moderation_member_bans");
    lines.retain(|line| line["table"] != "moderation_member_bans");
    // Old v3 has no ownership, persistent dispatch fence or retry ticket.
    for column in ["dispatch_uncertain", "retry_generation"] {
        let schedule = lines[0]["tables"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|t| t["name"] == "moderation_scheduled_unbans")
            .unwrap();
        let index = schedule["columns"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c == column)
            .unwrap();
        schedule["columns"].as_array_mut().unwrap().remove(index);
        schedule["column_types"]
            .as_array_mut()
            .unwrap()
            .remove(index);
        for line in &mut lines {
            if line["table"] == "moderation_scheduled_unbans" {
                line["data"].as_object_mut().unwrap().remove(column);
            }
        }
    }
    let rows = lines.iter().filter(|line| line["kind"] == "row").count();
    lines.last_mut().unwrap()["rows"] = serde_json::json!(rows);
    let mut encoder = dump_file::new_encoder();
    for line in lines {
        dump_file::write_line(&mut encoder, &line).unwrap();
    }
    std::fs::write(target, dump_file::finish_gzip(encoder).unwrap()).unwrap();
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn old_v3_restore_quarantines_without_inventing_acceptance() {
    let (admin, source, schema) = backup_database().await;
    seed(&source).await;
    let store = PgMemberModerationStore::new(source.clone(), "guild");
    accepted_unban(&store, "guild", "dispatch", "running", DUE, NOW).await;
    let jobs = store.claim_due_unbans("guild", DUE, 25).await.unwrap();
    assert_eq!(jobs.len(), 2);
    let original_token: String = sqlx::query_scalar(
        "SELECT claim_token FROM moderation_scheduled_unbans WHERE request_id = 'running'",
    )
    .fetch_one(&source)
    .await
    .unwrap();
    // Cover staged/pending and running imports separately.
    sqlx::query("UPDATE moderation_scheduled_unbans SET state = 'pending', claim_token = NULL, claimed_at = NULL, dispatch_uncertain = FALSE WHERE request_id = 'temp'")
        .execute(&source).await.unwrap();
    let modern = archive_path();
    dump::dump(&source, &modern).await.unwrap();
    let legacy = archive_path();
    legacy_without_ownership(&modern, &legacy);
    dump_file::inspect(&legacy).expect("the old 22-table v3 envelope remains readable");
    // In-place restore is refused: absence of ownership in the file must not
    // delete target PUT/DELETE evidence. A fresh target may import it safely.
    let destination = moderation_snapshot(&source).await;
    assert!(dump::restore(&source, &legacy).await.is_err());
    assert_eq!(moderation_snapshot(&source).await, destination);
    let (target_admin, target, target_schema) = backup_database().await;
    let store = PgMemberModerationStore::new(target.clone(), "guild");
    let report = dump::restore(&target, &legacy).await.unwrap();
    assert!(report.ok);
    assert!(report.missing_member_ban_ownership);
    assert_eq!(report.quarantined_unbans, 2);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_member_bans")
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(count, 0, "never synthesize acceptance absent from backup");
    assert_eq!(unban_state(&target, "temp").await, "quarantined");
    assert_eq!(unban_state(&target, "running").await, "quarantined");
    let fence: (bool, String) = sqlx::query_as("SELECT dispatch_uncertain, claim_token FROM moderation_scheduled_unbans WHERE request_id = 'running'")
        .fetch_one(&target).await.unwrap();
    assert_eq!(fence, (true, original_token));
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .unwrap()
        .is_empty());
    assert!(
        store
            .stage_ban("guild", "dispatch", "unsafe", NOW)
            .await
            .is_err(),
        "quarantine cannot discard an imported running DELETE fence"
    );
    store
        .stage_ban("guild", "fresh", "next", NOW)
        .await
        .unwrap();
    assert_eq!(
        generation(&target, "next").await,
        1,
        "empty restored ownership sequence restarts"
    );
    cleanup(target_admin, target, target_schema).await;
    cleanup(admin, source, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn ownership_schedules_and_idempotency_share_one_dump_snapshot() {
    let (admin, source, schema) = backup_database().await;
    seed(&source).await;
    let path = archive_path();
    let mut mutation = source.begin().await.unwrap();
    sqlx::query("LOCK TABLE moderation_member_bans IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *mutation)
        .await
        .unwrap();
    let dumping = tokio::spawn({
        let source = source.clone();
        let path = path.clone();
        async move { dump::dump(&source, &path).await }
    });
    // Wait for the actual read lock, not a timing guess. Earlier table reads
    // have established the snapshot before this atomic source update commits.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE relation = \
                 'moderation_member_bans'::regclass AND NOT granted)",
            )
            .fetch_one(&source)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dump reached the ownership read lock");
    sqlx::query("UPDATE moderation_member_bans SET state = 'prepared', completed_at = NULL WHERE request_id = 'temp'")
        .execute(&mut *mutation).await.unwrap();
    sqlx::query(
        "UPDATE moderation_scheduled_unbans SET state = 'staged' WHERE request_id = 'temp'",
    )
    .execute(&mut *mutation)
    .await
    .unwrap();
    sqlx::query("UPDATE moderation_idempotency SET state = 'in_progress', outcome = NULL WHERE idempotency_key = 'temp-key'")
        .execute(&mut *mutation).await.unwrap();
    mutation.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), dumping)
        .await
        .expect("bounded dump")
        .unwrap()
        .unwrap();
    let contents = dump_file::inspect(&path).unwrap();
    let intent = contents.buffers["moderation_member_bans"]
        .iter()
        .find(|row| row["request_id"] == "temp")
        .unwrap();
    assert_eq!(intent["state"], "accepted");
    assert_eq!(
        contents.buffers["moderation_scheduled_unbans"][0]["state"],
        "pending"
    );
    assert_eq!(
        contents.buffers["moderation_idempotency"][0]["state"],
        "done"
    );
    assert_eq!(
        ban_state(&source, "temp").await,
        "prepared",
        "source mutation actually committed"
    );
    cleanup(admin, source, schema).await;
}
