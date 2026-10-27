//! Protected RED-first acceptance: member-report schema, retention and erasure.
//! Builders implement the contract; do not edit these tests to make it pass.
//! All fixtures are synthetic and use only an owned disposable test database.

use std::future::Future;

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_cutover::{
    member_erasure::{erase_member, plan, schema_gaps, ErasureMode},
    retention::{run_retention, RetentionMode},
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "710000000000000001";
const OTHER_GUILD: &str = "710000000000000002";
const SUBJECT: &str = "720000000000000001";
const REPORTER: &str = "720000000000000002";
const OTHER: &str = "720000000000000003";
const TABLES: [&str; 3] = ["member_reports", "report_snapshots", "report_reporters"];

// Joining the assertion task before closing guarantees explicit fixture teardown
// even on the expected RED assertion, rather than relying on panic-time Drop.
async fn with_database<F, Fut>(test: F)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("configure the disposable agent-testdb bootstrap; acceptance never skips");
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .expect("create migrated disposable database");
    let result = tokio::spawn(test(db.pool().clone())).await;
    db.close().await.expect("verify owned database teardown");
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}

async fn require_report_schema(pool: &PgPool) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name IN \
         ('member_reports', 'report_snapshots', 'report_reporters') ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
    .expect("inspect migrated report schema");
    assert_eq!(
        tables,
        ["member_reports", "report_reporters", "report_snapshots"],
        "missing slice-1 migration: all three report tables must exist"
    );
}

async fn snapshot_tables(pool: &PgPool) -> Vec<Vec<String>> {
    let mut result = Vec::new();
    for table in TABLES {
        // Audited: `table` comes only from the fixed `TABLES` const, never from
        // test input, so interpolating it cannot inject SQL.
        result.push(
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT row_to_json(t)::text FROM {table} t ORDER BY row_to_json(t)::text"
            )))
            .fetch_all(pool)
            .await
            .expect("snapshot synthetic report rows"),
        );
    }
    result
}

async fn report(pool: &PgPool, id: &str, guild: &str, reporter: &str, status: &str) {
    sqlx::query(
        "INSERT INTO member_reports \
         (id, guild_id, channel_id, message_id, reported_member_id, reporter_id, \
          report_count, reason, snapshot_status, status, created_at, closed_at, disposition) \
         VALUES ($1, $2, '730000000000000001', $1, $3, $4, 3, \
          'Synthetic report without personal identifiers', 'present', $5, \
          now() - interval '200 days', \
          CASE WHEN $5 = 'closed' THEN now() - interval '1 day' ELSE NULL END, \
          CASE WHEN $5 = 'closed' THEN 'informational' ELSE NULL END)",
    )
    .bind(id)
    .bind(guild)
    .bind(SUBJECT)
    .bind(reporter)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed report group");
    sqlx::query(
        "INSERT INTO report_snapshots \
         (report_id, author_id, author_bot, message_created_at, content, attachments, attachment_overflow) \
         VALUES ($1, $2, false, now() - interval '201 days', $3, $4, false)",
    )
    .bind(id)
    .bind(SUBJECT)
    .bind(format!("Synthetic preserved evidence for {id}"))
    .bind(json!([{
        "filename": "synthetic.txt", "size_bytes": 7, "content_type": "text/plain",
        "url": "https://example.invalid/synthetic.txt",
        "proxy_url": "https://example.invalid/proxy/synthetic.txt"
    }]))
    .execute(pool)
    .await
    .expect("seed single-message snapshot");
    for member in [reporter, OTHER] {
        sqlx::query(
            "INSERT INTO report_reporters (report_id, reporter_id, reported_at) VALUES ($1, $2, now())",
        )
        .bind(id)
        .bind(member)
        .execute(pool)
        .await
        .expect("seed bounded coalesced reporters");
    }
}

async fn group(pool: &PgPool, id: &str) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(t) FROM member_reports t WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read report group")
}

