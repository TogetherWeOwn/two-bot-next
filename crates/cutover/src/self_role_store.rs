//! Fenced self-role event leases and shared exclusive-panel lanes.
//!
//! Ports legacy `src/store/selfRoleStore.ts` at d5d11793. Callers supply the
//! current UTC time on each operation (a deterministic test seam), and must
//! force-fetch member state after acquiring a lane. Recovery reuses persisted
//! intent, not a recalculated button toggle. A panel claim is keyed by
//! guild/member/panel, not event, so distinct events serialize across workers.
//!
//! This is storage fencing, not a guarantee that an in-flight Discord request
//! cannot outlive a lease. The executor must check/renew before and after REST
//! calls and reconcile ambiguous mutations to the committed target.
//!
//! Transactions reborrow the connection as documented by sqlx 0.9:
//! https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html
//! Unique-index arbitration uses PostgreSQL ON CONFLICT:
//! https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT

use sqlx::{PgConnection, PgPool};
use time::{Duration, OffsetDateTime};
use two_bot_core::self_roles::{
    event_order_from_snowflake, self_role_renew_after_ms, PanelMode, RoleOperation, SettledOutcome,
    SELF_ROLE_CLAIM_LEASE_MS,
};

/// Persist observed effects separately from attempted/compensated effects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditEffects {
    pub added_role_ids: Vec<String>,
    pub removed_role_ids: Vec<String>,
    pub attempted_added_role_ids: Vec<String>,
    pub attempted_removed_role_ids: Vec<String>,
    pub compensated_added_role_ids: Vec<String>,
    pub compensated_removed_role_ids: Vec<String>,
    pub unresolved_added_role_ids: Vec<String>,
    pub unresolved_removed_role_ids: Vec<String>,
}

impl AuditEffects {
    fn encoded(&self) -> [String; 8] {
        [
            &self.added_role_ids,
            &self.removed_role_ids,
            &self.attempted_added_role_ids,
            &self.attempted_removed_role_ids,
            &self.compensated_added_role_ids,
            &self.compensated_removed_role_ids,
            &self.unresolved_added_role_ids,
            &self.unresolved_removed_role_ids,
        ]
        .map(ids_json)
    }
}

/// Input for claim and settlement. Claim always persists `processing`;
/// settlement uses the typed final outcome and preserves the original intent.
#[derive(Debug, Clone)]
pub struct SelfRoleAudit {
    pub event_id: String,
    pub event_order: Option<String>,
    pub guild_id: String,
    pub panel_id: String,
    pub member_id: String,
    pub source_id: String,
    pub option_key: Option<String>,
    pub role_id: Option<String>,
    pub source: PanelMode,
    pub operation: RoleOperation,
    pub outcome: SettledOutcome,
    pub code: Option<String>,
    pub reason: Option<String>,
    pub effects: AuditEffects,
    pub desired_role_ids: Vec<String>,
    pub pre_mutation_role_ids: Vec<String>,
}

/// Opaque fencing identity plus original intent returned by recovery.
#[derive(Debug, Clone)]
pub struct EventClaim {
    pub event_id: String,
    pub token: String,
    pub generation: i32,
    pub recovered: bool,
    pub desired_role_ids: Vec<String>,
    pub pre_mutation_role_ids: Vec<String>,
    pub renew_after_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelKey {
    pub guild_id: String,
    pub member_id: String,
    pub panel_id: String,
}

/// Null option with `committed=true` means committed empty selection; null
/// with `committed=false` means an uninitialized lane that must not be repaired.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PanelTarget {
    pub latest_event_id: Option<String>,
    pub latest_event_order: Option<String>,
    pub option_key: Option<String>,
    pub committed: bool,
}

#[derive(Debug, Clone)]
pub struct PanelClaim {
    pub key: PanelKey,
    pub token: String,
    pub generation: i32,
    pub target: PanelTarget,
    pub renew_after_ms: u64,
}

