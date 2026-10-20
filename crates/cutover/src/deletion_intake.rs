//! Authenticated member deletion intake + durable requester receipts.
//!
//! Thin S4 adapter over the existing retention ledger and scheduler: the
//! published `/privacy delete` member entrypoint accepts an explicit deletion
//! request and persists it through the same
//! `retention_deletion_requests` rows [`crate::retention::request_user_deletion`]
//! writes — never the narrower opt-out purge — and the existing
//! [`crate::retention::run_retention`] scheduler drain settles the receipt.
//! No new deletion engine, no new scheduler, no new HTTP route, no new table.
//!
//! ## Reuse boundary (adjacent work, not re-implemented here)
//!
//! Member opt-out suppression/purge semantics belong to [`crate::privacy`];
//! guild removal/rejoin, runtime scheduling and retry signals belong to the
//! runtime slice behind [`crate::retention::handle_guild_delete`] and
//! [`crate::retention::run_retention`]; policy publication belongs to the web
//! surface. This module only binds the authenticated guild/requester pair to
//! the durable request API and exposes token-free status/receipt reads.
//!
//! ## Coverage limits (do not silently claim more)
//!
//! * The request scope names the guild/member footprint only — every
//!   erasure-plan table's guild/member rows — never global erasure. Retained
//!   control-plane exceptions stay retained and are disclosed on the receipt.
//! * The live `/privacy` delete, deletion-status and deletion-receipt handlers
//!   bind every intake call to the interaction's guild and authenticated actor;
//!   the request id is the only caller-supplied argument on reads.
//! * Voice-raw coverage is pinned to voice-session `events` rows, matching the
//!   retention module's documented limits.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use two_bot_core::CommandDefinition;

use super::retention::DELETION_SLA_HOURS;

/// Acknowledgement for a deletion request. Fresh requests are pending only;
/// an interaction retry can report `completed` if the scheduler drained since
/// the original request. Completion details are observed through
/// [`deletion_status_for_requester`] / [`deletion_receipt_for_requester`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionAck {
    /// The durable request id (deterministic per guild/requester/interaction,
    /// so an interaction retry returns the same request).
    pub request_id: String,
    /// Always `"pending"` for a freshly accepted request.
    pub status: String,
}

/// Requester-authorized view of one deletion request. Readable without any
/// bearer token, so it stays usable after the interaction token expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionStatus {
    /// The durable request id.
    pub request_id: String,
    /// Owning guild scope.
    pub guild_id: String,
    /// Owning member scope (the authenticated requester).
    pub user_id: String,
    /// `"pending"` or `"completed"`.
    pub status: String,
    /// Request time, seconds since the Unix epoch.
    pub requested_at_unix: i64,
    /// Completion time, seconds since the Unix epoch (`None` while pending).
    pub completed_at_unix: Option<i64>,
    /// True while pending past the under-72-hour service objective.
    pub overdue: bool,
}

/// Requester-authorized completion receipt. Identifies request, scope and both
/// timestamps plus the retained limits, without leaking another member's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionReceiptView {
    /// The settled request id.
    pub request_id: String,
    /// The receipt artifact id.
    pub receipt_id: String,
    /// Owning guild scope.
    pub guild_id: String,
    /// Owning member scope (the authenticated requester).
    pub user_id: String,
    /// Request time, seconds since the Unix epoch.
    pub requested_at_unix: i64,
    /// Completion time, seconds since the Unix epoch.
    pub completed_at_unix: i64,
    /// Guild/member footprint, never a global-erasure claim.
    pub scope: String,
    /// Disclosed control-plane exceptions, never empty.
    pub retained_limits: String,
}

/// Upper bound on outstanding pending requests per guild/member pair. A retry
/// with the same interaction id returns the existing request and never counts
/// against this bound; only distinct new requests do.
const MAX_PENDING_PER_PAIR: i64 = 5;

/// Longest accepted request/interaction id, matching the ledger's own bound.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Static disclosure of the retained control-plane exceptions. Constant text:
/// it names scopes and tables, never a member id.
const RETAINED_LIMITS: &str = "Retained control-plane exceptions: privacy_opt_outs suppression \
    tombstone; privacy_opt_out_audit (member_id, actor) purge receipts; \
    retention_deletion_requests request/receipt ledger; operator-released \
    moderation_idempotency replay fence and its executions; guild_settings, \
    guild_settings_audit, audit_kill_switch, member_erasure_audit; moderation, \
    audit, ticket, voice-room and settings rows outside the member footprint. \
    This receipt covers the guild/member footprint only.";

fn invalid_request(reason: &str) -> sqlx::Error {
    sqlx::Error::InvalidArgument(reason.into())
}

fn validate_identity(value: &str) -> Result<(), sqlx::Error> {
    if !crate::is_snowflake(value) || value.parse::<u64>().map_or(true, |id| id == 0) {
        return Err(invalid_request("invalid retention identity"));
    }
    Ok(())
}

