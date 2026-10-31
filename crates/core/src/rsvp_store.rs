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
    AUDIT_RETENTION_DAYS, CHECKIN_AUDIT_ACTION, MAX_CHECKINS_PER_OCCURRENCE, MAX_RSVPS_PER_EVENT,
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
    #[error("rsvp required audit write failed: {0}")]
    AuditDb(sqlx::Error),
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
    let previous = upsert_rsvp_row(&mut tx, record).await?;
    tx.commit().await?;
    finish_transition(previous, record)
}

/// Upsert one RSVP row and commit its required audit row atomically: the
/// response upsert and the audit insert share one transaction, so a failed
/// audit insert rolls the response back with it. A first write then leaves
/// neither a new response nor a new audit row, and a failed rewrite
/// preserves the exact preceding response and audit history. The caller
/// builds `audit` from the same `record` via [`RsvpAudit::for_rsvp`], so the
/// audit carries this attempt's idempotency id and timestamp. Admission
/// refusals still precede any row write, and the audit insert runs after the
/// upsert, so the per-user rate ledger read is unchanged. A retried audit id
/// still dedupes (`ON CONFLICT DO NOTHING`) exactly as [`write_audit`] does.
/// An audit-step transport failure surfaces as [`RsvpStoreError::AuditDb`],
/// distinct from a response-step [`RsvpStoreError::Db`], so the router keeps
/// the existing audit refusal text for this path.
pub async fn put_rsvp_with_audit(
    pool: &Pool<Postgres>,
    record: &RsvpRecord,
    audit: &RsvpAudit,
) -> Result<RsvpTransition, RsvpStoreError> {
    let mut tx = pool.begin().await?;
    let previous = upsert_rsvp_row(&mut tx, record).await?;
    insert_audit_row(&mut *tx, audit)
        .await
        .map_err(RsvpStoreError::AuditDb)?;
    tx.commit().await?;
    finish_transition(previous, record)
}

/// The locked read-check-upsert half of an RSVP write, without the commit:
/// shared by [`put_rsvp`] and [`put_rsvp_with_audit`] so admission order,
/// lock order and row SQL stay identical on both paths.
async fn upsert_rsvp_row(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    record: &RsvpRecord,
) -> Result<Option<(String, sqlx::types::time::OffsetDateTime)>, RsvpStoreError> {
    // Coarser lock first, always in this order on every RSVP write path:
    // event, then user budget, then identity. The event lock serializes the
    // admission count below across distinct members. The user-budget lock
    // serializes the per-user rate check plus its audit-consuming write
    // across distinct events: the ledger below counts by (guild, user), so
    // same-user writers on different events must meet here or a single
    // remaining budget slot admits both. The identity lock then serializes
    // the read-modify-write on this member's row. Independent users take
    // distinct budget keys and never contend; per-event capacity still
    // serializes on the event key. The budget key carries a literal tag so
    // it can never alias the untagged 2- and 3-element keys (whose first
    // element is always a numeric guild snowflake); a hash collision only
    // causes extra contention, never an incorrect transition.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
            jsonb_build_array($1::text, $2::text)::text, 0))",
    )
    .bind(&record.guild_id)
    .bind(&record.event_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
            jsonb_build_array('rsvp-user-budget-v1', $1::text, $2::text)::text, 0))",
    )
    .bind(&record.guild_id)
    .bind(&record.user_id)
    .execute(&mut **tx)
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
    .execute(&mut **tx)
    .await?;
    let previous: Option<(String, sqlx::types::time::OffsetDateTime)> = sqlx::query_as(
        "SELECT status, responded_at FROM event_rsvps
         WHERE guild_id = $1 AND event_id = $2 AND user_id = $3 FOR UPDATE",
    )
    .bind(&record.guild_id)
    .bind(&record.event_id)
    .bind(&record.user_id)
    .fetch_optional(&mut **tx)
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
    .fetch_one(&mut **tx)
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
        .fetch_one(&mut **tx)
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
    .execute(&mut **tx)
    .await?;
    Ok(previous)
}

