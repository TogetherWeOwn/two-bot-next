//! Member-report storage hardening (not acceptance).
//!
//! Pins the guards the protected acceptance suite leaves open: the reporter cap
//! on `UPDATE` and under concurrency, the snapshot and group shape checks, the
//! retention user-deletion redaction callsite, dry-versus-execute parity for the
//! guild projection, and the two erasure sub-clauses whose fixtures were vacuous.
//! All fixtures are synthetic and use only an owned disposable test database.

use std::time::Duration;

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_cutover::{
    member_erasure::{erase_member, ErasureMode},
    retention::{
        fulfill_user_deletion, handle_guild_delete, request_user_deletion, run_retention,
        RetentionMode,
    },
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "810000000000000001";
const SUBJECT: &str = "820000000000000001";
const REPORTER: &str = "820000000000000002";
const OTHER: &str = "820000000000000003";
const CHECK_VIOLATION: &str = "23514";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the member-report hardening suite"
            );
            eprintln!("SKIP member-report hardening: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

fn code_of(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    result.err().and_then(|e| {
        e.as_database_error()
            .and_then(|d| d.code().map(|c| c.to_string()))
    })
}

/// One group with no snapshot or reporters; `id` doubles as the message id.
async fn group(
    pool: &PgPool,
    id: &str,
    guild: &str,
    reporter: &str,
    reported: &str,
    status: &str,
    closed_days_ago: Option<i32>,
) {
    sqlx::query(
        "INSERT INTO member_reports \
         (id, guild_id, channel_id, message_id, reported_member_id, reporter_id, \
          report_count, reason, snapshot_status, status, created_at, closed_at, disposition) \
         VALUES ($1, $2, '830000000000000001', $1, $3, $4, 3, 'Synthetic report', 'present', $5, \
          now() - interval '200 days', \
          CASE WHEN $6::int IS NULL THEN NULL ELSE now() - make_interval(days => $6::int) END, \
          CASE WHEN $5 = 'closed' THEN 'informational' ELSE NULL END)",
    )
    .bind(id)
    .bind(guild)
    .bind(reported)
    .bind(reporter)
    .bind(status)
    .bind(closed_days_ago)
    .execute(pool)
    .await
    .unwrap();
}

async fn snapshot(pool: &PgPool, id: &str, author: &str, content: &str, overflow: bool) {
    sqlx::query(
        "INSERT INTO report_snapshots \
         (report_id, author_id, author_bot, message_created_at, content, attachments, attachment_overflow) \
         VALUES ($1, $2, false, now() - interval '201 days', $3, \
                 '[{\"filename\": \"synthetic.txt\"}]'::jsonb, $4)",
    )
    .bind(id)
    .bind(author)
    .bind(content)
    .bind(overflow)
    .execute(pool)
    .await
    .unwrap();
}

async fn reporter(pool: &PgPool, id: &str, member: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO report_reporters (report_id, reporter_id) VALUES ($1, $2)")
        .bind(id)
        .bind(member)
        .execute(pool)
        .await
        .map(|_| ())
}

async fn row(pool: &PgPool, table: &'static str, key: &str, id: &str) -> Value {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT to_jsonb(t) FROM {table} t WHERE {key} = $1"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn reporter_cap_rejects_an_update_that_moves_a_reporter_into_a_full_group() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    group(
        pool,
        "840000000000000001",
        GUILD,
        REPORTER,
        SUBJECT,
        "open",
        None,
    )
    .await;
    group(
        pool,
        "840000000000000002",
        GUILD,
        REPORTER,
        SUBJECT,
        "open",
        None,
    )
    .await;
    for n in 0..10 {
        reporter(pool, "840000000000000001", &format!("85000000000000000{n}"))
            .await
            .unwrap();
    }
    reporter(pool, "840000000000000002", "850000000000000099")
        .await
        .unwrap();

    let moved = sqlx::query(
        "UPDATE report_reporters SET report_id = '840000000000000001' \
         WHERE report_id = '840000000000000002'",
    )
    .execute(pool)
    .await;
    assert_eq!(
        code_of(moved).as_deref(),
        Some(CHECK_VIOLATION),
        "an UPDATE of report_id must not smuggle an eleventh identity into a full group"
    );
    // Rewriting report_id to itself, or any other column, moves nobody.
    sqlx::query(
        "UPDATE report_reporters SET report_id = report_id WHERE report_id = '840000000000000001'",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE report_reporters SET reported_at = now() WHERE report_id = '840000000000000001'",
    )
    .execute(pool)
    .await
    .unwrap();
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT report_id, count(*) FROM report_reporters GROUP BY report_id ORDER BY report_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        counts,
        vec![
            ("840000000000000001".to_owned(), 10),
            ("840000000000000002".to_owned(), 1)
        ]
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn racing_inserts_for_the_tenth_reporter_slot_admit_exactly_one() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    group(
        pool,
        "840000000000000011",
        GUILD,
        REPORTER,
        SUBJECT,
        "open",
        None,
    )
    .await;
    for n in 0..9 {
        reporter(pool, "840000000000000011", &format!("85000000000000001{n}"))
            .await
            .unwrap();
    }
    // The first transaction holds the tenth slot uncommitted; the second must
    // wait on the parent-row lock and then see the committed tenth identity.
    let mut first = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO report_reporters (report_id, reporter_id) VALUES ('840000000000000011', '850000000000000091')",
    )
    .execute(&mut *first)
    .await
    .unwrap();
    let racer_pool = pool.clone();
    let racer = tokio::spawn(async move {
        reporter(&racer_pool, "840000000000000011", "850000000000000092").await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !racer.is_finished(),
        "the second insert must block on the parent-row lock, not count nine concurrently"
    );
    first.commit().await.unwrap();
    let raced = tokio::time::timeout(Duration::from_secs(10), racer)
        .await
        .expect("racer finishes once the lock is released")
        .unwrap();
    let code = raced.err().and_then(|e| {
        e.as_database_error()
            .and_then(|d| d.code().map(|c| c.to_string()))
    });
    assert_eq!(code.as_deref(), Some(CHECK_VIOLATION));
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM report_reporters")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(stored, 10);
    db.close().await.unwrap();
}