fn validate_token_free_id(value: &str, what: &str) -> Result<(), sqlx::Error> {
    if value.trim().is_empty() || value.len() > MAX_REQUEST_ID_LEN {
        return Err(invalid_request(what));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(invalid_request(what));
    }
    Ok(())
}

/// Deterministic request id for one guild/requester/interaction triple, so an
/// interaction retry returns the same request with its current status without
/// queueing a second destructive effect. The interaction id is the idempotency key: it is
/// validated but never stored (the durable ledger holds scope only, no bearer
/// tokens), and the hash binds it to the authenticated pair so one member's
/// retry can never resolve to another member's request.
fn deterministic_request_id(guild_id: &str, requester_id: &str, interaction_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"two-bot deletion-intake v1");
    hasher.update([0]);
    hasher.update(guild_id.as_bytes());
    hasher.update([0]);
    hasher.update(requester_id.as_bytes());
    hasher.update([0]);
    hasher.update(interaction_id.as_bytes());
    format!("del-{}", hex::encode(hasher.finalize()))
}

/// Accept an explicit member deletion request from the authenticated
/// guild/requester pair and persist it to the durable request ledger (the same
/// rows [`crate::retention::request_user_deletion`] writes), never the
/// narrower opt-out purge. Fresh requests are acknowledged as pending. A retry
/// with the same interaction id returns the same request's current status
/// without queueing a duplicate destructive effect. Identities and the
/// interaction id are refused before any write.
pub async fn request_member_deletion(
    pool: &PgPool,
    guild_id: &str,
    requester_id: &str,
    interaction_id: &str,
) -> Result<DeletionAck, sqlx::Error> {
    validate_identity(guild_id)?;
    validate_identity(requester_id)?;
    validate_token_free_id(interaction_id, "invalid deletion interaction")?;
    let request_id = deterministic_request_id(guild_id, requester_id, interaction_id);

    // Fast path: an interaction retry returns the same request without taking
    // the per-pair lock.
    let existing: Option<(String, String, String)> = sqlx::query_as(
        "SELECT guild_id, user_id, status FROM retention_deletion_requests WHERE request_id = $1",
    )
    .bind(&request_id)
    .fetch_optional(pool)
    .await?;
    if let Some((owner_guild, owner_user, status)) = existing {
        if owner_guild != guild_id || owner_user != requester_id {
            return Err(invalid_request("unknown or settled deletion request"));
        }
        return Ok(DeletionAck { request_id, status });
    }

    // Serialize the pending-cap check with the insert under a per-pair
    // transaction-scoped advisory lock. Without this, concurrent requests with
    // distinct interaction ids all observe a count below the cap and all
    // insert. The lock key is namespaced to this intake so it never collides
    // with other per-pair locks.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{guild_id}:{requester_id}:deletion-intake"))
        .execute(&mut *tx)
        .await?;

    // Re-check inside the lock: a concurrent retry may have inserted the same
    // request id between the fast path and the lock acquisition.
    let existing: Option<(String, String, String)> = sqlx::query_as(
        "SELECT guild_id, user_id, status FROM retention_deletion_requests WHERE request_id = $1",
    )
    .bind(&request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((owner_guild, owner_user, status)) = existing {
        if owner_guild != guild_id || owner_user != requester_id {
            return Err(invalid_request("unknown or settled deletion request"));
        }
        tx.commit().await?;
        return Ok(DeletionAck { request_id, status });
    }

    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests \
         WHERE guild_id = $1 AND user_id = $2 AND status = 'pending'",
    )
    .bind(guild_id)
    .bind(requester_id)
    .fetch_one(&mut *tx)
    .await?;
    if pending >= MAX_PENDING_PER_PAIR {
        return Err(invalid_request("deletion request rate limited"));
    }

    sqlx::query(
        "INSERT INTO retention_deletion_requests (request_id, guild_id, user_id) \
         VALUES ($1, $2, $3) ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(&request_id)
    .bind(guild_id)
    .bind(requester_id)
    .execute(&mut *tx)
    .await?;
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM retention_deletion_requests \
         WHERE request_id = $1 AND guild_id = $2 AND user_id = $3",
    )
    .bind(&request_id)
    .bind(guild_id)
    .bind(requester_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(status) = status else {
        return Err(invalid_request("unknown or settled deletion request"));
    };
    tx.commit().await?;
    Ok(DeletionAck { request_id, status })
}

struct LedgerRow {
    guild_id: String,
    user_id: String,
    status: String,
    requested_at_unix: i64,
    completed_at_unix: Option<i64>,
    receipt_id: Option<String>,
}

/// Tuple shape of the ledger read in [`ledger_row_for_requester`], factored out
/// so the query binding does not inline a very complex type.
type LedgerRowTuple = (String, String, String, i64, Option<i64>, Option<String>);

/// Load the ledger row for `request_id` and bind it to the authenticated
/// caller. Every mismatch — unknown id, cross-member read, cross-guild read —
/// fails with the same error and never leaks another member's data.
async fn ledger_row_for_requester(
    pool: &PgPool,
    guild_id: &str,
    requester_id: &str,
    request_id: &str,
) -> Result<LedgerRow, sqlx::Error> {
    validate_identity(guild_id)?;
    validate_identity(requester_id)?;
    validate_token_free_id(request_id, "invalid deletion request id")?;
    let row: Option<LedgerRowTuple> = sqlx::query_as(
        "SELECT guild_id, user_id, status, \
            (EXTRACT(EPOCH FROM requested_at))::BIGINT, \
            (EXTRACT(EPOCH FROM completed_at))::BIGINT, receipt_id \
         FROM retention_deletion_requests WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await?;
    let Some((owner_guild, owner_user, status, requested_at_unix, completed_at_unix, receipt_id)) =
        row
    else {
        return Err(invalid_request("unknown or settled deletion request"));
    };
    if owner_guild != guild_id || owner_user != requester_id {
        return Err(invalid_request("unknown or settled deletion request"));
    }
    Ok(LedgerRow {
        guild_id: owner_guild,
        user_id: owner_user,
        status,
        requested_at_unix,
        completed_at_unix,
        receipt_id,
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Durable, requester-authorized status read. Takes only guild/requester/
/// request ids — no bearer token — so it stays usable after the interaction
/// token expires.
pub async fn deletion_status_for_requester(
    pool: &PgPool,
    guild_id: &str,
    requester_id: &str,
    request_id: &str,
) -> Result<DeletionStatus, sqlx::Error> {
    let row = ledger_row_for_requester(pool, guild_id, requester_id, request_id).await?;
    let overdue =
        row.status == "pending" && now_unix() - row.requested_at_unix > DELETION_SLA_HOURS * 3600;
    Ok(DeletionStatus {
        request_id: request_id.to_owned(),
        guild_id: row.guild_id,
        user_id: row.user_id,
        status: row.status,
        requested_at_unix: row.requested_at_unix,
        completed_at_unix: row.completed_at_unix,
        overdue,
    })
}

fn deletion_receipt_scope(guild_id: &str, requester_id: &str) -> String {
    format!(
        "guild {guild_id} member {requester_id} member-footprint erasure \
         (erasable rows in the member erasure plan; request ledger retained)"
    )
}

/// Durable, requester-authorized completion receipt read. Fails while the
/// request is still pending — no false receipt — and never infers delivery
/// from anything but the settled ledger row the scheduler drain writes.
pub async fn deletion_receipt_for_requester(
    pool: &PgPool,
    guild_id: &str,
    requester_id: &str,
    request_id: &str,
) -> Result<DeletionReceiptView, sqlx::Error> {
    let row = ledger_row_for_requester(pool, guild_id, requester_id, request_id).await?;
    let (completed_at_unix, receipt_id) = match (row.completed_at_unix, row.receipt_id) {
        (Some(completed_at_unix), Some(receipt_id))
            if row.status == "completed" && !receipt_id.is_empty() =>
        {
            (completed_at_unix, receipt_id)
        }
        _ => return Err(invalid_request("unknown or settled deletion request")),
    };
    Ok(DeletionReceiptView {
        request_id: request_id.to_owned(),
        receipt_id,
        guild_id: row.guild_id.clone(),
        user_id: row.user_id.clone(),
        requested_at_unix: row.requested_at_unix,
        completed_at_unix,
        scope: deletion_receipt_scope(&row.guild_id, &row.user_id),
        retained_limits: RETAINED_LIMITS.to_owned(),
    })
}

/// The existing member `/privacy` definition, reused — never replaced or
/// duplicated — so registration, publication and gateway dispatch stay on the
/// one reviewed command surface. It binds the subject to the authenticated
/// requester (no caller-supplied user/member/target option).
///
/// The published definition includes deletion request/status/receipt verbs on
/// the same guild-only surface; the bot's live handler derives guild/member
/// identity from the interaction before invoking this intake.
pub fn privacy_deletion_command() -> CommandDefinition {
    if let Some(definition) = two_bot_core::commands::core_commands()
        .into_iter()
        .find(|definition| definition.name == "privacy")
    {
        return definition;
    }
    CommandDefinition::new(
        "privacy",
        "Manage your analytics privacy and deletion requests for this server.",
    )
}

#[cfg(test)]
mod tests {
    use super::{deletion_receipt_scope, RETAINED_LIMITS};

    #[test]
    fn receipt_discloses_retained_audit_and_only_member_erasure_scope() {
        assert!(RETAINED_LIMITS.contains("privacy_opt_out_audit (member_id, actor)"));
        assert!(RETAINED_LIMITS.contains("retention_deletion_requests request/receipt ledger"));
        let scope = deletion_receipt_scope("711111111111111111", "744444444444444444");
        assert!(scope.contains("guild 711111111111111111 member 744444444444444444"));
        assert!(scope.contains("erasable rows in the member erasure plan"));
        assert!(scope.contains("request ledger retained"));
        assert!(!scope.contains("purge queue"));
    }
}
