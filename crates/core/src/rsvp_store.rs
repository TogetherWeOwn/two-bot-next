//! RSVP + host check-in sqlx store (TOG-10083).
//!
//! Ports the `AnnouncementsStore` RSVP/audit writes (`putRsvp`, `listRsvps`,
//! `audit` in legacy `src/announcements/store.ts`) and the community-facts
//! attendance write (`recordAttendance` in legacy
//! `src/analytics/communityFacts.ts`) onto the `0160_rsvp` tables. The pure
//! domain stays in [`crate::rsvp`]: this module only moves rows, keyed by the
//! helpers there, so the SQL is a thin transliteration of the legacy queries
//! (`?` placeholders become `$n`; timestamps bind as `timestamptz`).
//!
//! Behind the `db` feature so domain unit tests never need a Postgres driver.
//! The interaction router (TOG-10075) calls these; until it lands, the outcome
//! types (`bool` inserted flags, [`RsvpTransition`]) are the integration
//! surface — no private dispatcher lives here.
//!
//! Legacy table/column names are kept exactly so RSVP rows stay consistent
//! with what two-web-next reads.

use sqlx::{Pool, Postgres};

use super::rsvp::{
    checkin_idempotency_key, checkin_metadata_json, checkin_source, checkin_source_event_id,
    AttendanceProof, RsvpAudit, RsvpRecord, RsvpStatus, RsvpTransition, ATTENDANCE_EVENT_TYPE,
};

/// RSVP store failure: transport plus the one domain parse a stored row can
/// still trigger (a `status` outside the legacy CHECK, e.g. written by hand).
#[derive(Debug, thiserror::Error)]
pub enum RsvpStoreError {
    #[error("rsvp store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("stored RSVP status {0:?} is not a known response")]
    UnknownStatus(String),
}

/// Upsert one RSVP row and report the transition (legacy `putRsvp` blind
/// upsert, plus a locked read-back so going/interested/declined transitions
/// are observable). Last writer wins on the row; concurrent same-key writers
/// serialize on a transaction-scoped key lock even before the row exists,
/// so the reported `previous` is never torn.
/// Every call site also writes an audit row via [`write_audit`] — legacy
/// audits every response, including repeats.
pub async fn put_rsvp(
    pool: &Pool<Postgres>,
    record: &RsvpRecord,
) -> Result<RsvpTransition, RsvpStoreError> {
    let mut tx = pool.begin().await?;
    // FOR UPDATE cannot lock an absent row. Serialize the complete identity
    // first; tuple encoding avoids delimiter ambiguity, and a hash collision
    // only causes extra contention, never an incorrect transition.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
            jsonb_build_array($1::text, $2::text, $3::text)::text, 0))",
    )
    .bind(&record.guild_id)
    .bind(&record.event_id)
    .bind(&record.user_id)
    .execute(&mut *tx)
    .await?;
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT status FROM event_rsvps
         WHERE guild_id = $1 AND event_id = $2 AND user_id = $3 FOR UPDATE",
    )
    .bind(&record.guild_id)
    .bind(&record.event_id)
    .bind(&record.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at)
         VALUES ($1, $2, $3, $4, $5::timestamptz)
         ON CONFLICT (guild_id, event_id, user_id) DO UPDATE SET
           status = excluded.status, responded_at = excluded.responded_at",
    )
    .bind(&record.guild_id)
    .bind(&record.event_id)
    .bind(&record.user_id)
    .bind(record.status.as_str())
    .bind(&record.responded_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let previous = previous
        .map(|s| RsvpStatus::parse(&s).map_err(|_| RsvpStoreError::UnknownStatus(s)))
        .transpose()?;
    Ok(RsvpTransition {
        previous,
        current: record.status,
    })
}

