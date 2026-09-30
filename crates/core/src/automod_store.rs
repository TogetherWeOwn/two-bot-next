//! sqlx automod persistence; callers use the same pool as the bot's S6 store.
//!
//! Delivery claims prevent duplicate effects/funnel awards. The legacy ledger
//! counts each matched message once, even across different edit revisions.
//! Claim tokens fence stale completions. No lease automatically retries a
//! mutation whose Discord outcome is uncertain. Never log SQL binds here.

use sqlx::{PgPool, Postgres, Transaction};

use crate::automod::AutomodFilter;
use crate::automod_runtime::{AutomodMatch, DeliveryKey, MessageSubject, ViolationRecord};

#[derive(Debug, Clone)]
pub struct AutomodStore {
    pool: PgPool,
}

/// Opaque claim capability: only the winning insert can create it.
#[derive(Debug)]
pub struct DeliveryClaim {
    key: DeliveryKey,
    token: String,
}

#[derive(Debug)]
pub enum ClaimResult {
    Acquired(DeliveryClaim),
    InFlight,
    Replayed(StoredOutcome),
    /// A released pre-count claim preserved its match decision: the retry
    /// owns a rotated claim and replays these IDs/reason code without
    /// re-running the mutable repeat tracker. The caller still reconciles
    /// through target resolution, counting and planning.
    Preserved(DeliveryClaim, AutomodMatch),
}

/// One conflicting delivery-claim row: settled receipt plus the preserved
/// pre-count decision (IDs only, no message content).
type ClaimRow = (
    Option<sqlx::types::Json<StoredOutcome>>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
);

/// Execution receipt, not a plan: `deleted` is true only after confirmed REST
/// success. Completion has no arbitrary metadata field to smuggle message text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredOutcome {
    pub matched: bool,
    pub deleted: bool,
    pub outcome: CompletionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    Accepted,
    AlreadyProcessed,
    DryRun,
    Protected,
    Deleted,
    Warned,
    TimedOut,
    SanctionRefused,
}

