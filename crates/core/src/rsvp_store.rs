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
/// serialize on the row lock, so the reported `previous` is never torn.
/// Every call site also writes an audit row via [`write_audit`] — legacy
/// audits every response, including repeats.
pub async fn put_rsvp(
    pool: &Pool<Postgres>,
    record: &RsvpRecord,
) -> Result<RsvpTransition, RsvpStoreError> {
    let mut tx = pool.begin().await?;
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

    /// Open an isolated test schema and apply the slice DDL. Returns `None`
    /// (skip) unless `TWO_TEST_DATABASE_URL` points at Postgres — CI runs
    /// without a database, so store tests must degrade to a skip, never fail.
    async fn test_pool(schema: &str) -> Option<Pool<Postgres>> {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return None;
        }
        // Schema names are fixed per-test identifiers; the guard below keeps
        // them interpolation-safe (values always bind — only DDL names
        // interpolate, and only after this check).
        if !is_test_schema_name(schema) {
            return None;
        }
        let admin = match PgPoolOptions::new().max_connections(1).connect(&url).await {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("rsvp_store test setup: admin connect failed: {e}");
                return None;
            }
        };
        // Idempotent setup: a crashed earlier run may have left this schema
        // behind, and stale rows would pollute exact-count assertions.
        // SAFETY: `schema` passed `is_test_schema_name`, so interpolation
        // cannot break out of the identifier position.
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE"
        )))
        .execute(&admin)
        .await
        {
            eprintln!("rsvp_store test setup: drop schema failed: {e}");
            return None;
        }
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
        {
            eprintln!("rsvp_store test setup: create schema failed: {e}");
            return None;
        }
        admin.close().await;
        let mut options: PgConnectOptions = match url.parse() {
            Ok(options) => options,
            Err(e) => {
                eprintln!("rsvp_store test setup: parse url failed: {e}");
                return None;
            }
        };
        options = options.options([("search_path", schema)]);
        let pool = match PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
        {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("rsvp_store test setup: schema connect failed: {e}");
                return None;
            }
        };
        // `raw_sql` runs the whole multi-statement script at once (the same
        // way `sqlx::migrate!` applies it in production); splitting on `;`
        // would break on semicolons inside header comments.
        let ddl = include_str!("../../cutover/migrations/0160_rsvp.sql");
        if let Err(e) = sqlx::raw_sql(ddl).execute(&pool).await {
            eprintln!("rsvp_store test setup: DDL failed: {e}");
            return None;
        }
        Some(pool)
    }

    /// Test-only schema names: lowercase identifier characters, so DDL
    /// interpolation cannot break out of the identifier position.
    fn is_test_schema_name(schema: &str) -> bool {
        !schema.is_empty()
            && schema.len() <= 40
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
        if !is_test_schema_name(schema) {
            return;
        }
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await;
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
        let Some(pool) = test_pool(schema).await else {
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
        drop_schema(&pool, schema).await;
    }

    #[tokio::test]
    async fn list_orders_by_response_time_then_user() {
        let schema = "tog_10083_rsvp_list";
        let Some(pool) = test_pool(schema).await else {
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
        drop_schema(&pool, schema).await;
    }

    #[tokio::test]
    async fn audit_appends_every_response() {
        let schema = "tog_10083_rsvp_audit";
        let Some(pool) = test_pool(schema).await else {
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
        drop_schema(&pool, schema).await;
    }

    #[tokio::test]
    async fn checkin_dedupes_and_ignores_rsvp_proof() {
        let schema = "tog_10083_rsvp_checkin";
        let Some(pool) = test_pool(schema).await else {
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
        drop_schema(&pool, schema).await;
    }
}