#[tokio::test]
async fn schema_guards_reject_oversized_metadata_long_dispositions_and_inconsistent_closure() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    group(
        pool,
        "840000000000000021",
        GUILD,
        REPORTER,
        SUBJECT,
        "open",
        None,
    )
    .await;
    snapshot(pool, "840000000000000021", SUBJECT, "Synthetic", false).await;

    // `[{"k": "<n x>"}]` is eleven octets plus the payload, so 16373 fills the
    // 16384-octet bound exactly and one more overflows it with a single entry.
    for (payload, accepted) in [(16373, true), (16374, false)] {
        let result = sqlx::query(
            "UPDATE report_snapshots \
             SET attachments = jsonb_build_array(jsonb_build_object('k', repeat('x', $1))) \
             WHERE report_id = '840000000000000021'",
        )
        .bind(payload)
        .execute(pool)
        .await;
        if accepted {
            result.unwrap();
        } else {
            assert_eq!(
                code_of(result).as_deref(),
                Some(CHECK_VIOLATION),
                "attachment metadata beyond 16384 octets is rejected at ten entries or fewer"
            );
        }
    }

    for (n, status, closed, disposition, ok) in [
        (30, "closed", "now()", "repeat('d', 64)", true),
        (31, "closed", "now()", "repeat('d', 65)", false),
        (32, "closed", "NULL", "'informational'", false),
        (33, "closed", "now()", "NULL", false),
        (34, "open", "now()", "NULL", false),
    ] {
        let id = format!("8400000000000000{n}");
        // Audited: the interpolated fragments are fixed literals from this table.
        let result = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO member_reports \
             (id, guild_id, message_id, reported_member_id, reporter_id, reason, snapshot_status, \
              status, closed_at, disposition) \
             VALUES ('{id}', '{GUILD}', '{id}', '{SUBJECT}', '{REPORTER}', 'Synthetic', 'present', \
                     '{status}', {closed}, {disposition})"
        )))
        .execute(pool)
        .await;
        if ok {
            result.unwrap();
        } else {
            assert_eq!(
                code_of(result).as_deref(),
                Some(CHECK_VIOLATION),
                "{status} with closed_at {closed} and disposition {disposition} must be rejected"
            );
        }
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn retention_user_deletion_redacts_reports_instead_of_deleting_the_tombstone() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Recently closed, so the 90-day expiry cannot be what leaves it behind.
    let id = "840000000000000041";
    group(pool, id, GUILD, SUBJECT, SUBJECT, "closed", Some(1)).await;
    snapshot(pool, id, SUBJECT, "Synthetic preserved evidence", true).await;
    reporter(pool, id, SUBJECT).await.unwrap();
    reporter(pool, id, OTHER).await.unwrap();
    let before = row(pool, "member_reports", "id", id).await;

    let request_id = request_user_deletion(pool, GUILD, SUBJECT).await.unwrap();
    fulfill_user_deletion(pool, &request_id).await.unwrap();

    let kept = row(pool, "member_reports", "id", id).await;
    assert_eq!(
        kept["reporter_id"],
        Value::Null,
        "first reporter is redacted"
    );
    for field in [
        "reported_member_id",
        "report_count",
        "disposition",
        "closed_at",
        "status",
    ] {
        assert_eq!(
            kept[field], before[field],
            "tombstone field {field} survives"
        );
    }
    let evidence = row(pool, "report_snapshots", "report_id", id).await;
    assert_eq!(evidence["content"], "[erased]");
    assert_eq!(evidence["attachments"], json!([]));
    assert_eq!(evidence["attachment_overflow"], false);
    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT reporter_id FROM report_reporters WHERE report_id = $1 ORDER BY reporter_id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        remaining,
        vec![OTHER.to_owned()],
        "only the requester's identity goes"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn dry_run_guild_projection_counts_report_rows_exactly_as_execute_removes_them() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let gone = "810000000000000009";
    // An expired closed group leaves in the time-window stage, children included.
    group(
        pool,
        "840000000000000051",
        gone,
        OTHER,
        OTHER,
        "closed",
        Some(120),
    )
    .await;
    snapshot(
        pool,
        "840000000000000051",
        OTHER,
        "Synthetic expired",
        false,
    )
    .await;
    reporter(pool, "840000000000000051", OTHER).await.unwrap();
    // Live groups that match the deletion user as reporter and as author are
    // redacted by the user stage, then removed by the guild purge.
    group(
        pool,
        "840000000000000052",
        gone,
        SUBJECT,
        OTHER,
        "open",
        None,
    )
    .await;
    snapshot(
        pool,
        "840000000000000052",
        OTHER,
        "Synthetic live reporter",
        false,
    )
    .await;
    reporter(pool, "840000000000000052", SUBJECT).await.unwrap();
    // The other reporter survives user erasure because the parent is redacted,
    // not deleted; its row must still count toward the guild purge.
    reporter(pool, "840000000000000052", OTHER).await.unwrap();
    group(
        pool,
        "840000000000000053",
        gone,
        OTHER,
        SUBJECT,
        "claimed",
        None,
    )
    .await;
    snapshot(
        pool,
        "840000000000000053",
        SUBJECT,
        "Synthetic live author",
        false,
    )
    .await;
    reporter(pool, "840000000000000053", OTHER).await.unwrap();
    for member in [SUBJECT, OTHER] {
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(gone)
            .bind(member)
            .execute(pool)
            .await
            .unwrap();
    }
    request_user_deletion(pool, gone, SUBJECT).await.unwrap();
    assert!(handle_guild_delete(pool, gone, None).await.unwrap());

    let dry = run_retention(pool, RetentionMode::DryRun).await.unwrap();
    let live = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        live.member_reports_expired, 1,
        "one expired group, got {live:?}"
    );
    assert!(
        live.guild_rows_purged > 0,
        "the purge removes the live groups, got {live:?}"
    );
    assert_eq!(live.deletions_completed, 1, "got {live:?}");
    assert_eq!(
        (
            dry.member_reports_expired,
            dry.guild_rows_purged,
            dry.deletions_completed,
            dry.total_deleted()
        ),
        (
            live.member_reports_expired,
            live.guild_rows_purged,
            live.deletions_completed,
            live.total_deleted()
        ),
        "dry run must project execute exactly, got dry {dry:?} live {live:?}"
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM member_reports) + (SELECT count(*) FROM report_snapshots) \
         + (SELECT count(*) FROM report_reporters)",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(left, 0, "nothing survives the departed guild");
    db.close().await.unwrap();
}