/// A superseded event has no ownership; do not confuse it with a claim.
#[derive(Debug, Clone)]
pub enum PanelClaimResult {
    Acquired(PanelClaim),
    Busy,
    Superseded(PanelTarget),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid persisted self-role intent: {0}")]
    Intent(#[from] serde_json::Error),
    #[error("self-role event claim is stale or belongs to another event")]
    StaleClaim,
    #[error("self-role event and panel claim scopes differ")]
    WrongPanel,
    #[error("lease duration must be positive and expiry must be representable")]
    InvalidLease,
    #[error("claim generation exhausted")]
    GenerationExhausted,
}

#[derive(Debug, Clone)]
pub struct SelfRoleStore {
    pool: PgPool,
    lease_ms: u64,
}

impl SelfRoleStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            lease_ms: SELF_ROLE_CLAIM_LEASE_MS,
        }
    }

    pub fn with_lease(pool: PgPool, lease_ms: u64) -> Result<Self, StoreError> {
        if lease_ms == 0 || lease_ms > i64::MAX as u64 {
            return Err(StoreError::InvalidLease);
        }
        Ok(Self { pool, lease_ms })
    }

    fn expiry(&self, now: OffsetDateTime) -> Result<OffsetDateTime, StoreError> {
        now.checked_add(Duration::milliseconds(self.lease_ms as i64))
            .ok_or(StoreError::InvalidLease)
    }

    /// Insert-first deduplication, then generation-fenced expired recovery.
    pub async fn claim_audit(
        &self,
        row: &SelfRoleAudit,
        now: OffsetDateTime,
    ) -> Result<Option<EventClaim>, StoreError> {
        let expires = self.expiry(now)?;
        let mut tx = self.pool.begin().await?;
        let created = now
            .to_offset(time::UtcOffset::UTC)
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| StoreError::InvalidLease)?;
        let mut query = sqlx::query_as::<_, (String,)>(
            "INSERT INTO self_role_audit
             (event_id, event_order, guild_id, panel_id, member_id, source_id, option_key, role_id,
              source, operation, outcome, added_role_ids, removed_role_ids,
              attempted_added_role_ids, attempted_removed_role_ids,
              compensated_added_role_ids, compensated_removed_role_ids,
              unresolved_added_role_ids, unresolved_removed_role_ids,
              desired_role_ids, pre_mutation_role_ids, claim_token, claim_generation,
              processing_expires_at, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'processing',
                     $11,$12,$13,$14,$15,$16,$17,$18,$19,$20,gen_random_uuid()::text,1,$21,$22)
             ON CONFLICT (event_id) DO NOTHING RETURNING claim_token",
        )
        .bind(&row.event_id)
        .bind(&row.event_order)
        .bind(&row.guild_id)
        .bind(&row.panel_id)
        .bind(&row.member_id)
        .bind(&row.source_id)
        .bind(&row.option_key)
        .bind(&row.role_id)
        .bind(row.source.as_str())
        .bind(row.operation.as_str());
        for effect in row.effects.encoded() {
            query = query.bind(effect);
        }
        let inserted = query
            .bind(ids_json(&row.desired_role_ids))
            .bind(ids_json(&row.pre_mutation_role_ids))
            .bind(expires)
            .bind(created)
            .fetch_optional(&mut *tx)
            .await?;
        let claim = if let Some((token,)) = inserted {
            Some(EventClaim {
                event_id: row.event_id.clone(),
                token,
                generation: 1,
                recovered: false,
                desired_role_ids: row.desired_role_ids.clone(),
                pre_mutation_role_ids: row.pre_mutation_role_ids.clone(),
                renew_after_ms: self_role_renew_after_ms(self.lease_ms),
            })
        } else {
            let prior: Option<(i32, String, String)> = sqlx::query_as(
                "SELECT claim_generation, desired_role_ids, pre_mutation_role_ids
                 FROM self_role_audit WHERE event_id=$1 AND outcome='processing'
                 AND processing_expires_at <= $2 FOR UPDATE",
            )
            .bind(&row.event_id)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((generation, desired, before)) = prior {
                let next = generation
                    .checked_add(1)
                    .ok_or(StoreError::GenerationExhausted)?;
                // Invalid snapshots fail closed rather than recovering an empty target.
                let desired_role_ids = serde_json::from_str(&desired)?;
                let pre_mutation_role_ids = serde_json::from_str(&before)?;
                let (token,): (String,) = sqlx::query_as(
                    "UPDATE self_role_audit SET claim_token=gen_random_uuid()::text,
                     claim_generation=$2, processing_expires_at=$3 WHERE event_id=$1
                     RETURNING claim_token",
                )
                .bind(&row.event_id)
                .bind(next)
                .bind(expires)
                .fetch_one(&mut *tx)
                .await?;
                Some(EventClaim {
                    event_id: row.event_id.clone(),
                    token,
                    generation: next,
                    recovered: true,
                    desired_role_ids,
                    pre_mutation_role_ids,
                    renew_after_ms: self_role_renew_after_ms(self.lease_ms),
                })
            } else {
                None
            }
        };
        tx.commit().await?;
        Ok(claim)
    }

    pub async fn owns_claim(
        &self,
        claim: &EventClaim,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let (owned,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM self_role_audit WHERE event_id=$1
             AND claim_token=$2 AND claim_generation=$3 AND outcome='processing'
             AND processing_expires_at > $4)",
        )
        .bind(&claim.event_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(owned)
    }

    pub async fn renew_claim(
        &self,
        claim: &EventClaim,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "UPDATE self_role_audit SET processing_expires_at=$5 WHERE event_id=$1
             AND claim_token=$2 AND claim_generation=$3 AND outcome='processing'
             AND processing_expires_at > $4",
        )
        .bind(&claim.event_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(now)
        .bind(self.expiry(now)?)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Try once; the executor owns bounded retry/backoff. Only a lane winner
    /// supersedes older audit intents. Repair (`event=None`) retains chronology.
    pub async fn claim_panel(
        &self,
        key: &PanelKey,
        event: Option<(&str, &str)>,
        now: OffsetDateTime,
    ) -> Result<PanelClaimResult, StoreError> {
        let expires = self.expiry(now)?;
        let mut tx = self.pool.begin().await?;
        let event_id = event.map(|(id, _)| id);
        let event_order = event.map(|(_, order)| order);
        let inserted: Option<(String,)> = sqlx::query_as(
            "INSERT INTO self_role_panel_claims
             (guild_id,member_id,panel_id,claim_token,claim_generation,processing_expires_at,
              latest_event_id,latest_event_order)
             VALUES ($1,$2,$3,gen_random_uuid()::text,1,$4,$5,$6)
             ON CONFLICT (guild_id,member_id,panel_id) DO NOTHING RETURNING claim_token",
        )
        .bind(&key.guild_id)
        .bind(&key.member_id)
        .bind(&key.panel_id)
        .bind(expires)
        .bind(event_id)
        .bind(event_order)
        .fetch_optional(&mut *tx)
        .await?;
        let (token, generation, target) = if let Some((token,)) = inserted {
            (
                token,
                1,
                PanelTarget {
                    latest_event_id: event_id.map(str::to_owned),
                    latest_event_order: event_order.map(str::to_owned),
                    ..PanelTarget::default()
                },
            )
        } else {
            let (generation, expiry, latest_id, option, committed, order):
                (i32, OffsetDateTime, Option<String>, Option<String>, bool, Option<String>) = sqlx::query_as(
                "SELECT claim_generation,processing_expires_at,latest_event_id,
                 latest_option_key,target_committed,latest_event_order
                 FROM self_role_panel_claims WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3 FOR UPDATE",
            ).bind(&key.guild_id).bind(&key.member_id).bind(&key.panel_id)
                .fetch_one(&mut *tx).await?;
            let prior_order =
                order.or_else(|| latest_id.as_deref().and_then(event_order_from_snowflake));
            let mut target = PanelTarget {
                latest_event_id: latest_id,
                latest_event_order: prior_order,
                option_key: option,
                committed,
            };
            if event_order
                .zip(target.latest_event_order.as_deref())
                .is_some_and(|(a, b)| a < b)
            {
                tx.commit().await?;
                return Ok(PanelClaimResult::Superseded(target));
            }
            if expiry > now {
                tx.commit().await?;
                return Ok(PanelClaimResult::Busy);
            }
            let next = generation
                .checked_add(1)
                .ok_or(StoreError::GenerationExhausted)?;
            if let Some((id, order)) = event {
                target.latest_event_id = Some(id.to_owned());
                target.latest_event_order = Some(order.to_owned());
            }
            let (token,): (String,) = sqlx::query_as(
                "UPDATE self_role_panel_claims SET claim_token=gen_random_uuid()::text,
                 claim_generation=$4, processing_expires_at=$5, latest_event_id=$6, latest_event_order=$7
                 WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3 RETURNING claim_token",
            ).bind(&key.guild_id).bind(&key.member_id).bind(&key.panel_id)
                .bind(next).bind(expires).bind(&target.latest_event_id).bind(&target.latest_event_order)
                .fetch_one(&mut *tx).await?;
            (token, next, target)
        };
        if let Some((id, order)) = event {
            sqlx::query(
                "UPDATE self_role_audit SET outcome='rejected',code='superseded_by_later_event',
                 reason='a later exclusive-panel event was accepted',processing_expires_at=NULL
                 WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3 AND event_id<>$4
                 AND event_order<$5 AND outcome='processing'",
            )
            .bind(&key.guild_id)
            .bind(&key.member_id)
            .bind(&key.panel_id)
            .bind(id)
            .bind(order)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(PanelClaimResult::Acquired(PanelClaim {
            key: key.clone(),
            token,
            generation,
            target,
            renew_after_ms: self_role_renew_after_ms(self.lease_ms),
        }))
    }

    pub async fn owns_panel_claim(
        &self,
        claim: &PanelClaim,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let (owned,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM self_role_panel_claims WHERE guild_id=$1
             AND member_id=$2 AND panel_id=$3 AND claim_token=$4 AND claim_generation=$5
             AND processing_expires_at > $6)",
        )
        .bind(&claim.key.guild_id)
        .bind(&claim.key.member_id)
        .bind(&claim.key.panel_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(owned)
    }

    pub async fn renew_panel_claim(
        &self,
        claim: &PanelClaim,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "UPDATE self_role_panel_claims SET processing_expires_at=$7 WHERE guild_id=$1
             AND member_id=$2 AND panel_id=$3 AND claim_token=$4 AND claim_generation=$5
             AND processing_expires_at > $6",
        )
        .bind(&claim.key.guild_id)
        .bind(&claim.key.member_id)
        .bind(&claim.key.panel_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(now)
        .bind(self.expiry(now)?)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Retain the committed target and chronology; stale releases are no-ops.
    pub async fn release_panel_claim(
        &self,
        claim: &PanelClaim,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query(
            "UPDATE self_role_panel_claims SET processing_expires_at=$6 WHERE guild_id=$1
             AND member_id=$2 AND panel_id=$3 AND claim_token=$4 AND claim_generation=$5",
        )
        .bind(&claim.key.guild_id)
        .bind(&claim.key.member_id)
        .bind(&claim.key.panel_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Seed a previously uninitialized lane from freshly fetched member state.
    pub async fn set_panel_claim_option(
        &self,
        claim: &mut PanelClaim,
        option: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut conn = self.pool.acquire().await?;
        let changed = set_panel_option(&mut conn, claim, option, now).await?;
        if changed {
            claim.target.option_key = option.map(str::to_owned);
            claim.target.committed = true;
        }
        Ok(changed)
    }

    /// Effects may arrive after expiry, but never after transfer to a new
    /// generation. This records evidence, not authorization for another REST call.
    pub async fn update_audit_effects(
        &self,
        claim: &EventClaim,
        effects: &AuditEffects,
    ) -> Result<bool, StoreError> {
        let mut query = sqlx::query(
            "UPDATE self_role_audit SET added_role_ids=$1,removed_role_ids=$2,
             attempted_added_role_ids=$3,attempted_removed_role_ids=$4,
             compensated_added_role_ids=$5,compensated_removed_role_ids=$6,
             unresolved_added_role_ids=$7,unresolved_removed_role_ids=$8
             WHERE event_id=$9 AND claim_token=$10 AND claim_generation=$11 AND outcome='processing'",
        );
        for effect in effects.encoded() {
            query = query.bind(effect);
        }
        Ok(query
            .bind(&claim.event_id)
            .bind(&claim.token)
            .bind(claim.generation)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    /// Explicit claim is required (no unsafe implicit lookup of another worker's
    /// token). Original intent/order/created_at stay immutable during settlement.
    pub async fn finish_audit(
        &self,
        row: &SelfRoleAudit,
        claim: &EventClaim,
    ) -> Result<(), StoreError> {
        let mut conn = self.pool.acquire().await?;
        finish_audit(&mut conn, row, claim).await
    }

    /// Publish committed target and settled audit atomically. A stale event
    /// rolls back the panel write; a stale panel makes no writes at all.
    pub async fn finish_audit_and_set_panel_option(
        &self,
        row: &SelfRoleAudit,
        claim: &EventClaim,
        panel: &mut PanelClaim,
        option: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        if row.guild_id != panel.key.guild_id
            || row.member_id != panel.key.member_id
            || row.panel_id != panel.key.panel_id
        {
            return Err(StoreError::WrongPanel);
        }
        let mut tx = self.pool.begin().await?;
        if !set_panel_option(&mut tx, panel, option, now).await? {
            tx.rollback().await?;
            return Ok(false);
        }
        finish_audit(&mut tx, row, claim).await?;
        tx.commit().await?;
        panel.target.option_key = option.map(str::to_owned);
        panel.target.committed = true;
        Ok(true)
    }
}

fn ids_json(ids: &Vec<String>) -> String {
    // Serialization of a Vec<String> cannot fail.
    serde_json::to_string(ids).expect("role-id array serializes")
}

async fn set_panel_option(
    conn: &mut PgConnection,
    claim: &PanelClaim,
    option: Option<&str>,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    Ok(sqlx::query(
        "UPDATE self_role_panel_claims SET latest_option_key=$7,target_committed=TRUE
         WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3 AND claim_token=$4
         AND claim_generation=$5 AND processing_expires_at > $6",
    )
    .bind(&claim.key.guild_id)
    .bind(&claim.key.member_id)
    .bind(&claim.key.panel_id)
    .bind(&claim.token)
    .bind(claim.generation)
    .bind(now)
    .bind(option)
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

async fn finish_audit(
    conn: &mut PgConnection,
    row: &SelfRoleAudit,
    claim: &EventClaim,
) -> Result<(), StoreError> {
    if row.event_id != claim.event_id {
        return Err(StoreError::StaleClaim);
    }
    let mut query = sqlx::query(
        "UPDATE self_role_audit SET option_key=$1,role_id=$2,source=$3,operation=$4,
         outcome=$5,code=$6,reason=$7,added_role_ids=$8,removed_role_ids=$9,
         attempted_added_role_ids=$10,attempted_removed_role_ids=$11,
         compensated_added_role_ids=$12,compensated_removed_role_ids=$13,
         unresolved_added_role_ids=$14,unresolved_removed_role_ids=$15,processing_expires_at=NULL
         WHERE event_id=$16 AND claim_token=$17 AND claim_generation=$18 AND outcome='processing'
         AND guild_id=$19 AND member_id=$20 AND panel_id=$21 AND source_id=$22",
    )
    .bind(&row.option_key)
    .bind(&row.role_id)
    .bind(row.source.as_str())
    .bind(row.operation.as_str())
    .bind(row.outcome.as_str())
    .bind(&row.code)
    .bind(&row.reason);
    for effect in row.effects.encoded() {
        query = query.bind(effect);
    }
    let changed = query
        .bind(&row.event_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(&row.guild_id)
        .bind(&row.member_id)
        .bind(&row.panel_id)
        .bind(&row.source_id)
        .execute(conn)
        .await?
        .rows_affected();
    if changed != 1 {
        return Err(StoreError::StaleClaim);
    }
    Ok(())
}