impl AutomodStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn claim(&self, key: &DeliveryKey) -> Result<ClaimResult, sqlx::Error> {
        let inserted: Option<(String,)> = sqlx::query_as(
            "INSERT INTO automod_delivery_claims
               (guild_id, message_id, delivery_kind, dry_run, request_hash)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT DO NOTHING RETURNING claim_token",
        )
        .bind(&key.guild_id)
        .bind(&key.message_id)
        .bind(key.kind_name())
        .bind(key.dry_run)
        .bind(&key.request_hash)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((token,)) = inserted {
            return Ok(ClaimResult::Acquired(DeliveryClaim {
                key: key.clone(),
                token,
            }));
        }
        let row: Option<ClaimRow> = sqlx::query_as(
            "SELECT result_json, matched_filter, matched_guild_id, matched_channel_id,
                    matched_message_id, matched_author_id, released
             FROM automod_delivery_claims
             WHERE guild_id = $1 AND message_id = $2 AND delivery_kind = $3
               AND dry_run = $4 AND request_hash = $5",
        )
        .bind(&key.guild_id)
        .bind(&key.message_id)
        .bind(key.kind_name())
        .bind(key.dry_run)
        .bind(&key.request_hash)
        .fetch_optional(&self.pool)
        .await?;
        // A settled row replays its receipt. A released row is an explicit
        // pre-count handoff: the retry rotates ownership and replays the
        // preserved IDs/reason code without re-running the mutable in-memory
        // repeat tracker. An unreleased row is still owned by a live
        // claimant, so a conflict means InFlight.
        match row {
            Some((Some(result), _, _, _, _, _, _)) => Ok(ClaimResult::Replayed(result.0)),
            Some((
                None,
                Some(filter_name),
                Some(guild_id),
                Some(channel_id),
                Some(message_id),
                Some(author_id),
                true,
            )) => {
                let Some(filter) = AutomodFilter::from_persisted_name(&filter_name) else {
                    // An unrecognised stored name is replayable state
                    // corruption, never an invented match: block the retry
                    // for explicit reconciliation instead of accepting fresh.
                    return Ok(ClaimResult::InFlight);
                };
                let matched = AutomodMatch {
                    subject: MessageSubject {
                        guild_id,
                        channel_id,
                        message_id,
                        author_id,
                    },
                    filter,
                    funnel: key.kind.funnel(true),
                };
                // Rotate ownership to the retry so it can reconcile through
                // target resolution, counting and planning. Only one retry
                // wins; losers stay InFlight.
                let rotated: Option<(String,)> = sqlx::query_as(
                    "UPDATE automod_delivery_claims
                     SET claim_token = gen_random_uuid()::text, released = FALSE
                     WHERE guild_id = $1 AND message_id = $2 AND delivery_kind = $3
                       AND dry_run = $4 AND request_hash = $5
                       AND released = TRUE AND result_json IS NULL
                       AND matched_filter IS NOT NULL AND mutation_started = FALSE
                       AND counted = FALSE
                     RETURNING claim_token",
                )
                .bind(&key.guild_id)
                .bind(&key.message_id)
                .bind(key.kind_name())
                .bind(key.dry_run)
                .bind(&key.request_hash)
                .fetch_optional(&self.pool)
                .await?;
                match rotated {
                    Some((token,)) => Ok(ClaimResult::Preserved(
                        DeliveryClaim {
                            key: key.clone(),
                            token,
                        },
                        matched,
                    )),
                    None => Ok(ClaimResult::InFlight),
                }
            }
            _ => Ok(ClaimResult::InFlight),
        }
    }

    /// Preserve a pre-count match decision on the winning claim. The gateway
    /// calls this immediately after `inspect` returns a match, before target
    /// resolution: a same-revision retry then replays these IDs/reason code
    /// even when unrelated traffic has since swept the mutable in-memory
    /// repeat history. Setting is idempotent and owner-gated by token; only
    /// an active unmutated claim can carry a decision. Returns `false` when
    /// the claim is gone, settled, started, or counted.
    pub async fn preserve_match(
        &self,
        claim: &DeliveryClaim,
        matched: &AutomodMatch,
    ) -> Result<bool, sqlx::Error> {
        if claim.key.dry_run
            || claim.key.guild_id != matched.subject.guild_id
            || claim.key.message_id != matched.subject.message_id
        {
            return Err(sqlx::Error::InvalidArgument(
                "preserved match requires a matching enforce claim".into(),
            ));
        }
        let affected = sqlx::query(
            "UPDATE automod_delivery_claims
             SET matched_filter = $1, matched_guild_id = $2, matched_channel_id = $3,
                 matched_message_id = $4, matched_author_id = $5
             WHERE claim_token = $6 AND guild_id = $7 AND message_id = $8
               AND matched_filter IS NULL AND mutation_started = FALSE
               AND counted = FALSE AND result_json IS NULL AND released = FALSE",
        )
        .bind(matched.filter.as_str())
        .bind(&matched.subject.guild_id)
        .bind(&matched.subject.channel_id)
        .bind(&matched.subject.message_id)
        .bind(&matched.subject.author_id)
        .bind(&claim.token)
        .bind(&claim.key.guild_id)
        .bind(&claim.key.message_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    /// Commit this fence BEFORE sending the first Discord mutation. On failure
    /// the caller sends nothing; after success only `complete` can settle it.
    pub async fn mark_mutation_started(&self, claim: &DeliveryClaim) -> Result<bool, sqlx::Error> {
        let affected = sqlx::query(
            "UPDATE automod_delivery_claims SET mutation_started = TRUE
             WHERE claim_token = $1 AND guild_id = $2 AND message_id = $3
               AND result_json IS NULL AND mutation_started = FALSE AND dry_run = FALSE
               AND released = FALSE",
        )
        .bind(&claim.token)
        .bind(&claim.key.guild_id)
        .bind(&claim.key.message_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    pub async fn complete(
        &self,
        claim: &DeliveryClaim,
        outcome: &StoredOutcome,
    ) -> Result<bool, sqlx::Error> {
        if claim.key.dry_run
            && (outcome.deleted
                || !matches!(
                    outcome.outcome,
                    CompletionKind::Accepted | CompletionKind::DryRun
                ))
        {
            return Err(sqlx::Error::InvalidArgument(
                "dry-run claim cannot complete a mutation".into(),
            ));
        }
        let affected = sqlx::query(
            "UPDATE automod_delivery_claims SET result_json = $1, completed_at = CURRENT_TIMESTAMP
             WHERE claim_token = $2 AND guild_id = $3 AND message_id = $4 AND result_json IS NULL
               AND released = FALSE",
        )
        .bind(sqlx::types::Json(outcome))
        .bind(&claim.token)
        .bind(&claim.key.guild_id)
        .bind(&claim.key.message_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    /// Resolver/DB failures before mutation can be retried — but only before
    /// counting. Once the violation ledger holds this delivery, the claim is
    /// reconciliation evidence: keep it even if no Discord mutation started,
    /// so a retry never degrades to a silent AlreadyProcessed. After marking
    /// any mutation started, retain the claim even if a later followup was
    /// rejected.
    ///
    /// When the claim carries a preserved match decision, the row is marked
    /// released instead of deleted: the same-revision retry replays the
    /// decision (`ClaimResult::Preserved`) without re-running the mutable
    /// in-memory repeat tracker, which unrelated traffic may have swept in
    /// the meantime. Claims with no decision delete as before. Returns
    /// `false` when the claim is gone, settled, started, or counted.
    pub async fn release_unmutated(&self, claim: &DeliveryClaim) -> Result<bool, sqlx::Error> {
        let marked: Option<(bool,)> = sqlx::query_as(
            "UPDATE automod_delivery_claims SET released = TRUE
             WHERE claim_token = $1 AND guild_id = $2 AND message_id = $3
               AND mutation_started = FALSE AND counted = FALSE AND result_json IS NULL
               AND matched_filter IS NOT NULL AND released = FALSE
             RETURNING TRUE",
        )
        .bind(&claim.token)
        .bind(&claim.key.guild_id)
        .bind(&claim.key.message_id)
        .fetch_optional(&self.pool)
        .await?;
        if marked.is_some() {
            return Ok(true);
        }
        let affected = sqlx::query(
            "DELETE FROM automod_delivery_claims
             WHERE claim_token = $1 AND guild_id = $2 AND message_id = $3
               AND mutation_started = FALSE AND counted = FALSE AND result_json IS NULL
               AND matched_filter IS NULL",
        )
        .bind(&claim.token)
        .bind(&claim.key.guild_id)
        .bind(&claim.key.message_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    /// Run only for a resolved enforcing match (including protected targets).
    /// Inserting the processed ID FIRST arbitrates concurrent retries before
    /// the counter is touched, unlike a SELECT-then-increment race.
    pub async fn record_violation(
        &self,
        claim: &DeliveryClaim,
        subject: &MessageSubject,
        filter: AutomodFilter,
        at_iso: &str,
    ) -> Result<ViolationRecord, sqlx::Error> {
        if claim.key.dry_run
            || claim.key.guild_id != subject.guild_id
            || claim.key.message_id != subject.message_id
        {
            return Err(sqlx::Error::InvalidArgument(
                "violation requires a matching enforce claim".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let active: Option<(bool, bool, bool, bool)> = sqlx::query_as(
            "SELECT mutation_started, counted, result_json IS NOT NULL, released
             FROM automod_delivery_claims
             WHERE claim_token = $1 AND guild_id = $2 AND message_id = $3 FOR UPDATE",
        )
        .bind(&claim.token)
        .bind(&subject.guild_id)
        .bind(&subject.message_id)
        .fetch_optional(&mut *tx)
        .await?;
        if active != Some((false, false, false, false)) {
            return Err(sqlx::Error::InvalidArgument(
                "violation requires an active unmutated claim".into(),
            ));
        }
        let inserted = sqlx::query(
            "INSERT INTO automod_processed_messages (guild_id, message_id, user_id, processed_at)
             VALUES ($1, $2, $3, $4) ON CONFLICT (guild_id, message_id) DO NOTHING",
        )
        .bind(&subject.guild_id)
        .bind(&subject.message_id)
        .bind(&subject.author_id)
        .bind(at_iso)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        let count = if inserted {
            let (count,): (i32,) = sqlx::query_as(
                "INSERT INTO automod_violations
                   (guild_id, user_id, violation_count, last_filter, last_message_id, updated_at)
                 VALUES ($1, $2, 1, $3, $4, $5)
                 ON CONFLICT (guild_id, user_id) DO UPDATE
                   SET violation_count = automod_violations.violation_count + 1,
                       last_filter = excluded.last_filter,
                       last_message_id = excluded.last_message_id,
                       updated_at = excluded.updated_at
                 RETURNING violation_count",
            )
            .bind(&subject.guild_id)
            .bind(&subject.author_id)
            .bind(filter.as_str())
            .bind(&subject.message_id)
            .bind(at_iso)
            .fetch_one(&mut *tx)
            .await?;
            count
        } else {
            existing_count(&mut tx, subject).await?
        };
        // Counting and the counted fence commit together: no crash window lets
        // a counted ledger row outlive an uncounted (releasable) claim.
        sqlx::query(
            "UPDATE automod_delivery_claims SET counted = TRUE
             WHERE claim_token = $1 AND guild_id = $2 AND message_id = $3",
        )
        .bind(&claim.token)
        .bind(&subject.guild_id)
        .bind(&subject.message_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ViolationRecord {
            count: count as u64,
            inserted,
        })
    }
}

async fn existing_count(
    tx: &mut Transaction<'_, Postgres>,
    subject: &MessageSubject,
) -> Result<i32, sqlx::Error> {
    let (user_id, count): (String, i32) = sqlx::query_as(
        "SELECT p.user_id, v.violation_count FROM automod_processed_messages p
         JOIN automod_violations v ON v.guild_id = p.guild_id AND v.user_id = p.user_id
         WHERE p.guild_id = $1 AND p.message_id = $2",
    )
    .bind(&subject.guild_id)
    .bind(&subject.message_id)
    .fetch_one(&mut **tx)
    .await?;
    if user_id != subject.author_id {
        return Err(sqlx::Error::InvalidArgument(
            "automod message id belongs to another user".into(),
        ));
    }
    Ok(count)
}
