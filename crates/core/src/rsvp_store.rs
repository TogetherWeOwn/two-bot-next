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
    AUDIT_RETENTION_DAYS, MAX_CHECKINS_PER_OCCURRENCE, MAX_RSVPS_PER_EVENT,
    MAX_RSVP_WRITES_PER_USER_PER_MINUTE, RSVP_AUDIT_ACTION,
};

/// RSVP store failure: transport plus the one domain parse a stored row can
/// still trigger (a `status` outside the legacy CHECK, e.g. written by hand).
/// Admission refusals (`EventAtCapacity`, `OccurrenceAtCapacity`,
/// `RsvpRateLimited`, `CutoffTooRecent`) are distinct from transport and
/// parse failures so the router can refuse with a specific reply instead of
/// a generic write error — and never report a refused write as saved.
#[derive(Debug, thiserror::Error)]
pub enum RsvpStoreError {
    #[error("rsvp store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("stored RSVP status {0:?} is not a known response")]
    UnknownStatus(String),
    #[error("event RSVP cap reached")]
    EventAtCapacity,
    #[error("occurrence check-in cap reached")]
    OccurrenceAtCapacity,
    #[error("per-user RSVP write rate exceeded")]
    RsvpRateLimited,
    #[error("retention cutoff is newer than the retention floor")]
    CutoffTooRecent,
}