/// Report the transition for a committed RSVP write. The stored-status parse
/// stays after the commit, exactly as before: a row that bypassed the status
/// CHECK still surfaces as `UnknownStatus` without rolling the write back.
fn finish_transition(
    previous: Option<(String, sqlx::types::time::OffsetDateTime)>,
    record: &RsvpRecord,
) -> Result<RsvpTransition, RsvpStoreError> {
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
    Ok(insert_audit_row(pool, audit).await?)
}

/// The audit append shared by [`write_audit`] and [`put_rsvp_with_audit`]:
/// same row SQL and bindings on either a pool or an open transaction, so the
/// standalone and atomic paths can never drift apart.
pub(crate) async fn insert_audit_row<'e, E>(exec: E, audit: &RsvpAudit) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
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
    .fetch_optional(exec)
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
/// inserted, `false` when the idempotency key already held it (retry dedupe),
/// the proof is `rsvp` (legacy writes nothing for it), or the member opted
/// out (suppressed pairs drop the fact like every other community writer).
///
/// RA-03 admission (TOG-19773): inside one transaction the write is refused
/// with [`RsvpStoreError::OccurrenceAtCapacity`] when a new fact would grow
/// the occurrence past [`MAX_CHECKINS_PER_OCCURRENCE`] facts. A retry of an
/// already-recorded key still reports `false` (duplicate, not refusal) at a
/// full occurrence. The occurrence-scoped advisory lock keeps the
/// existence check and the admission count stable across concurrent
/// writers, so concurrent same-key writes still produce one logical effect.
///
/// Privacy gate, inside the write transaction: a suppressed `(guild, member)`
/// pair drops the fact. Attendance facts are lifecycle-like (no analytics
/// family), so suppression alone gates them, mirroring join/rules facts.
/// The shared table lock serializes with concurrent opt-outs (which take
/// SHARE ROW EXCLUSIVE over these tables): whoever locks first wins, so a
/// write that passes the gate can never land after the purge.
///
/// Callers that must audit the check-in use [`record_checkin_with_audit`]:
/// the fact and its audit row commit in one transaction, so a failed audit
/// insert rolls the fact back with it. This standalone path writes no audit
/// row.
pub async fn record_checkin(
    pool: &Pool<Postgres>,
    checkin: &CheckinWrite,
) -> Result<bool, RsvpStoreError> {
    let (inserted, _) = record_checkin_inner(pool, checkin, None).await?;
    Ok(inserted)
}

/// Record one verified-attendance fact with its required audit row, sharing
/// one transaction like [`put_rsvp_with_audit`]: the fact insert and the
/// audit insert commit together, so a failed audit insert rolls the fact
/// back and no partial write can survive this path. Returns
/// `(fact_inserted, audit_inserted)`: duplicates, `rsvp` proofs and
/// suppressed pairs report `(false, false)` with no audit write, and a
/// retried audit id dedupes (`ON CONFLICT DO NOTHING`) exactly as
/// [`write_audit`] does — the fact still records while `audit_inserted`
/// reports `false`, so the caller must handle that boolean explicitly
/// instead of discarding it. An audit-step transport failure surfaces as
/// [`RsvpStoreError::AuditDb`], distinct from a fact-step
/// [`RsvpStoreError::Db`].
pub async fn record_checkin_with_audit(
    pool: &Pool<Postgres>,
    checkin: &CheckinWrite,
    audit: &RsvpAudit,
) -> Result<(bool, bool), RsvpStoreError> {
    record_checkin_inner(pool, checkin, Some(audit)).await
}