#[test]
fn manifest_explicitly_covers_reporter_identity_and_documents_retained_tombstone_scope() {
    let manifest = plan();
    for (table, column) in [
        ("member_reports", "reporter_id"),
        ("report_reporters", "reporter_id"),
    ] {
        assert!(
            manifest
                .tables
                .iter()
                .any(|entry| entry.table == table
                    && entry.columns.iter().any(|field| field == column)),
            "erasable identity must be covered, never merely allowlisted: {table}.{column}"
        );
    }
    for (table, column) in [
        ("member_reports", "reported_member_id"),
        ("member_reports", "message_id"),
        ("report_snapshots", "author_id"),
    ] {
        assert!(
            manifest.allowlist.iter().any(|entry| entry.table == table
                && entry.column == column
                && !entry.reason.trim().is_empty()),
            "retained tombstone scope needs a column-specific reason: {table}.{column}"
        );
    }
    let claimer_covered = manifest.tables.iter().any(|entry| {
        entry.table == "member_reports" && entry.columns.iter().any(|field| field == "claimer_id")
    });
    let claimer_retained = manifest.allowlist.iter().any(|entry| {
        entry.table == "member_reports"
            && entry.column == "claimer_id"
            && !entry.reason.trim().is_empty()
    });
    assert!(
        claimer_covered || claimer_retained,
        "staff attribution cannot be invisible to the manifest merely because its name is new"
    );
}

#[tokio::test]
async fn open_and_claimed_message_groups_are_unique_but_closed_and_other_guild_are_not() {
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        report(&pool, "740000000000000001", GUILD, REPORTER, "open").await;
        let insert = "INSERT INTO member_reports \
            (id, guild_id, message_id, reported_member_id, reporter_id, report_count, reason, \
             snapshot_status, status, created_at) \
            VALUES ($1, $2, '740000000000000001', $3, $4, 1, 'Synthetic duplicate', \
             'present', 'open', now())";
        for status in ["open", "claimed"] {
            sqlx::query("UPDATE member_reports SET status = $1 WHERE id = '740000000000000001'")
                .bind(status)
                .execute(&pool)
                .await
                .expect("set active state");
            let error = sqlx::query(insert)
                .bind("740000000000000002")
                .bind(GUILD)
                .bind(SUBJECT)
                .bind(REPORTER)
                .execute(&pool)
                .await
                .expect_err("one active group per guild and message");
            assert_eq!(
                error.as_database_error().and_then(|e| e.code()).as_deref(),
                Some("23505")
            );
        }
        sqlx::query(insert)
            .bind("740000000000000003")
            .bind(OTHER_GUILD)
            .bind(SUBJECT)
            .bind(REPORTER)
            .execute(&pool)
            .await
            .expect("another guild has an independent group");
        sqlx::query(
            "UPDATE member_reports SET status = 'closed', closed_at = now(), \
                     disposition = 'informational' WHERE id = '740000000000000001'",
        )
        .execute(&pool)
        .await
        .expect("close old group");
        sqlx::query(insert)
            .bind("740000000000000004")
            .bind(GUILD)
            .bind(SUBJECT)
            .bind(REPORTER)
            .execute(&pool)
            .await
            .expect("closed groups must not absorb a new report");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM member_reports")
                .fetch_one(&pool)
                .await
                .expect("count groups"),
            3
        );
    })
    .await;
}

#[tokio::test]
async fn a_message_report_has_one_snapshot_and_snapshot_children_follow_group_deletion() {
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        report(&pool, "740000000000000011", GUILD, REPORTER, "closed").await;
        let duplicate = sqlx::query(
            "INSERT INTO report_snapshots \
             (report_id, author_id, author_bot, message_created_at, content, attachments, attachment_overflow) \
             SELECT report_id, author_id, author_bot, message_created_at, content, attachments, attachment_overflow \
             FROM report_snapshots WHERE report_id = '740000000000000011'",
        ).execute(&pool).await.expect_err("snapshot is one-per-report, never a history crawl");
        assert_eq!(duplicate.as_database_error().and_then(|e| e.code()).as_deref(), Some("23505"));
        let orphan = sqlx::query(
            "INSERT INTO report_snapshots \
             (report_id, author_id, author_bot, message_created_at, content, attachments, attachment_overflow) \
             VALUES ('740000000000000099', '720000000000000001', false, now(), 'Synthetic quote', '[]', false)",
        ).execute(&pool).await.expect_err("snapshot requires its report group");
        assert_eq!(orphan.as_database_error().and_then(|e| e.code()).as_deref(), Some("23503"));
        sqlx::query("DELETE FROM member_reports WHERE id = '740000000000000011'")
            .execute(&pool).await.expect("delete synthetic group");
        assert_eq!(snapshot_tables(&pool).await, vec![Vec::<String>::new(); 3]);
    }).await;
}