/// Upsert one RSVP row and report the transition (legacy `putRsvp` blind
/// upsert, plus a locked read-back so going/interested/declined transitions
/// are observable). Last writer wins on the row; concurrent same-key writers
/// serialize on a transaction-scoped key lock even before the row exists,
/// so the reported `previous` is never torn.
/// Every call site also writes an audit row via [`write_audit`] — legacy
/// audits every response, including repeats.
///
/// RA-03 admission (TOG-19773): inside the same transaction the write is
/// refused when the member's per-minute write budget is spent
/// ([`MAX_RSVP_WRITES_PER_USER_PER_MINUTE`], ledgered in
/// `announcements_audit_log`) or when a first response would grow the event
/// past [`MAX_RSVPS_PER_EVENT`] rows. Repeat responses from a member who
/// already holds a row are updates, never admissions, so they stay allowed at
/// a full event (still subject to the per-user rate). Refusals happen before
/// any row write.
pub async fn put_rsvp(
    pool: &Pool<Postgres>,
    record: &RsvpRecord,
) -> Result<RsvpTransition, RsvpStoreError> {
    let mut tx = pool.begin().await?;
    // Coarser lock first, always in this order: the event lock serializes the
    // admission count below across distinct members, and the identity lock
    // then serializes the read-modify-write on this member's row. Arity
    // differs (2 vs 3 elements), so the two keys never alias by construction.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
            jsonb_build_array($1::text, $2::text)::text, 0))",
    )
    .bind(&record.guild_id)
    .bind(&record.event_id)
    .execute(&mut *tx)
    .await?;
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
    let previous: Option<(String, sqlx::types::time::OffsetDateTime)> = sqlx::query_as(
        "SELECT status, responded_at FROM event_rsvps
         WHERE guild_id = $1 AND event_id = $2 AND user_id = $3 FOR UPDATE",
    )
    .bind(&record.guild_id)
    .bind(&record.event_id)
    .bind(&record.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    // Per-user rate first (caller behavior), then the event admission cap.
    // The window buckets on the write's own timestamp, not the database
    // clock, so the bound is deterministic for a given audit trail. The range
    // form keeps the `idx_announcements_audit_guild_time` prefix usable
    // instead of scanning on a `date_trunc` expression.
    let recent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM announcements_audit_log
         WHERE guild_id = $1 AND actor_id = $2 AND action = $3
           AND created_at >= date_trunc('minute', $4::timestamptz)
           AND created_at < date_trunc('minute', $4::timestamptz) + interval '1 minute'",
    )
    .bind(&record.guild_id)
    .bind(&record.user_id)
    .bind(RSVP_AUDIT_ACTION)
    .bind(&record.responded_at)
    .fetch_one(&mut *tx)
    .await?;
    if recent >= MAX_RSVP_WRITES_PER_USER_PER_MINUTE {
        return Err(RsvpStoreError::RsvpRateLimited);
    }
    if previous.is_none() {
        let members: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM event_rsvps WHERE guild_id = $1 AND event_id = $2",
        )
        .bind(&record.guild_id)
        .bind(&record.event_id)
        .fetch_one(&mut *tx)
        .await?;
        if members >= MAX_RSVPS_PER_EVENT {
            return Err(RsvpStoreError::EventAtCapacity);
        }
    }
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
    let (previous, previous_responded_at) = previous
        .map(|(s, at)| {
            RsvpStatus::parse(&s)
                .map_err(|_| RsvpStoreError::UnknownStatus(s))
                .map(|status| (Some(status), Some(iso_millis(at))))
        })
        .transpose()?
        .unwrap_or((None, None));
    Ok(RsvpTransition {
        previous,
        previous_responded_at,
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
/// Returns `true` when the row was appended, `false` when the id already
/// held a row: retried submissions reuse the caller's idempotency id (the
/// checkin `ON CONFLICT DO NOTHING` pattern), so a transport retry dedupes
/// instead of failing on the primary key or doubling the audit trail.
pub async fn write_audit(pool: &Pool<Postgres>, audit: &RsvpAudit) -> Result<bool, RsvpStoreError> {
    let inserted: Option<(String,)> = sqlx::query_as(
        "INSERT INTO announcements_audit_log
           (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz)
         ON CONFLICT (id) DO NOTHING RETURNING id",
    )
    .bind(&audit.id)
    .bind(&audit.guild_id)
    .bind(&audit.actor_id)
    .bind(&audit.action)
    .bind(&audit.target_key)
    .bind(&audit.outcome)
    .bind(&audit.reason)
    .bind(&audit.created_at)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
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
///
/// RA-03 admission (TOG-19773): inside one transaction the write is refused
/// with [`RsvpStoreError::OccurrenceAtCapacity`] when a new fact would grow
/// the occurrence past [`MAX_CHECKINS_PER_OCCURRENCE`] facts. A retry of an
/// already-recorded key still reports `false` (duplicate, not refusal) at a
/// full occurrence. The occurrence-scoped advisory lock keeps the
/// existence check and the admission count stable across concurrent
/// writers, so concurrent same-key writes still produce one logical effect.
pub async fn record_checkin(
    pool: &Pool<Postgres>,
    checkin: &CheckinWrite,
) -> Result<bool, RsvpStoreError> {
    if !checkin.proof.writes_fact() {
        return Ok(false);
    }
    let key = checkin_idempotency_key(&checkin.event_occurrence_id, &checkin.member_id);
    let source = checkin_source(&checkin.event_occurrence_id);
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
            jsonb_build_array($1::text, $2::text)::text, 0))",
    )
    .bind(&checkin.guild_id)
    .bind(&checkin.event_occurrence_id)
    .execute(&mut *tx)
    .await?;
    let duplicate: Option<i64> =
        sqlx::query_scalar("SELECT id FROM community_facts WHERE idempotency_key = $1")
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
    if duplicate.is_some() {
        return Ok(false);
    }
    let facts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM community_facts WHERE guild_id = $1 AND source = $2",
    )
    .bind(&checkin.guild_id)
    .bind(&source)
    .fetch_one(&mut *tx)
    .await?;
    if facts >= MAX_CHECKINS_PER_OCCURRENCE {
        return Err(RsvpStoreError::OccurrenceAtCapacity);
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
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(inserted.is_some())
}