/// The transactional check-in write shared by [`record_checkin`] and
/// [`record_checkin_with_audit`], so admission order, lock order, gate order
/// and row SQL stay identical on both paths. The audit insert runs only when
/// the fact insert lands, inside the same transaction.
async fn record_checkin_inner(
    pool: &Pool<Postgres>,
    checkin: &CheckinWrite,
    audit: Option<&RsvpAudit>,
) -> Result<(bool, bool), RsvpStoreError> {
    if !checkin.proof.writes_fact() {
        return Ok((false, false));
    }
    let key = checkin_idempotency_key(&checkin.event_occurrence_id, &checkin.member_id);
    let source = checkin_source(&checkin.event_occurrence_id);
    let mut tx = pool.begin().await?;
    // Serialize with concurrent opt-outs: lock order matches
    // `opt_out_member`'s sorted order, and ROW EXCLUSIVE never blocks other
    // writers.
    sqlx::query("LOCK TABLE community_facts, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if crate::privacy::suppressed_in(&mut tx, &checkin.guild_id, &checkin.member_id).await? {
        tx.rollback().await?;
        return Ok((false, false));
    }
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
        return Ok((false, false));
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
    let fact_inserted = inserted.is_some();
    // Only a landed fact earns its audit row, in the same transaction: a
    // duplicate, suppression or lost race writes no audit row, and a failed
    // audit insert rolls the fact back with it.
    let audit_inserted = match (fact_inserted, audit) {
        (true, Some(audit)) => insert_audit_row(&mut *tx, audit)
            .await
            .map_err(RsvpStoreError::AuditDb)?,
        _ => false,
    };
    tx.commit().await?;
    Ok((fact_inserted, audit_inserted))
}