#[tokio::test]
async fn author_erasure_clears_a_lone_overflow_flag_and_keeps_trailing_digit_neighbours() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    // Already-erased content with only the overflow flag left: the flag alone
    // must select the row and the redaction must clear it.
    let flagged = "840000000000000061";
    group(pool, flagged, GUILD, REPORTER, SUBJECT, "closed", Some(1)).await;
    snapshot(pool, flagged, SUBJECT, "[erased]", true).await;
    sqlx::query("UPDATE report_snapshots SET attachments = '[]'::jsonb WHERE report_id = $1")
        .bind(flagged)
        .execute(pool)
        .await
        .unwrap();
    // A message can literally contain '[erased]'; attachment metadata alone
    // must still select it even when the overflow flag is false.
    let attached = "840000000000000064";
    group(pool, attached, GUILD, REPORTER, SUBJECT, "closed", Some(1)).await;
    snapshot(pool, attached, SUBJECT, "[erased]", false).await;
    // The erased ID followed by another digit is a different number.
    let neighbour = "840000000000000062";
    let prose = format!("Synthetic order {SUBJECT}9 is a different number");
    group(pool, neighbour, GUILD, REPORTER, OTHER, "closed", Some(1)).await;
    sqlx::query("UPDATE member_reports SET reason = $1 WHERE id = $2")
        .bind(&prose)
        .bind(neighbour)
        .execute(pool)
        .await
        .unwrap();
    let mention = "840000000000000063";
    group(pool, mention, GUILD, REPORTER, OTHER, "closed", Some(1)).await;
    sqlx::query("UPDATE member_reports SET reason = $1 WHERE id = $2")
        .bind(format!("Synthetic mention <@{SUBJECT}> and {SUBJECT}."))
        .bind(mention)
        .execute(pool)
        .await
        .unwrap();

    erase_member(
        pool,
        GUILD,
        SUBJECT,
        ErasureMode::Execute { actor: "hardening" },
    )
    .await
    .unwrap();

    let snap = row(pool, "report_snapshots", "report_id", flagged).await;
    assert_eq!(
        snap["attachment_overflow"], false,
        "the lone overflow flag is cleared"
    );
    assert_eq!(snap["content"], "[erased]");
    let snap = row(pool, "report_snapshots", "report_id", attached).await;
    assert_eq!(snap["content"], "[erased]");
    assert_eq!(snap["attachments"], json!([]), "lone metadata is cleared");
    assert_eq!(snap["attachment_overflow"], false);
    assert_eq!(
        row(pool, "member_reports", "id", neighbour).await["reason"],
        prose,
        "a trailing digit makes it a different number"
    );
    assert_eq!(
        row(pool, "member_reports", "id", mention).await["reason"],
        "Synthetic mention <@[erased]> and [erased]."
    );
    let remaining = erase_member(pool, GUILD, SUBJECT, ErasureMode::DryRun)
        .await
        .unwrap();
    assert!(
        remaining.iter().all(|row| row.rows == 0),
        "no predicate may keep selecting redacted rows or trailing-digit neighbours: {remaining:?}"
    );
    let repeated = erase_member(
        pool,
        GUILD,
        SUBJECT,
        ErasureMode::Execute { actor: "hardening" },
    )
    .await
    .unwrap();
    assert!(
        repeated.iter().all(|row| row.rows == 0),
        "repeated author erasure changes no rows: {repeated:?}"
    );
    db.close().await.unwrap();
}