#[tokio::test]
async fn reporter_storage_is_bounded_to_ten_even_when_aggregate_count_grows() {
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        report(&pool, "740000000000000021", GUILD, REPORTER, "open").await;
        for n in 0..8 {
            sqlx::query(
                "INSERT INTO report_reporters (report_id, reporter_id, reported_at) \
                         VALUES ('740000000000000021', $1, now())",
            )
            .bind(format!("7500000000000000{n:02}"))
            .execute(&pool)
            .await
            .expect("first ten identities fit");
        }
        let extra = sqlx::query(
            "INSERT INTO report_reporters (report_id, reporter_id, reported_at) \
                                VALUES ('740000000000000021', '750000000000000099', now())",
        )
        .execute(&pool)
        .await;
        assert!(
            extra.is_err(),
            "the schema must not permit an eleventh persisted reporter identity"
        );
        sqlx::query("UPDATE member_reports SET report_count = 11 WHERE id = '740000000000000021'")
            .execute(&pool)
            .await
            .expect("aggregate can grow without extra identities");
        assert_eq!(group(&pool, "740000000000000021").await["report_count"], 11);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM report_reporters")
                .fetch_one(&pool)
                .await
                .expect("count retained identities"),
            10
        );
    })
    .await;
}

#[tokio::test]
async fn snapshot_storage_bounds_unicode_and_metadata_and_preserves_missing_provenance() {
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        report(&pool, "740000000000000031", GUILD, REPORTER, "open").await;
        let bounded = format!("{}…[truncated]", "é".repeat(3988));
        assert_eq!(bounded.chars().count(), 4000);
        let attachments = json!((0..10).map(|n| json!({
            "filename": format!("synthetic-{n}.txt"), "size_bytes": 7,
            "content_type": "text/plain", "url": "https://example.invalid/synthetic.txt",
            "proxy_url": "https://example.invalid/proxy/synthetic.txt"
        })).collect::<Vec<_>>());
        sqlx::query("UPDATE report_snapshots SET content = $1, attachments = $2, \
                     attachment_overflow = true WHERE report_id = '740000000000000031'")
            .bind(&bounded).bind(&attachments).execute(&pool).await
            .expect("4000 Unicode characters and ten metadata entries fit");
        let stored: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM report_snapshots t \
                                               WHERE report_id = '740000000000000031'")
            .fetch_one(&pool).await.expect("read bounded snapshot");
        assert_eq!(stored["content"], bounded);
        assert_eq!(stored["attachments"], attachments);
        assert_eq!(stored["attachment_overflow"], true);
        let too_long = sqlx::query("UPDATE report_snapshots SET content = $1 \
                                    WHERE report_id = '740000000000000031'")
            .bind("é".repeat(4001)).execute(&pool).await;
        assert!(too_long.is_err(), "storage must not retain content beyond 4000 characters");
        let eleven = json!(vec![attachments[0].clone(); 11]);
        assert!(sqlx::query("UPDATE report_snapshots SET attachments = $1 \
                             WHERE report_id = '740000000000000031'")
            .bind(eleven).execute(&pool).await.is_err(), "no eleventh attachment metadata row");
        assert!(sqlx::query("UPDATE member_reports SET reason = $1 WHERE id = '740000000000000031'")
            .bind("x".repeat(1001)).execute(&pool).await.is_err(), "report reasons stay bounded");

        for (id, status) in [
            ("740000000000000032", "missing-deleted"),
            ("740000000000000033", "missing-unreadable"),
        ] {
            // Missing states still have a single row, but no invented quote or metadata.
            sqlx::query("INSERT INTO member_reports \
                (id, guild_id, message_id, reported_member_id, reporter_id, report_count, reason, \
                 snapshot_status, status, created_at) \
                VALUES ($1, $2, $1, $3, $4, 1, 'Synthetic missing message', $5, 'open', now())")
                .bind(id).bind(GUILD).bind(SUBJECT).bind(REPORTER).bind(status)
                .execute(&pool).await.expect("missing-message provenance fits schema");
            sqlx::query("INSERT INTO report_snapshots \
                (report_id, author_id, author_bot, message_created_at, content, attachments, attachment_overflow) \
                VALUES ($1, NULL, NULL, NULL, NULL, '[]', false)")
                .bind(id).execute(&pool).await.expect("honest missing snapshot has no fetched author or text");
            let row: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM report_snapshots t WHERE report_id = $1")
                .bind(id).fetch_one(&pool).await.expect("read missing snapshot");
            assert_eq!(row["content"], Value::Null);
            assert_eq!(row["attachments"], json!([]));
            assert_eq!(group(&pool, id).await["snapshot_status"], status);
        }
    }).await;
}