/// RA-03 compensation (TOG-19773): undo exactly the RSVP write one attempt
/// made when post-write revalidation finds the event or membership gone.
/// A first response (`previous` is `None`) deletes the row this attempt
/// wrote; a re-response restores the member's exact prior row (status plus
/// timestamp) instead, so a refused rewrite never destroys the earlier
/// response that was live before this attempt. Both paths are guarded by
/// this attempt's `responded_at`: a newer concurrent rewrite of the same
/// member's row survives (the newer attempt delivers its own reply), while
/// this attempt's audit row — owned by its unique id — is always removed.
/// Net effect on refusal: the table looks as if this attempt never ran.
pub async fn compensate_rsvp_write(
    pool: &Pool<Postgres>,
    guild_id: &str,
    event_id: &str,
    user_id: &str,
    responded_at: &str,
    audit_id: &str,
    previous: Option<(RsvpStatus, &str)>,
) -> Result<(), RsvpStoreError> {
    if let Some((status, previous_responded_at)) = previous {
        sqlx::query(
            "UPDATE event_rsvps SET status = $4, responded_at = $5::timestamptz
             WHERE guild_id = $1 AND event_id = $2 AND user_id = $3
               AND responded_at = $6::timestamptz",
        )
        .bind(guild_id)
        .bind(event_id)
        .bind(user_id)
        .bind(status.as_str())
        .bind(previous_responded_at)
        .bind(responded_at)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "DELETE FROM event_rsvps
             WHERE guild_id = $1 AND event_id = $2 AND user_id = $3
               AND responded_at = $4::timestamptz",
        )
        .bind(guild_id)
        .bind(event_id)
        .bind(user_id)
        .bind(responded_at)
        .execute(pool)
        .await?;
    }
    sqlx::query("DELETE FROM announcements_audit_log WHERE id = $1")
        .bind(audit_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// RA-03 compensation (TOG-19773): remove the attendance fact one attempt
/// inserted when post-write revalidation finds a membership gone. The delete
/// keys on the exact idempotency key, so only this identity's fact can match.
pub async fn compensate_checkin_write(
    pool: &Pool<Postgres>,
    idempotency_key: &str,
) -> Result<(), RsvpStoreError> {
    sqlx::query("DELETE FROM community_facts WHERE idempotency_key = $1")
        .bind(idempotency_key)
        .execute(pool)
        .await?;
    Ok(())
}

/// RA-03 retention (TOG-19773): delete RSVP rows and this slice's own audit
/// rows strictly older than `cutoff_iso`, returning
/// `(rsvps_deleted, audits_deleted)`. The audit delete is scoped to
/// [`RSVP_AUDIT_ACTION`] rows: the log table is shared with feed, LFG and
/// recovery writers, and their evidence must survive an RSVP purge. The
/// cutoff must be older than the audit retention floor
/// ([`AUDIT_RETENTION_DAYS`], the stricter horizon, which also covers the
/// RSVP floor); a newer cutoff is refused with
/// [`RsvpStoreError::CutoffTooRecent`] before any delete, so required
/// recovery and audit evidence is never removed. A malformed cutoff fails as
/// a database error — never as a partial purge.
pub async fn prune_rsvp_history(
    pool: &Pool<Postgres>,
    cutoff_iso: &str,
) -> Result<(u64, u64), RsvpStoreError> {
    let allowed: bool =
        sqlx::query_scalar("SELECT $1::timestamptz <= now() - make_interval(days => $2)")
            .bind(cutoff_iso)
            .bind(AUDIT_RETENTION_DAYS as i32)
            .fetch_one(pool)
            .await?;
    if !allowed {
        return Err(RsvpStoreError::CutoffTooRecent);
    }
    let rsvps = sqlx::query("DELETE FROM event_rsvps WHERE responded_at < $1::timestamptz")
        .bind(cutoff_iso)
        .execute(pool)
        .await?
        .rows_affected();
    let audits = sqlx::query(
        "DELETE FROM announcements_audit_log
         WHERE action = $2 AND created_at < $1::timestamptz",
    )
    .bind(cutoff_iso)
    .bind(RSVP_AUDIT_ACTION)
    .execute(pool)
    .await?
    .rows_affected();
    Ok((rsvps, audits))
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

    fn checkin(occurrence: &str, member: &str, at: &str) -> CheckinWrite {
        let human = checkin_classification(false);
        CheckinWrite {
            guild_id: "1545644954272137297".to_owned(),
            event_occurrence_id: occurrence.to_owned(),
            member_id: member.to_owned(),
            occurred_at: at.to_owned(),
            proof: AttendanceProof::HostCheckin,
            classifier_version: "community-test-v1".to_owned(),
            classification: human.classification.to_owned(),
            matched_rule: human.matched_rule.to_owned(),
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
        assert_eq!(first.previous_response(), None);
        let moved = put_rsvp(
            &pool,
            &rsvp(RsvpStatus::Interested, "u1", "2026-09-10T10:01:00.000Z"),
        )
        .await
        .expect("second write");
        assert_eq!(moved.previous, Some(RsvpStatus::Going));
        assert_eq!(
            moved.previous_response(),
            Some((RsvpStatus::Going, "2026-09-10T10:00:00.000Z"))
        );
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
        // Hold the event admission lock before either writer starts. put_rsvp
        // takes the coarser event lock first, so both writers queue here and
        // then serialize through the identity lock after release. Holding
        // only the identity lock strands the second writer on the event lock
        // instead, and the waiter count below never reaches two.
        let mut guard = pool.begin().await.expect("guard transaction");
        let guard_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *guard)
            .await
            .expect("guard pid");
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended(
                jsonb_build_array($1::text, $2::text)::text, 0))",
        )
        .bind(&going.guild_id)
        .bind(&going.event_id)
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
        assert!(
            write_audit(&pool, &RsvpAudit::for_rsvp("audit-1", &record))
                .await
                .expect("audit 1"),
            "first audit id appends"
        );
        assert!(
            write_audit(&pool, &RsvpAudit::for_rsvp("audit-2", &record))
                .await
                .expect("audit 2"),
            "distinct audit id appends"
        );
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

    /// RA-03 burst: rapid repeats from one member collapse onto their single
    /// row (last writer wins) instead of growing storage. Writes spread over
    /// three UTC minutes stay inside every per-minute budget; `put_rsvp`
    /// reads the audit ledger it does not itself write, so these ledger-free
    /// puts never trip the rate gate — the next test covers the ledgered path.
    #[tokio::test]
    async fn burst_same_user_rewrites_collapse_to_one_row() {
        let Some((pool, schema)) = test_pool("rsvp_burst_same")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        for i in 0..25 {
            let at = format!("2026-09-10T{:02}:00:{:02}.000Z", 10 + i / 10, i % 10);
            put_rsvp(&pool, &rsvp(RsvpStatus::Going, "u1", &at))
                .await
                .expect("burst write");
        }
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].responded_at, "2026-09-10T12:00:04.000Z");
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 per-user rate: the audit ledger admits a full minute budget,
    /// then the next write in the same minute is refused before any row
    /// write; a new minute re-admits the same member.
    #[tokio::test]
    async fn per_user_rate_refuses_burst_inside_one_minute() {
        let Some((pool, schema)) = test_pool("rsvp_rate").await.expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        for i in 0..MAX_RSVP_WRITES_PER_USER_PER_MINUTE {
            let at = format!("2026-09-10T10:00:{i:02}.000Z");
            let record = rsvp(RsvpStatus::Going, "u1", &at);
            put_rsvp(&pool, &record).await.expect("budgeted write");
            assert!(
                write_audit(&pool, &RsvpAudit::for_rsvp(&format!("rate-{i}"), &record))
                    .await
                    .expect("ledger"),
                "each budgeted write ledgers one audit row"
            );
        }
        let over = rsvp(RsvpStatus::Interested, "u1", "2026-09-10T10:00:59.000Z");
        assert!(matches!(
            put_rsvp(&pool, &over).await,
            Err(RsvpStoreError::RsvpRateLimited)
        ));
        // Refused before any row write: the stored response is still the last
        // budgeted write's.
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RsvpStatus::Going);
        assert_eq!(rows[0].responded_at, "2026-09-10T10:00:19.000Z");
        let next = rsvp(RsvpStatus::Interested, "u1", "2026-09-10T10:01:00.000Z");
        let moved = put_rsvp(&pool, &next).await.expect("next minute");
        assert_eq!(moved.previous, Some(RsvpStatus::Going));
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 event cap: the event admits exactly `MAX_RSVPS_PER_EVENT`
    /// members; the next new member is refused, while a member who already
    /// holds a row can still move (updates are not admissions).
    #[tokio::test]
    async fn event_cap_refuses_new_members_but_keeps_updates() {
        let Some((pool, schema)) = test_pool("rsvp_cap").await.expect("test database setup") else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        for i in 0..MAX_RSVPS_PER_EVENT {
            let at = format!("2026-09-10T10:{:02}:{:02}.000Z", i / 60, i % 60);
            let user = format!("cap-user-{i}");
            put_rsvp(&pool, &rsvp(RsvpStatus::Going, &user, &at))
                .await
                .expect("admitted");
        }
        let full = rsvp(
            RsvpStatus::Going,
            "cap-user-new",
            "2026-09-10T11:00:00.000Z",
        );
        assert!(matches!(
            put_rsvp(&pool, &full).await,
            Err(RsvpStoreError::EventAtCapacity)
        ));
        let moved = rsvp(
            RsvpStatus::Declined,
            "cap-user-0",
            "2026-09-10T11:00:01.000Z",
        );
        let transition = put_rsvp(&pool, &moved).await.expect("update at capacity");
        assert_eq!(transition.previous, Some(RsvpStatus::Going));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_rsvps")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, MAX_RSVPS_PER_EVENT);
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 occurrence bound: the occurrence admits exactly
    /// `MAX_CHECKINS_PER_OCCURRENCE` facts; a retry of a recorded key still
    /// reports duplicate (not refusal) at capacity, a new member is refused,
    /// and a distinct occurrence is unaffected.
    #[tokio::test]
    async fn checkin_burst_caps_facts_and_dedupes_retries() {
        let Some((pool, schema)) = test_pool("checkin_cap").await.expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        for i in 0..MAX_CHECKINS_PER_OCCURRENCE {
            let member = format!("member-{i}");
            assert!(
                record_checkin(
                    &pool,
                    &checkin("occ-cap", &member, "2026-09-02T10:00:00.000Z")
                )
                .await
                .expect("admitted"),
                "member {i} admitted"
            );
        }
        assert!(
            !record_checkin(
                &pool,
                &checkin("occ-cap", "member-0", "2026-09-02T10:01:00.000Z")
            )
            .await
            .expect("duplicate"),
            "recorded key retries duplicate at capacity"
        );
        assert!(matches!(
            record_checkin(
                &pool,
                &checkin("occ-cap", "member-new", "2026-09-02T10:02:00.000Z")
            )
            .await,
            Err(RsvpStoreError::OccurrenceAtCapacity)
        ));
        assert!(
            record_checkin(
                &pool,
                &checkin("occ-other", "member-new", "2026-09-02T10:02:00.000Z")
            )
            .await
            .expect("other occurrence"),
            "distinct occurrences have independent bounds"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_facts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, MAX_CHECKINS_PER_OCCURRENCE + 1);
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 race: two concurrent writers on the same check-in identity
    /// produce one logical effect — exactly one insert, one duplicate.
    #[tokio::test]
    async fn concurrent_same_key_checkins_record_once() {
        let Some((pool, schema)) = test_pool("checkin_race")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let first = checkin("occ-race", "racer", "2026-09-02T10:00:00.000Z");
        let second = first.clone();
        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let (a, b) = tokio::join!(
            tokio::spawn(async move { record_checkin(&pool_a, &first).await }),
            tokio::spawn(async move { record_checkin(&pool_b, &second).await }),
        );
        let (a, b) = (
            a.expect("first task").expect("first write"),
            b.expect("second task").expect("second write"),
        );
        assert_ne!(a, b, "exactly one concurrent same-key write inserts");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_facts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1);
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 retry dedupe: a repeated audit id appends nothing and reports
    /// `false`, so a transport retry of one submission cannot double the
    /// audit trail or fail on the primary key.
    #[tokio::test]
    async fn audit_id_dedupes_retries() {
        let Some((pool, schema)) = test_pool("rsvp_audit_idem")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let record = rsvp(RsvpStatus::Going, "u1", "2026-09-10T10:00:00.000Z");
        put_rsvp(&pool, &record).await.expect("write");
        let audit = RsvpAudit::for_rsvp("audit-retry", &record);
        assert!(
            write_audit(&pool, &audit).await.expect("first"),
            "first audit id appends"
        );
        assert!(
            !write_audit(&pool, &audit).await.expect("retry"),
            "retried audit id dedupes"
        );
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM announcements_audit_log")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count.0, 1);
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 retention: history older than the cutoff is purged from both
    /// tables while floored rows survive; a cutoff newer than the audit
    /// floor is refused before any delete, and a malformed cutoff fails as
    /// a database error rather than a partial purge. Audit rows from other
    /// slices sharing the log table always survive the purge.
    #[tokio::test]
    async fn retention_prune_keeps_floor_and_clears_history() {
        let Some((pool, schema)) = test_pool("rsvp_retain").await.expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // The database clock defines "recent": formatting it through the
        // store's own canonicalizer keeps the test rot-proof as wall time
        // passes the fixed 2020 history below.
        let now: sqlx::types::time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
            .fetch_one(&pool)
            .await
            .expect("now");
        let recent = super::iso_millis(now);
        let old = rsvp(RsvpStatus::Going, "old-user", "2020-01-01T00:00:00.000Z");
        put_rsvp(&pool, &old).await.expect("old write");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-old", &old))
            .await
            .expect("old audit");
        let fresh = rsvp(RsvpStatus::Interested, "new-user", &recent);
        put_rsvp(&pool, &fresh).await.expect("recent write");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-new", &fresh))
            .await
            .expect("recent audit");
        // Another slice's audit evidence shares the log table: the purge
        // must leave it alone even when it predates the cutoff.
        write_audit(
            &pool,
            &RsvpAudit {
                id: "audit-foreign".to_owned(),
                guild_id: "1545644954272137297".to_owned(),
                actor_id: Some("other-user".to_owned()),
                action: "feed.relay".to_owned(),
                target_key: None,
                outcome: "delivered".to_owned(),
                reason: None,
                created_at: "2020-01-01T00:00:00.000Z".to_owned(),
            },
        )
        .await
        .expect("foreign audit");
        let pruned = prune_rsvp_history(&pool, "2021-06-01T00:00:00.000Z")
            .await
            .expect("history purge");
        assert_eq!(pruned, (1, 1));
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_id, "new-user");
        let audits: Vec<String> =
            sqlx::query_scalar("SELECT id FROM announcements_audit_log ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("audit ids");
        assert_eq!(audits, ["audit-foreign".to_owned(), "audit-new".to_owned()]);
        assert_eq!(
            prune_rsvp_history(&pool, "2021-06-01T00:00:00.000Z")
                .await
                .expect("repeat purge"),
            (0, 0),
            "purging twice deletes nothing new"
        );
        assert!(matches!(
            prune_rsvp_history(&pool, &recent).await,
            Err(RsvpStoreError::CutoffTooRecent)
        ));
        assert!(matches!(
            prune_rsvp_history(&pool, "not-a-timestamp").await,
            Err(RsvpStoreError::Db(_))
        ));
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1, "refused purges write nothing");
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 compensation: undoing a raced first response deletes exactly
    /// this attempt's rows — a newer concurrent rewrite of the same member's
    /// row survives (it delivers its own reply), while the attempt's own
    /// audit row is always removed. Check-in compensation keys on the exact
    /// idempotency key.
    #[tokio::test]
    async fn compensation_removes_only_the_raced_write() {
        let Some((pool, schema)) = test_pool("rsvp_compensate")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let first = rsvp(RsvpStatus::Going, "u1", "2026-09-10T10:00:00.000Z");
        put_rsvp(&pool, &first).await.expect("first");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-first", &first))
            .await
            .expect("audit");
        // A newer rewrite lands before compensation runs.
        let second = rsvp(RsvpStatus::Interested, "u1", "2026-09-10T10:01:00.000Z");
        put_rsvp(&pool, &second).await.expect("second");
        compensate_rsvp_write(
            &pool,
            "1545644954272137297",
            "1546451670500642999",
            "u1",
            "2026-09-10T10:00:00.000Z",
            "audit-first",
            None,
        )
        .await
        .expect("compensate");
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RsvpStatus::Interested);
        let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM announcements_audit_log")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(audits, 0);
        // The exact-timestamp case removes the row itself.
        compensate_rsvp_write(
            &pool,
            "1545644954272137297",
            "1546451670500642999",
            "u1",
            "2026-09-10T10:01:00.000Z",
            "audit-missing",
            None,
        )
        .await
        .expect("compensate exact");
        assert!(
            list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
                .await
                .expect("list")
                .is_empty()
        );
        assert!(
            record_checkin(
                &pool,
                &checkin("occ-comp", "m1", "2026-09-02T10:00:00.000Z")
            )
            .await
            .expect("m1"),
            "m1 records"
        );
        assert!(
            record_checkin(
                &pool,
                &checkin("occ-comp", "m2", "2026-09-02T10:00:00.000Z")
            )
            .await
            .expect("m2"),
            "m2 records"
        );
        compensate_checkin_write(&pool, &checkin_idempotency_key("occ-comp", "m1"))
            .await
            .expect("compensate m1");
        let keys: Vec<String> = sqlx::query_scalar("SELECT idempotency_key FROM community_facts")
            .fetch_all(&pool)
            .await
            .expect("keys");
        assert_eq!(keys, [checkin_idempotency_key("occ-comp", "m2")]);
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 compensation: a refused re-response restores the member's exact
    /// prior row (status plus timestamp) instead of deleting it, while the
    /// refused attempt's audit row is still removed. A newer concurrent
    /// rewrite keeps the guarded restore from touching it.
    #[tokio::test]
    async fn compensation_restores_previous_response_on_refused_rewrite() {
        let Some((pool, schema)) = test_pool("rsvp_compensate_restore")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let first = rsvp(RsvpStatus::Going, "u1", "2026-09-10T10:00:00.000Z");
        put_rsvp(&pool, &first).await.expect("first");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-restore-first", &first))
            .await
            .expect("audit");
        let second = rsvp(RsvpStatus::Interested, "u1", "2026-09-10T10:01:00.000Z");
        let transition = put_rsvp(&pool, &second).await.expect("rewrite");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-restore-second", &second))
            .await
            .expect("audit");
        // The fence refuses the rewrite (event or membership gone): the
        // member's earlier `going` row must survive the compensation.
        compensate_rsvp_write(
            &pool,
            "1545644954272137297",
            "1546451670500642999",
            "u1",
            "2026-09-10T10:01:00.000Z",
            "audit-restore-second",
            transition.previous_response(),
        )
        .await
        .expect("compensate rewrite");
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RsvpStatus::Going);
        assert_eq!(rows[0].responded_at, "2026-09-10T10:00:00.000Z");
        let audits: Vec<String> = sqlx::query_scalar("SELECT id FROM announcements_audit_log")
            .fetch_all(&pool)
            .await
            .expect("audit ids");
        assert_eq!(audits, ["audit-restore-first".to_owned()]);
        // A newer rewrite landing after the refused attempt is untouched by
        // a repeat of that attempt's compensation.
        let third = rsvp(RsvpStatus::Declined, "u1", "2026-09-10T10:02:00.000Z");
        put_rsvp(&pool, &third).await.expect("newer rewrite");
        compensate_rsvp_write(
            &pool,
            "1545644954272137297",
            "1546451670500642999",
            "u1",
            "2026-09-10T10:01:00.000Z",
            "audit-restore-second",
            transition.previous_response(),
        )
        .await
        .expect("stale compensate");
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RsvpStatus::Declined);
        assert_eq!(rows[0].responded_at, "2026-09-10T10:02:00.000Z");
        drop_schema(&pool, &schema).await;
    }
}