/// RA-03 compensation (TOG-19773): undo exactly the RSVP write one attempt
/// made when post-write revalidation finds the event or membership gone.
/// A first response (`previous` is `None`) deletes the row this attempt
/// wrote; a re-response restores the member's exact prior row (status plus
/// timestamp) instead, so a refused rewrite never destroys the earlier
/// response that was live before this attempt.
///
/// The response cleanup is fenced to this attempt's write identity — its
/// audit row's `(outcome, created_at)` plus insertion commit order — never
/// `responded_at` alone and never timestamp order: a later-committed rewrite
/// (including an identical same-millisecond status rewrite, or an earlier
/// timestamp committed second) survives older compensation, while this
/// attempt's audit row — owned by its unique id — is always removed. When
/// the audit row is absent (idempotent retry or a write that never
/// ledgered), the fence falls back to the `responded_at` guard so the
/// exact-timestamp case still cleans up.
///
/// The response cleanup and the audit delete share one transaction under
/// the same identity lock writers take, so a midway database failure
/// rolls both back and a same-identity rewrite cannot land between the
/// fence check and the cleanup. Net effect on refusal when this call
/// succeeds: the table looks as if this attempt never ran. A failed call
/// leaves both pre-cleanup rows intact; the caller reports that as a
/// `compensation_failed` diagnostic.
pub async fn compensate_rsvp_write(
    pool: &Pool<Postgres>,
    guild_id: &str,
    event_id: &str,
    user_id: &str,
    responded_at: &str,
    audit_id: &str,
    previous: Option<(RsvpStatus, &str)>,
) -> Result<(), RsvpStoreError> {
    let mut tx = pool.begin().await?;
    // Serialize with same-identity writers and compensations. Writers take
    // event -> budget -> identity; taking only the identity key here cannot
    // deadlock (this path never waits for event/budget while holding it)
    // and still blocks a same-identity rewrite from landing mid-cleanup.
    // Tuple encoding matches the writer identity key exactly.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
            jsonb_build_array($1::text, $2::text, $3::text)::text, 0))",
    )
    .bind(guild_id)
    .bind(event_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    // This attempt's own audit row is the write-identity witness: its
    // outcome/created_at names what the attempt wrote, and its insertion
    // commit order decides which attempt is newer — timestamps never order
    // attempts because mint order can differ from commit order.
    let owned: Option<(String, sqlx::types::time::OffsetDateTime)> =
        sqlx::query_as("SELECT outcome, created_at FROM announcements_audit_log WHERE id = $1")
            .bind(audit_id)
            .fetch_optional(&mut *tx)
            .await?;
    let current: Option<(String, sqlx::types::time::OffsetDateTime)> = sqlx::query_as(
        "SELECT status, responded_at FROM event_rsvps
         WHERE guild_id = $1 AND event_id = $2 AND user_id = $3 FOR UPDATE",
    )
    .bind(guild_id)
    .bind(event_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let owned_response = match (&owned, &current) {
        (Some((outcome, created)), Some((status, responded))) => {
            // Values must still be this attempt's write, and no later
            // same-identity attempt may exist — otherwise the row belongs
            // to the newer attempt and survives this compensation.
            status == outcome
                && responded == created
                && !has_newer_rsvp_audit(&mut tx, guild_id, user_id, event_id, audit_id).await?
        }
        // No audit row to witness ownership: keep the legacy
        // responded_at-guarded cleanup so the exact-timestamp case still
        // removes its row (the guard below no-ops on mismatch).
        (None, Some(_)) => true,
        // No response row: nothing to clean, only the audit delete below.
        (_, None) => false,
    };
    if owned_response {
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
            .execute(&mut *tx)
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
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query("DELETE FROM announcements_audit_log WHERE id = $1")
        .bind(audit_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// SQL predicate "`newer` committed after `owned`", comparing 32-bit
/// transaction ids the way PostgreSQL itself does (`TransactionIdPrecedes`):
/// the forward distance modulo 2^32 must fall in `[1, 2^31)`. A plain `>`
/// on `xmin` inverts once the two ids straddle a transaction-id wraparound
/// (an older pre-wrap audit then looks numerically greater than a new
/// post-wrap attempt), so the fence must never compare `xmin` numerically.
/// Operands interpolate as SQL fragments (column expressions in the fence,
/// literals in the regression test) — never caller input.
fn xid_committed_after(newer: &str, owned: &str) -> String {
    format!("((({newer}) - ({owned}) + 4294967296) % 4294967296) BETWEEN 1 AND 2147483647")
}

/// True when a later same-identity RSVP attempt committed after `audit_id`,
/// by insertion commit order (`xmin`) alone. Timestamps never order
/// attempts: the handler mints `responded_at` before pool and lock
/// acquisition, so concurrent attempts can mint in one order and commit in
/// the other, and the store accepts decreasing timestamps. Same-user
/// writers serialize on the budget lock and same-identity writers on the
/// identity lock, so commit order matches write order here; the `xmin`
/// comparison is wraparound-aware (see [`xid_committed_after`]), so it
/// stays correct when transaction ids straddle a wraparound. Scoped to
/// `event.rsvp` rows so other slices sharing the log table never fence an
/// RSVP cleanup.
async fn has_newer_rsvp_audit(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    guild_id: &str,
    user_id: &str,
    event_id: &str,
    audit_id: &str,
) -> Result<bool, sqlx::Error> {
    // SAFETY: only the fixed predicate fragment interpolates; values bind.
    let newer_after_owned =
        xid_committed_after("newer.xmin::text::bigint", "owned.xmin::text::bigint");
    let sql = format!(
        "SELECT EXISTS(
           SELECT 1
           FROM announcements_audit_log AS newer,
                announcements_audit_log AS owned
           WHERE owned.id = $5
             AND newer.guild_id = $1
             AND newer.actor_id = $2
             AND newer.action = $3
             AND newer.target_key = $4
             AND newer.id <> owned.id
             AND {newer_after_owned}
         )",
    );
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(guild_id)
        .bind(user_id)
        .bind(RSVP_AUDIT_ACTION)
        .bind(event_id)
        .bind(audit_id)
        .fetch_one(&mut **tx)
        .await
}

/// RA-03 compensation (TOG-19773): remove the attendance fact one attempt
/// inserted when post-write revalidation finds a membership gone, plus the
/// audit row that attempt wrote — both deletes in one transaction, so either
/// both rows go or neither does. The fact delete keys on the exact
/// idempotency key, so only this identity's fact can match; `audit_id` is
/// `Some` only when this attempt inserted its audit row (a deduped audit id
/// belongs to the earlier attempt and must survive), so a refused attempt
/// leaves neither row it owns — like [`compensate_rsvp_write`], which always
/// removes its attempt's audit row too.
pub async fn compensate_checkin_write(
    pool: &Pool<Postgres>,
    idempotency_key: &str,
    audit_id: Option<&str>,
) -> Result<(), RsvpStoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM community_facts WHERE idempotency_key = $1")
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await?;
    if let Some(audit_id) = audit_id {
        sqlx::query("DELETE FROM announcements_audit_log WHERE id = $1")
            .bind(audit_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// RA-03 retention (TOG-19773): delete RSVP rows and this slice's own audit
/// rows strictly older than `cutoff_iso`, returning
/// `(rsvps_deleted, audits_deleted)`. The audit delete is scoped to
/// [`RSVP_AUDIT_ACTION`] and [`CHECKIN_AUDIT_ACTION`] rows: the log table is
/// shared with feed, LFG and recovery writers, and their evidence must
/// survive an RSVP purge. The cutoff must be older than the audit retention floor
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
         WHERE action IN ($2, $3) AND created_at < $1::timestamptz",
    )
    .bind(cutoff_iso)
    .bind(RSVP_AUDIT_ACTION)
    .bind(CHECKIN_AUDIT_ACTION)
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
        // `record_checkin` gates on the opt-out suppression table (same
        // shape as 0433, suppression only): the privacy migration itself is
        // not applied here so this fixture stays on the RSVP slice.
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS privacy_opt_outs (
               guild_id TEXT NOT NULL, member_id TEXT NOT NULL,
               opted_out_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
               PRIMARY KEY (guild_id, member_id))",
        )
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
        // Use a dedicated (guild, event) pair for this test. The event
        // admission lock is database-wide, not per-schema, so sharing the
        // fixture key with the other store tests lets their writers queue
        // behind the guard below in parallel runs and the waiter count never
        // reaches exactly two.
        let going = RsvpRecord {
            guild_id: "1545644954272137333".to_owned(),
            event_id: "1546451670500643333".to_owned(),
            ..rsvp(RsvpStatus::Going, &schema, "2026-09-10T10:00:00.000Z")
        };
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
        let first_record = going.clone();
        let first = tokio::spawn(async move { put_rsvp(&first_pool, &first_record).await });
        let second_pool = pool.clone();
        let second = tokio::spawn(async move { put_rsvp(&second_pool, &interested).await });
        // Both spawned writers must be polled onto the advisory lock before
        // the timeout. The guard above holds this test's dedicated
        // (guild, event) key, so only these two writers can queue here and a
        // short budget suffices. The bypass panic below still catches a
        // writer that never queues.
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
        let rows = list_rsvps(&pool, &going.guild_id, &going.event_id)
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

    /// Check-in audit parity: a recorded fact commits its audit row in the
    /// same transaction; a retried audit id dedupes while the fact still
    /// records; a duplicate writes neither; and a failed audit insert rolls
    /// the fact back, so no partial write survives.
    #[tokio::test]
    async fn checkin_with_audit_commits_fact_and_audit_atomically() {
        let Some((pool, schema)) = test_pool("checkin_audit")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let audit = |id: &str, member: &str| {
            RsvpAudit::for_checkin(
                id,
                "1545644954272137297",
                "host-1",
                "occ-audit",
                member,
                "2026-09-02T10:00:00.000Z",
            )
        };
        // Recorded: one fact, one audit row.
        assert_eq!(
            record_checkin_with_audit(
                &pool,
                &checkin("occ-audit", "m1", "2026-09-02T10:00:00.000Z"),
                &audit("checkin:1", "m1"),
            )
            .await
            .expect("recorded"),
            (true, true)
        );
        // Duplicate: neither a new fact nor a new audit row, even with a
        // fresh audit id.
        assert_eq!(
            record_checkin_with_audit(
                &pool,
                &checkin("occ-audit", "m1", "2026-09-02T10:01:00.000Z"),
                &audit("checkin:2", "m1"),
            )
            .await
            .expect("duplicate"),
            (false, false)
        );
        // Retried audit id: the new member's fact still records while the
        // audit dedupes — the caller must handle that `false` explicitly.
        assert_eq!(
            record_checkin_with_audit(
                &pool,
                &checkin("occ-audit", "m2", "2026-09-02T10:02:00.000Z"),
                &audit("checkin:1", "m2"),
            )
            .await
            .expect("audit retry"),
            (true, false)
        );
        let counts: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM community_facts),
                    (SELECT COUNT(*) FROM announcements_audit_log)",
        )
        .fetch_one(&pool)
        .await
        .expect("counts");
        assert_eq!(counts, (2, 1));
        // A failed audit insert rolls the fact back: dropping the audit
        // table stands in for an unavailable audit write.
        sqlx::query("DROP TABLE announcements_audit_log")
            .execute(&pool)
            .await
            .expect("drop audit table");
        assert!(matches!(
            record_checkin_with_audit(
                &pool,
                &checkin("occ-audit", "m3", "2026-09-02T10:03:00.000Z"),
                &audit("checkin:3", "m3"),
            )
            .await,
            Err(RsvpStoreError::AuditDb(_))
        ));
        let facts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_facts")
            .fetch_one(&pool)
            .await
            .expect("facts");
        assert_eq!(facts, 2, "refused audit write leaves no fact");
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
        // This slice's check-in audit rows purge with RSVP history.
        write_audit(
            &pool,
            &RsvpAudit::for_checkin(
                "audit-checkin-old",
                "1545644954272137297",
                "host-1",
                "occ-old",
                "old-member",
                "2020-01-01T00:00:00.000Z",
            ),
        )
        .await
        .expect("old check-in audit");
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
        assert_eq!(pruned, (1, 2));
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
        assert!(
            !audits.iter().any(|id| id == "audit-checkin-old"),
            "old check-in audit row purges with RSVP history"
        );
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
        compensate_checkin_write(
            &pool,
            &checkin_idempotency_key("occ-comp", "m1"),
            Some("audit-missing"),
        )
        .await
        .expect("compensate m1");
        let keys: Vec<String> = sqlx::query_scalar("SELECT idempotency_key FROM community_facts")
            .fetch_all(&pool)
            .await
            .expect("keys");
        assert_eq!(keys, [checkin_idempotency_key("occ-comp", "m2")]);
        drop_schema(&pool, &schema).await;
    }

    /// RA-03 compensation ownership: when this attempt's audit id deduped, it
    /// owns no audit row, so compensation removes the fact but keeps the
    /// earlier attempt's row (`None` deletes no audit row); when the attempt
    /// inserted its row, compensation removes both rows together.
    #[tokio::test]
    async fn compensation_keeps_unowned_audit_row() {
        let Some((pool, schema)) = test_pool("checkin_comp_own")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Earlier attempt's audit row, still owned by that attempt.
        let prior = RsvpAudit::for_checkin(
            "checkin:prior",
            "1545644954272137297",
            "host-1",
            "occ-own",
            "m1",
            "2026-09-02T10:00:00.000Z",
        );
        write_audit(&pool, &prior).await.expect("prior audit");
        // This attempt records its fact while the audit id dedupes.
        let (inserted, audit_inserted) = record_checkin_with_audit(
            &pool,
            &checkin("occ-own", "m1", "2026-09-02T10:00:00.000Z"),
            &prior,
        )
        .await
        .expect("deduped write");
        assert!(inserted, "fact records");
        assert!(!audit_inserted, "retried audit id dedupes");
        // Refusal compensates with no owned audit row.
        compensate_checkin_write(&pool, &checkin_idempotency_key("occ-own", "m1"), None)
            .await
            .expect("compensate unowned");
        // Owned audit row: compensation removes both rows together.
        let owned = RsvpAudit::for_checkin(
            "checkin:owned",
            "1545644954272137297",
            "host-1",
            "occ-own",
            "m2",
            "2026-09-02T10:00:00.000Z",
        );
        let (inserted, audit_inserted) = record_checkin_with_audit(
            &pool,
            &checkin("occ-own", "m2", "2026-09-02T10:00:00.000Z"),
            &owned,
        )
        .await
        .expect("owned write");
        assert!(inserted && audit_inserted, "fact and audit record");
        compensate_checkin_write(
            &pool,
            &checkin_idempotency_key("occ-own", "m2"),
            Some("checkin:owned"),
        )
        .await
        .expect("compensate owned");
        let facts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_facts")
            .fetch_one(&pool)
            .await
            .expect("facts");
        assert_eq!(facts, 0, "refused facts are removed");
        let audits: Vec<String> = sqlx::query_scalar("SELECT id FROM announcements_audit_log")
            .fetch_all(&pool)
            .await
            .expect("audit ids");
        assert_eq!(
            audits,
            ["checkin:prior".to_owned()],
            "earlier attempt's audit row survives"
        );
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

    /// Commit order, not timestamp order, decides which attempt is newer: the
    /// handler mints `responded_at` before lock acquisition, so a later
    /// timestamp can commit first and an earlier timestamp second. Compensating
    /// the last-committed attempt must restore its predecessor even though the
    /// predecessor's timestamp is greater.
    #[tokio::test]
    async fn compensation_restores_predecessor_when_commit_order_inverts_timestamps() {
        let Some((pool, schema)) = test_pool("rsvp_fence_order_inversion")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Later timestamp commits first ...
        let first = rsvp(RsvpStatus::Interested, "u1", "2026-09-10T10:00:00.500Z");
        put_rsvp(&pool, &first).await.expect("first");
        write_audit(&pool, &RsvpAudit::for_rsvp("audit-inversion-first", &first))
            .await
            .expect("audit");
        // ... then the earlier timestamp commits second and owns the row.
        let second = rsvp(RsvpStatus::Going, "u1", "2026-09-10T10:00:00.000Z");
        let transition = put_rsvp(&pool, &second).await.expect("second");
        write_audit(
            &pool,
            &RsvpAudit::for_rsvp("audit-inversion-second", &second),
        )
        .await
        .expect("audit");
        assert_eq!(
            transition.previous_response(),
            Some((RsvpStatus::Interested, "2026-09-10T10:00:00.500Z"))
        );
        compensate_rsvp_write(
            &pool,
            "1545644954272137297",
            "1546451670500642999",
            "u1",
            "2026-09-10T10:00:00.000Z",
            "audit-inversion-second",
            transition.previous_response(),
        )
        .await
        .expect("compensate inverted");
        let rows = list_rsvps(&pool, "1545644954272137297", "1546451670500642999")
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RsvpStatus::Interested);
        assert_eq!(rows[0].responded_at, "2026-09-10T10:00:00.500Z");
        let audits: Vec<String> = sqlx::query_scalar("SELECT id FROM announcements_audit_log")
            .fetch_all(&pool)
            .await
            .expect("audit ids");
        assert_eq!(audits, ["audit-inversion-first".to_owned()]);
        drop_schema(&pool, &schema).await;
    }

    /// The fence's commit-order predicate stays correct across transaction-id
    /// wraparound: a plain numeric `>` on `xmin` inverts once ids straddle
    /// the 2^32 boundary. Real `xmin` wraparound is not fabricable in a
    /// test, so this pins the exact shared predicate over boundary
    /// literals — including the wrap-straddling pairs a numeric comparison
    /// gets backwards.
    #[tokio::test]
    async fn fence_commit_order_survives_transaction_id_wraparound() {
        let Some((pool, schema)) = test_pool("rsvp_fence_xid_wraparound")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping rsvp_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // (newer xid, owned xid, newer committed after owned)
        let cases = [
            ("101", "100", true),
            ("100", "100", false),
            ("100", "101", false),
            // Post-wrap attempt after a pre-wrap audit: numerically
            // smaller, but committed later.
            ("5", "4294967295", true),
            // Pre-wrap audit against a post-wrap attempt: numerically
            // greater, but committed earlier — this is the inversion a
            // numeric `xmin` comparison gets wrong.
            ("4294967295", "5", false),
            // Frozen-era audit against a recent attempt.
            ("500000000", "2", true),
            // Modulo-2^32 boundary, matching TransactionIdPrecedes: a
            // forward distance of 2^31 - 1 is after, 2^31 is not.
            ("2147483648", "1", true),
            ("2147483649", "1", false),
        ];
        for (newer, owned, expected) in cases {
            // SAFETY: only numeric literals interpolate; the predicate is
            // the exact fragment the fence query uses.
            let sql = format!("SELECT {}", xid_committed_after(newer, owned));
            let actual: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .fetch_one(&pool)
                .await
                .expect("predicate probe");
            assert_eq!(
                actual, expected,
                "commit order of newer={newer} against owned={owned}"
            );
        }
        drop_schema(&pool, &schema).await;
    }
}