#[tokio::test]
async fn ninety_day_closed_at_retention_preserves_active_and_recent_reports_and_dry_run_is_read_only(
) {
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        for (id, guild, status) in [
            ("740000000000000041", GUILD, "closed"),
            ("740000000000000042", GUILD, "closed"),
            ("740000000000000043", GUILD, "closed"),
            ("740000000000000044", GUILD, "open"),
            ("740000000000000045", GUILD, "claimed"),
            ("740000000000000046", OTHER_GUILD, "closed"),
        ] {
            report(&pool, id, guild, REPORTER, status).await;
        }
        sqlx::query("UPDATE member_reports SET closed_at = now() - interval '90 days 1 minute' \
                     WHERE id IN ('740000000000000041', '740000000000000042')")
            .execute(&pool).await.expect("seed just-expired closures");
        sqlx::query("UPDATE member_reports SET closed_at = now() - interval '89 days 23 hours 59 minutes' \
                     WHERE id = '740000000000000043'")
            .execute(&pool).await.expect("seed just-unexpired closure");
        let before = snapshot_tables(&pool).await;
        let survivors: Vec<Vec<String>> = before.iter().map(|rows| rows.iter()
            .filter(|row| !row.contains("740000000000000041") && !row.contains("740000000000000042"))
            .cloned().collect()).collect();
        let dry = run_retention(&pool, RetentionMode::DryRun).await.expect("dry-run retention");
        assert_eq!(snapshot_tables(&pool).await, before, "dry-run never changes report data");
        assert!(dry.total_deleted() > 0, "dry-run must count expired report groups/children");
        let executed = run_retention(&pool, RetentionMode::Execute).await.expect("execute retention");
        assert_eq!(executed.total_deleted(), dry.total_deleted(), "same input, same dry/execute count");
        assert_eq!(snapshot_tables(&pool).await, survivors,
            "only expired closed groups and their children disappear; unrelated rows are byte-identical");
        assert_eq!(run_retention(&pool, RetentionMode::Execute).await
            .expect("idempotent retention").total_deleted(), 0);
    }).await;
}

async fn author(pool: &PgPool, id: &str, member: &str) {
    sqlx::query("UPDATE member_reports SET reported_member_id = $1 WHERE id = $2")
        .bind(member)
        .bind(id)
        .execute(pool)
        .await
        .expect("set synthetic reported author");
    sqlx::query("UPDATE report_snapshots SET author_id = $1 WHERE report_id = $2")
        .bind(member)
        .bind(id)
        .execute(pool)
        .await
        .expect("set synthetic snapshot author");
}

async fn audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM member_erasure_audit")
        .fetch_one(pool)
        .await
        .expect("count minimal receipts")
}