/// List one event's RSVPs in legacy order (`responded_at, user_id`); the
/// caller partitions with [`crate::rsvp::partition_rsvps`].
pub async fn list_rsvps(
    pool: &Pool<Postgres>,
    guild_id: &str,
    event_id: &str,
) -> Result<Vec<RsvpRecord>, RsvpStoreError> {
    let rows: Vec<(
        String,
        String,
        String,
        String,
        sqlx::types::time::OffsetDateTime,
    )> = sqlx::query_as(
        "SELECT guild_id, event_id, user_id, status, responded_at FROM event_rsvps
             WHERE guild_id = $1 AND event_id = $2 ORDER BY responded_at, user_id",
    )
    .bind(guild_id)
    .bind(event_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(guild_id, event_id, user_id, status, responded_at)| {
            let status =
                RsvpStatus::parse(&status).map_err(|_| RsvpStoreError::UnknownStatus(status))?;
            Ok(RsvpRecord {
                guild_id,
                event_id,
                user_id,
                status,
                responded_at: iso_millis(responded_at),
            })
        })
        .collect()
}

/// Append one `announcements_audit_log` row (legacy `audit`: the id and `at`
/// timestamp are caller inputs — this module generates no randomness).
pub async fn write_audit(pool: &Pool<Postgres>, audit: &RsvpAudit) -> Result<(), RsvpStoreError> {
    sqlx::query(
        "INSERT INTO announcements_audit_log
           (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz)",
    )
    .bind(&audit.id)
    .bind(&audit.guild_id)
    .bind(&audit.actor_id)
    .bind(&audit.action)
    .bind(&audit.target_key)
    .bind(&audit.outcome)
    .bind(&audit.reason)
    .bind(&audit.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// One host check-in write (legacy `recordAttendance` inputs, minus the
/// classifier: this slice records the minimal bot/human rule via
/// [`crate::rsvp::checkin_classification`], and the store takes explicit
/// `classifier_version`/`classification`/`matched_rule` so the S5 classifier
/// plugs in without a signature change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckinWrite {
    pub guild_id: String,
    pub event_occurrence_id: String,
    pub member_id: String,
    /// ISO-8601 UTC (legacy 0018 stores `occurred_at` as TEXT).
    pub occurred_at: String,
    /// The `rsvp` proof writes nothing and reports `false` (legacy parity).
    pub proof: AttendanceProof,
    pub classifier_version: String,
    /// One of the seven legacy classifications (`eligible_human`, `bot`, …).
    pub classification: String,
    pub matched_rule: String,
}

/// Record one verified-attendance fact. Returns `true` when the fact was
/// inserted, `false` when the idempotency key already held it (retry dedupe)
/// or the proof is `rsvp` (legacy writes nothing for it).
pub async fn record_checkin(
    pool: &Pool<Postgres>,
    checkin: &CheckinWrite,
) -> Result<bool, RsvpStoreError> {
    if !checkin.proof.writes_fact() {
        return Ok(false);
    }
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO community_facts
           (guild_id, event_type, source_event_id, actor_id, occurred_at, source,
            classifier_version, classification, matched_rule, metadata, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(&checkin.guild_id)
    .bind(ATTENDANCE_EVENT_TYPE)
    .bind(checkin_source_event_id(
        &checkin.event_occurrence_id,
        &checkin.member_id,
    ))
    .bind(&checkin.member_id)
    .bind(&checkin.occurred_at)
    .bind(checkin_source(&checkin.event_occurrence_id))
    .bind(&checkin.classifier_version)
    .bind(&checkin.classification)
    .bind(&checkin.matched_rule)
    .bind(checkin_metadata_json(
        &checkin.event_occurrence_id,
        checkin.proof,
    ))
    .bind(checkin_idempotency_key(
        &checkin.event_occurrence_id,
        &checkin.member_id,
    ))
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

/// Format a `timestamptz` as millisecond ISO-8601 UTC (`…T…:….000Z`), the
/// canonical form the domain writes and legacy round-trips.
fn iso_millis(dt: sqlx::types::time::OffsetDateTime) -> String {
    let dt = dt.to_offset(sqlx::types::time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond()
    )
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::super::rsvp::{checkin_classification, AttendanceProof, RsvpStatus};
    use super::*;

    /// Only an absent URL skips the test. Configured setup failures are errors.
    async fn test_pool(prefix: &str) -> Result<Option<(Pool<Postgres>, String)>, sqlx::Error> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
        };
        test_pool_with_url(prefix, &url).await.map(Some)
    }

    async fn test_pool_with_url(
        prefix: &str,
        url: &str,
    ) -> Result<(Pool<Postgres>, String), sqlx::Error> {
        let options: PgConnectOptions = url.parse()?;
        assert_eq!(
            options.get_host(),
            "agent-testdb",
            "test-container-only URL"
        );
        assert_eq!(
            options.get_username(),
            "agent_test",
            "test-container-only user"
        );
        // No shared fixed-name fixtures: each invocation owns its schema,
        // including concurrent invocations of the same test in other processes.
        static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!(
            "{prefix}_{}_{nonce}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        assert!(is_test_schema_name(&schema), "safe unique test schema");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        // SAFETY: only the guarded identifier interpolates; values bind.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        // Execute the entire migration, including multi-statement DDL.
        sqlx::raw_sql(include_str!("../../cutover/migrations/0160_rsvp.sql"))
            .execute(&pool)
            .await?;
        Ok((pool, schema))
    }

    /// Test-only schema names: lowercase identifier characters, so DDL
    /// interpolation cannot break out of the identifier position.
    fn is_test_schema_name(schema: &str) -> bool {
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
        assert!(is_test_schema_name(schema));
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    fn rsvp(status: RsvpStatus, user: &str, at: &str) -> RsvpRecord {
        RsvpRecord {
            guild_id: "1545644954272137297".to_owned(),
            event_id: "1546451670500642999".to_owned(),
            user_id: user.to_owned(),
            status,
            responded_at: at.to_owned(),
        }
    }

    #[tokio::test]
    async fn put_reports_new_then_change() {
        let schema = "tog_10083_rsvp_put";
        let Some((pool, schema)) = test_pool(schema).await.expect("test database setup") else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let first = put_rsvp(
            &pool,
            &rsvp(RsvpStatus::Going, "u1", "2026-09-10T10:00:00.000Z"),
        )
        .await
        .expect("first write");
        assert!(first.is_new());
        let moved = put_rsvp(
            &pool,
            &rsvp(RsvpStatus::Interested, "u1", "2026-09-10T10:01:00.000Z"),
        )
        .await
        .expect("second write");
        assert_eq!(moved.previous, Some(RsvpStatus::Going));
        assert!(moved.changed());
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RsvpStatus::Interested);
        assert_eq!(rows[0].responded_at, "2026-09-10T10:01:00.000Z");
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn concurrent_first_responses_report_one_new_transition() {
        let Some((pool, schema)) = test_pool("rsvp_concurrent")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let going = rsvp(RsvpStatus::Going, &schema, "2026-09-10T10:00:00.000Z");
        let interested = RsvpRecord {
            status: RsvpStatus::Interested,
            ..going.clone()
        };
        // Hold the empty key before either writer starts. Both must wait here,
        // not read an absent row then race their upserts with previous=None.
        let mut guard = pool.begin().await.expect("guard transaction");
        let guard_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *guard)
            .await
            .expect("guard pid");
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended(
                jsonb_build_array($1::text, $2::text, $3::text)::text, 0))",
        )
        .bind(&going.guild_id)
        .bind(&going.event_id)
        .bind(&going.user_id)
        .execute(&mut *guard)
        .await
        .expect("hold empty RSVP key");
        let first_pool = pool.clone();
        let first = tokio::spawn(async move { put_rsvp(&first_pool, &going).await });
        let second_pool = pool.clone();
        let second = tokio::spawn(async move { put_rsvp(&second_pool, &interested).await });
        let waiting = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_locks waiter
                     WHERE waiter.locktype = 'advisory' AND NOT waiter.granted
                       AND (waiter.classid, waiter.objid, waiter.objsubid) IN
                         (SELECT classid, objid, objsubid FROM pg_locks
                          WHERE locktype = 'advisory' AND granted AND pid = $1)",
                )
                .bind(guard_pid)
                .fetch_one(&mut *guard)
                .await
                .expect("key waiters");
                if count == 2 {
                    break;
                }
                if first.is_finished() || second.is_finished() {
                    panic!("RSVP writer bypassed the empty-key lock");
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        guard.commit().await.expect("release key");
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            (
                first.await.expect("first task").expect("first write"),
                second.await.expect("second task").expect("second write"),
            )
        })
        .await
        .expect("writers finish");
        waiting.expect("both concurrent writers queued on empty key");
        let (new, changed) = if first.is_new() {
            (first, second)
        } else {
            (second, first)
        };
        assert!(new.is_new());
        assert_eq!(changed.previous, Some(new.current));
        assert!(changed.changed());
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("final RSVP");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, changed.current);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn same_fixture_prefix_allocates_independent_schemas() {
        let Some((first, first_schema)) = test_pool("rsvp_isolation").await.expect("first setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let record = rsvp(RsvpStatus::Going, "u1", "2026-09-10T10:00:00.000Z");
        put_rsvp(&first, &record)
            .await
            .expect("first fixture write");
        let (second, second_schema) = test_pool("rsvp_isolation")
            .await
            .expect("second setup")
            .expect("configured");
        assert_ne!(first_schema, second_schema);
        assert_eq!(
            list_rsvps(&first, &record.guild_id, &record.event_id)
                .await
                .expect("first fixture")
                .len(),
            1
        );
        assert!(list_rsvps(&second, &record.guild_id, &record.event_id)
            .await
            .expect("second fixture")
            .is_empty());
        drop_schema(&first, &first_schema).await;
        assert!(put_rsvp(&second, &record)
            .await
            .expect("surviving fixture write")
            .is_new());
        drop_schema(&second, &second_schema).await;
    }

    #[tokio::test]
    async fn configured_invalid_url_is_an_error_not_a_skip() {
        for url in ["", "not a database URL"] {
            assert!(test_pool_with_url("rsvp_invalid", url).await.is_err());
        }
    }

    #[tokio::test]
    async fn list_orders_by_response_time_then_user() {
        let schema = "tog_10083_rsvp_list";
        let Some((pool, schema)) = test_pool(schema).await.expect("test database setup") else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        put_rsvp(
            &pool,
            &rsvp(RsvpStatus::Going, "u2", "2026-09-10T10:01:00.000Z"),
        )
        .await
        .expect("write u2");
        put_rsvp(
            &pool,
            &rsvp(RsvpStatus::Declined, "u1", "2026-09-10T10:00:00.000Z"),
        )
        .await
        .expect("write u1");
        let totals = super::super::rsvp::partition_rsvps(
            &list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
                .await
                .expect("list"),
        );
        assert_eq!(totals.counts(), (1, 0, 1));
        assert_eq!(totals.going, ["u2"]);
        assert_eq!(totals.declined, ["u1"]);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn audit_appends_every_response() {
        let schema = "tog_10083_rsvp_audit";
        let Some((pool, schema)) = test_pool(schema).await.expect("test database setup") else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let record = rsvp(RsvpStatus::Going, "u1", "2026-09-10T10:00:00.000Z");
        put_rsvp(&pool, &record).await.expect("write");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-1", &record))
            .await
            .expect("audit 1");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-2", &record))
            .await
            .expect("audit 2");
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM announcements_audit_log WHERE action = 'event.rsvp'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count.0, 2);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn checkin_dedupes_and_ignores_rsvp_proof() {
        let schema = "tog_10083_rsvp_checkin";
        let Some((pool, schema)) = test_pool(schema).await.expect("test database setup") else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let human = checkin_classification(false);
        let write = CheckinWrite {
            guild_id: "1545644954272137297".to_owned(),
            event_occurrence_id: "event-1".to_owned(),
            member_id: "human-1".to_owned(),
            occurred_at: "2026-09-02T10:00:00.000Z".to_owned(),
            proof: AttendanceProof::HostCheckin,
            classifier_version: "community-test-v1".to_owned(),
            classification: human.classification.to_owned(),
            matched_rule: human.matched_rule.to_owned(),
        };
        assert!(record_checkin(&pool, &write).await.expect("first"));
        assert!(!record_checkin(&pool, &write).await.expect("retry"));
        let rsvp_proof = CheckinWrite {
            proof: AttendanceProof::Rsvp,
            ..write.clone()
        };
        assert!(!record_checkin(&pool, &rsvp_proof).await.expect("rsvp"));
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT event_type, source_event_id, classification, metadata FROM community_facts",
        )
        .fetch_all(&pool)
        .await
        .expect("facts");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "event_attended");
        assert_eq!(rows[0].1, "event-1:human-1");
        assert_eq!(rows[0].2, "eligible_human");
        assert_eq!(
            rows[0].3,
            r#"{"eventOccurrenceId":"event-1","proof":"host_checkin"}"#
        );
        drop_schema(&pool, &schema).await;
    }
}