#[tokio::test]
async fn reporter_erasure_redacts_first_and_coalesced_identities_without_changing_evidence_or_aggregate(
) {
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        report(&pool, "740000000000000051", GUILD, SUBJECT, "closed").await;
        report(&pool, "740000000000000052", GUILD, REPORTER, "closed").await;
        for id in ["740000000000000051", "740000000000000052"] {
            author(&pool, id, OTHER).await;
        }
        sqlx::query("INSERT INTO report_reporters (report_id, reporter_id, reported_at) \
                     VALUES ('740000000000000052', $1, now())")
            .bind(SUBJECT).execute(&pool).await.expect("subject is a coalesced reporter too");
        report(&pool, "740000000000000053", OTHER_GUILD, SUBJECT, "closed").await;
        report(&pool, "740000000000000054", GUILD, REPORTER, "closed").await;
        author(&pool, "740000000000000054", OTHER).await;
        let before = snapshot_tables(&pool).await;
        let before_groups = [group(&pool, "740000000000000051").await,
                             group(&pool, "740000000000000052").await];
        let receipts = audit_count(&pool).await;
        let dry = erase_member(&pool, GUILD, SUBJECT, ErasureMode::DryRun).await.expect("reporter dry run");
        assert!(dry.iter().any(|row| row.table == "report_reporters" && row.rows == 2),
            "manifest must count both first and coalesced reporter rows");
        assert_eq!(snapshot_tables(&pool).await, before);
        assert_eq!(audit_count(&pool).await, receipts);
        let executed = erase_member(&pool, GUILD, SUBJECT, ErasureMode::Execute { actor: "acceptance" })
            .await.expect("reporter erasure execute");
        assert_eq!(dry, executed);
        for (id, mut expected) in ["740000000000000051", "740000000000000052"]
            .into_iter().zip(before_groups) {
            if id == "740000000000000051" {
                expected["reporter_id"] = Value::Null;
            }
            assert_eq!(group(&pool, id).await, expected,
                "reporter erasure keeps count, reason, disposition, timestamps and the other reporter");
        }
        let after = snapshot_tables(&pool).await;
        assert_eq!(after[1], before[1], "a reporter's request does not erase someone else's evidence");
        for i in [0, 2] {
            let unrelated = |rows: &Vec<String>| rows.iter()
                .filter(|row| row.contains("740000000000000053") || row.contains("740000000000000054"))
                .cloned().collect::<Vec<_>>();
            assert_eq!(unrelated(&after[i]), unrelated(&before[i]));
        }
        assert_eq!(sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM report_reporters rr JOIN member_reports r ON r.id = rr.report_id \
             WHERE r.guild_id = $1 AND rr.reporter_id = $2")
            .bind(GUILD).bind(SUBJECT).fetch_one(&pool).await.expect("remaining reporter identities"), 0);
        assert_eq!(sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM member_reports WHERE guild_id = $1 AND reporter_id = $2")
            .bind(GUILD).bind(SUBJECT).fetch_one(&pool).await.expect("remaining first reporters"), 0);
        assert!(erase_member(&pool, GUILD, SUBJECT, ErasureMode::DryRun).await
            .expect("post-erasure count").iter().all(|row| row.rows == 0));
        assert!(erase_member(&pool, GUILD, SUBJECT, ErasureMode::Execute { actor: "acceptance" }).await
            .expect("idempotent reporter erasure").iter().all(|row| row.rows == 0));
        assert_eq!(snapshot_tables(&pool).await, after);
        assert_eq!(audit_count(&pool).await, receipts + 2);
    }).await;
}

#[tokio::test]
async fn reported_author_erasure_removes_content_metadata_and_id_bearing_reason_but_keeps_tombstone(
) {
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        for (id, guild) in [
            ("740000000000000061", GUILD),
            ("740000000000000062", GUILD),
            ("740000000000000063", GUILD),
            ("740000000000000064", OTHER_GUILD),
        ] {
            report(&pool, id, guild, REPORTER, "closed").await;
        }
        author(&pool, "740000000000000063", OTHER).await;
        sqlx::query("UPDATE member_reports SET reason = 'Synthetic mention <@720000000000000001>' \
                     WHERE id = '740000000000000061'")
            .execute(&pool).await.expect("seed identity-bearing free text");
        let prose = "Synthetic prose without a subject ID; larger number 9720000000000000001 is not that ID";
        sqlx::query("UPDATE member_reports SET reason = $1 WHERE id = '740000000000000062'")
            .bind(prose).execute(&pool).await.expect("seed prose and digit-boundary control");
        let before = snapshot_tables(&pool).await;
        let tombstones = [group(&pool, "740000000000000061").await,
                          group(&pool, "740000000000000062").await];
        let dry = erase_member(&pool, GUILD, SUBJECT, ErasureMode::DryRun).await.expect("author dry run");
        assert!(dry.iter().any(|row| row.table == "report_snapshots" && row.rows == 2),
            "both of this author's snapshots must be in the manifest");
        assert_eq!(snapshot_tables(&pool).await, before);
        let executed = erase_member(&pool, GUILD, SUBJECT, ErasureMode::Execute { actor: "acceptance" })
            .await.expect("author erasure execute");
        assert_eq!(dry, executed);
        for (id, original) in ["740000000000000061", "740000000000000062"]
            .into_iter().zip(tombstones) {
            let kept = group(&pool, id).await;
            for field in ["id", "guild_id", "disposition", "created_at", "closed_at", "snapshot_status", "report_count"] {
                assert_eq!(kept[field], original[field], "accountability/provenance field {field} survives");
            }
            let snapshot: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM report_snapshots t WHERE report_id = $1")
                .bind(id).fetch_one(&pool).await.expect("erased marker instead of preserved content");
            assert_eq!(snapshot["content"], "[erased]");
            assert_eq!(snapshot["attachments"], json!([]));
            assert_eq!(snapshot["attachment_overflow"], false);
            // Subject/message IDs are allowed only as manifest-documented tombstone scope.
            assert_eq!(snapshot["author_id"], SUBJECT);
            assert_eq!(kept["reported_member_id"], SUBJECT);
            assert_eq!(kept["message_id"], id);
        }
        let redacted = group(&pool, "740000000000000061").await;
        assert!(!redacted["reason"].as_str().unwrap_or("").contains(SUBJECT));
        assert_eq!(group(&pool, "740000000000000062").await["reason"], prose,
            "manifest digit boundaries are not a prose recognizer");
        let after = snapshot_tables(&pool).await;
        for i in 0..3 {
            let unrelated = |rows: &Vec<String>| rows.iter()
                .filter(|row| row.contains("740000000000000063") || row.contains("740000000000000064"))
                .cloned().collect::<Vec<_>>();
            assert_eq!(unrelated(&after[i]), unrelated(&before[i]), "unrelated member/guild rows unchanged");
        }
        assert!(erase_member(&pool, GUILD, SUBJECT, ErasureMode::DryRun).await
            .expect("no remaining erasable author references").iter().all(|row| row.rows == 0));
        assert!(erase_member(&pool, GUILD, SUBJECT, ErasureMode::Execute { actor: "acceptance" }).await
            .expect("idempotent author erasure").iter().all(|row| row.rows == 0));
        assert_eq!(snapshot_tables(&pool).await, after);
    }).await;
}

#[tokio::test]
async fn report_manifest_covers_real_schema_and_new_identity_column_refuses_both_modes_atomically()
{
    with_database(|pool| async move {
        require_report_schema(&pool).await;
        report(&pool, "740000000000000071", GUILD, SUBJECT, "closed").await;
        let mut conn = pool.acquire().await.expect("inspect manifest coverage");
        assert!(
            schema_gaps(&mut conn)
                .await
                .expect("real migrated schema coverage")
                .is_empty(),
            "all report identities require erasure coverage or column-specific tombstone reasons"
        );
        sqlx::query("ALTER TABLE member_reports ADD COLUMN observer_user_id text")
            .execute(&pool)
            .await
            .expect("synthetic future identity migration");
        sqlx::query("UPDATE member_reports SET observer_user_id = $1")
            .bind(SUBJECT)
            .execute(&pool)
            .await
            .expect("seed uncovered identity");
        let gaps = schema_gaps(&mut conn)
            .await
            .expect("detect future identity gap");
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("member_reports.observer_user_id")),
            "a new recipient/observer column must not silently escape the manifest: {gaps:?}"
        );
        drop(conn);
        let before = snapshot_tables(&pool).await;
        let receipts = audit_count(&pool).await;
        assert!(erase_member(&pool, GUILD, SUBJECT, ErasureMode::DryRun)
            .await
            .is_err());
        assert!(erase_member(
            &pool,
            GUILD,
            SUBJECT,
            ErasureMode::Execute {
                actor: "acceptance"
            }
        )
        .await
        .is_err());
        assert_eq!(
            snapshot_tables(&pool).await,
            before,
            "coverage refusal is fully atomic"
        );
        assert_eq!(
            audit_count(&pool).await,
            receipts,
            "refusal is not an erasure receipt"
        );
        sqlx::query("ALTER TABLE member_reports DROP COLUMN observer_user_id")
            .execute(&pool)
            .await
            .expect("remove fixture-only column, not a production migration");
        erase_member(
            &pool,
            GUILD,
            SUBJECT,
            ErasureMode::Execute {
                actor: "acceptance",
            },
        )
        .await
        .expect("covered schema permits both-party erasure after the probe is removed");
        let row = group(&pool, "740000000000000071").await;
        assert_eq!(row["reporter_id"], Value::Null);
        assert_eq!(row["report_count"], 3);
        let content: String = sqlx::query_scalar(
            "SELECT content FROM report_snapshots WHERE report_id = '740000000000000071'",
        )
        .fetch_one(&pool)
        .await
        .expect("both-party erased marker");
        assert_eq!(content, "[erased]");
    })
    .await;
}
