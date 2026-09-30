//! Durable lockdown recovery state and the shared moderation audit/idempotency
//! ledger. Legacy table/column names live in migration `0120`.
//!
//! Compiled only with `db`; pure domain tests need no Postgres driver. The
//! runtime must persist the seed before locking, retain claims on uncertain
//! Discord failures, and clear recovery state only after restoration succeeds.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::str::FromStr;

use super::channel_moderation::LockdownRecord;

/// One audit row for a channel verb (legacy `ModerationAuditRow`).
#[derive(Debug, Clone)]
pub struct ChannelAuditRow {
    pub request_id: String,
    pub guild_id: String,
    pub actor_id: String,
    /// Legacy `ModerationActionName`, e.g. `moderation.lockdown`.
    pub action: String,
    pub channel_id: Option<String>,
    pub reason: String,
    /// Legacy outcome string, e.g. `locked_down`, or `refused`.
    pub outcome: String,
    pub idempotency_key: String,
    /// Bounded numbers only (count/seconds/affected), serialised as JSON.
    pub metadata_json: String,
    /// ISO-8601 UTC millis (legacy TEXT timestamps; see migration 0120).
    pub created_at: String,
}

/// Result of an idempotency claim (legacy `ModerationClaim`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelClaim {
    /// First attempt won; caller must act, then [`ChannelModerationStore::complete`].
    Claimed { ticket: ChannelClaimTicket },
    /// Key already completed; replay the stored result, make no Discord call.
    Replayed {
        outcome: String,
        result_json: String,
    },
    /// An earlier attempt is still uncertain; caller must refuse `in_progress`.
    InFlight,
    /// Key was used for different request content; caller must refuse `malformed`.
    Mismatch,
}

/// Ownership of one winning claim generation, returned only by [`ChannelModerationStore::claim`].
/// Keep this ticket for completion or a proven-safe release; never use a new
/// claimant's ticket to retry an old attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelClaimTicket {
    guild_id: String,
    idempotency_key: String,
    claim_token: String,
}

/// Durable store for the channel-moderation slice.
#[derive(Debug, Clone)]
pub struct ChannelModerationStore {
    pool: PgPool,
}

/// Legacy pool default (`TWO_DB_POOL_MAX ?? 5`).
pub const DB_POOL_MAX_DEFAULT: u32 = 5;
/// Legacy statement timeout (`statementTimeoutMillis ?? 15_000`).
pub const STATEMENT_TIMEOUT_MS: u64 = 15_000;

impl ChannelModerationStore {
    /// Reuse the runtime's pool instead of opening a separate pool per handler.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a pool against `url` (Postgres only).
    pub async fn connect(url: &str, pool_max: u32) -> Result<Self, sqlx::Error> {
        if url.trim().is_empty() {
            return Err(sqlx::Error::InvalidArgument(
                "database URL is required".to_owned(),
            ));
        }
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(sqlx::Error::InvalidArgument(
                "only Postgres is supported".to_owned(),
            ));
        }
        let mut options = PgConnectOptions::from_str(url)
            .map_err(|_| sqlx::Error::InvalidArgument("invalid database URL".to_owned()))?;
        options = options.options([("statement_timeout", format!("{}ms", STATEMENT_TIMEOUT_MS))]);
        let pool = PgPoolOptions::new()
            .max_connections(pool_max)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    /// Apply the crate's embedded cutover migrations (includes 0120).
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::migrate!("../cutover/migrations")
            .run(&self.pool)
            .await
            .map_err(|e| sqlx::Error::InvalidArgument(format!("migration failed: {e}")))
    }

    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Remember exactly what the `@everyone` overwrite was before Owen denied
    /// `SendMessages`. Insert-only: a repeated lockdown refreshes the reason
    /// but can never replace the original pre-lock masks (or the original
    /// recovery generation) with the already-locked masks (legacy
    /// `recordLockdown`, TOG-1659 High 1).
    pub async fn record_lockdown(
        &self,
        channel_id: &str,
        guild_id: &str,
        seed: &super::channel_moderation::LockdownSeed,
        reason: &str,
        locked_at: &str,
    ) -> Result<LockdownRecord, sqlx::Error> {
        let row = sqlx::query(
            "INSERT INTO moderation_lockdowns
               (channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason, locked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (channel_id) DO UPDATE
               SET reason = excluded.reason,
                   locked_at = excluded.locked_at
             RETURNING channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason,
                       recovery_generation",
        )
        .bind(channel_id)
        .bind(guild_id)
        .bind(&seed.prior_allow)
        .bind(&seed.prior_deny)
        .bind(seed.prior_exists)
        .bind(reason)
        .bind(locked_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(LockdownRecord {
            channel_id: row.get("channel_id"),
            guild_id: row.get("guild_id"),
            prior_allow: row.get("prior_allow"),
            prior_deny: row.get("prior_deny"),
            prior_exists: row.get("prior_exists"),
            reason: row.get("reason"),
            recovery_generation: row.get("recovery_generation"),
        })
    }

    /// Read the stored pre-lockdown overwrite without consuming recovery
    /// state (legacy `getLockdown`).
    pub async fn get_lockdown(
        &self,
        channel_id: &str,
    ) -> Result<Option<LockdownRecord>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason,
                    recovery_generation
               FROM moderation_lockdowns WHERE channel_id = $1",
        )
        .bind(channel_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| LockdownRecord {
            channel_id: row.get("channel_id"),
            guild_id: row.get("guild_id"),
            prior_allow: row.get("prior_allow"),
            prior_deny: row.get("prior_deny"),
            prior_exists: row.get("prior_exists"),
            reason: row.get("reason"),
            recovery_generation: row.get("recovery_generation"),
        }))
    }

    /// Delete recovery state only after Discord accepted the exact
    /// restoration the caller planned from the matching generation (legacy
    /// `clearLockdown`). A delayed unlock — or a retried cleanup whose
    /// earlier result was lost — must present the generation it restored;
    /// a stale generation (including one for a deleted-and-recreated row
    /// with identical masks) deletes nothing and returns false, so a later
    /// lockdown cycle's seed survives. The runtime caller must still
    /// serialize channel-scoped mutations; claim tokens fence only the
    /// request ledger (wiring follow-up: TOG-10174).
    pub async fn clear_lockdown(
        &self,
        channel_id: &str,
        recovery_generation: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM moderation_lockdowns
              WHERE channel_id = $1 AND recovery_generation = $2",
        )
        .bind(channel_id)
        .bind(recovery_generation)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Claim `(guild_id, idempotency_key)` before any Discord mutation
    /// (legacy `claim`, TOG-1659 High 3). One atomic `INSERT ... ON CONFLICT
    /// DO NOTHING`; losers read the existing row.
    pub async fn claim(
        &self,
        guild_id: &str,
        idempotency_key: &str,
        action: &str,
        request_hash: &str,
        claimed_at: &str,
    ) -> Result<ChannelClaim, sqlx::Error> {
        let won = sqlx::query(
            "INSERT INTO moderation_idempotency
               (guild_id, idempotency_key, action, request_hash, state, claimed_at)
             VALUES ($1, $2, $3, $4, 'in_flight', $5)
             ON CONFLICT (guild_id, idempotency_key) DO NOTHING
             RETURNING claim_token",
        )
        .bind(guild_id)
        .bind(idempotency_key)
        .bind(action)
        .bind(request_hash)
        .bind(claimed_at)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = won {
            return Ok(ChannelClaim::Claimed {
                ticket: ChannelClaimTicket {
                    guild_id: guild_id.to_owned(),
                    idempotency_key: idempotency_key.to_owned(),
                    claim_token: row.get("claim_token"),
                },
            });
        }
        let row = sqlx::query(
            "SELECT action, request_hash, state, outcome, result_json
               FROM moderation_idempotency
              WHERE guild_id = $1 AND idempotency_key = $2",
        )
        .bind(guild_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            // Deleted between our INSERT losing and this SELECT — a failed
            // attempt releasing its claim. Still running; the caller retries.
            return Ok(ChannelClaim::InFlight);
        };
        let stored_action: String = row.get("action");
        let stored_hash: String = row.get("request_hash");
        if stored_action != action || stored_hash != request_hash {
            return Ok(ChannelClaim::Mismatch);
        }
        let state: String = row.get("state");
        if state == "done" {
            let outcome: Option<String> = row.get("outcome");
            let result_json: Option<String> = row.get("result_json");
            return Ok(ChannelClaim::Replayed {
                outcome: outcome.unwrap_or_else(|| "unknown".to_owned()),
                result_json: result_json.unwrap_or_else(|| "{}".to_owned()),
            });
        }
        Ok(ChannelClaim::InFlight)
    }

    /// Take the channel lane only while this request still owns its claim.
    /// The lane is durable and has no timeout: a lost process or ambiguous
    /// Discord result cannot let another key race an unfinished restoration.
    pub async fn claim_channel(
        &self,
        ticket: &ChannelClaimTicket,
        channel_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO moderation_channel_execution
               (guild_id, channel_id, idempotency_key, claim_token)
             SELECT guild_id, $1, idempotency_key, claim_token
               FROM moderation_idempotency
              WHERE guild_id = $2 AND idempotency_key = $3
                AND claim_token = $4 AND state = 'in_flight'
             ON CONFLICT (guild_id, channel_id) DO NOTHING",
        )
        .bind(channel_id)
        .bind(&ticket.guild_id)
        .bind(&ticket.idempotency_key)
        .bind(&ticket.claim_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Finish without a gap between completion, audit, recovery cleanup and
    /// releasing the channel lane. On DB failure the claim/lane stay in flight;
    /// the caller may retry this write with the same ticket, never Discord.
    /// `restored` is supplied only after confirmed exact unlock restoration.
    pub async fn finish(
        &self,
        ticket: &ChannelClaimTicket,
        row: &ChannelAuditRow,
        result_json: &str,
        restored: Option<&LockdownRecord>,
    ) -> Result<bool, sqlx::Error> {
        if row.guild_id != ticket.guild_id || row.idempotency_key != ticket.idempotency_key {
            return Err(sqlx::Error::InvalidArgument(
                "audit does not match claim".to_owned(),
            ));
        }
        if restored.is_some_and(|rec| {
            rec.guild_id != ticket.guild_id || row.channel_id.as_deref() != Some(&rec.channel_id)
        }) {
            return Err(sqlx::Error::InvalidArgument(
                "recovery does not match claim".to_owned(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE moderation_idempotency
                SET state = 'done', outcome = $1, result_json = $2, completed_at = $3
              WHERE guild_id = $4 AND idempotency_key = $5
                AND claim_token = $6 AND state = 'in_flight' AND action = $7",
        )
        .bind(&row.outcome)
        .bind(result_json)
        .bind(&row.created_at)
        .bind(&ticket.guild_id)
        .bind(&ticket.idempotency_key)
        .bind(&ticket.claim_token)
        .bind(&row.action)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO moderation_audit
               (request_id, guild_id, actor_id, action, target_id, channel_id, reason,
                outcome, idempotency_key, metadata_json, created_at)
             VALUES ($1, $2, $3, $4, NULL, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (request_id) DO NOTHING",
        )
        .bind(&row.request_id)
        .bind(&row.guild_id)
        .bind(&row.actor_id)
        .bind(&row.action)
        .bind(&row.channel_id)
        .bind(&row.reason)
        .bind(&row.outcome)
        .bind(&row.idempotency_key)
        .bind(&row.metadata_json)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await?;
        if let Some(rec) = restored {
            let cleared = sqlx::query(
                "DELETE FROM moderation_lockdowns
                  WHERE channel_id = $1 AND guild_id = $2 AND recovery_generation = $3",
            )
            .bind(&rec.channel_id)
            .bind(&rec.guild_id)
            .bind(&rec.recovery_generation)
            .execute(&mut *tx)
            .await?;
            if cleared.rows_affected() != 1 {
                return Ok(false);
            }
        }
        sqlx::query(
            "DELETE FROM moderation_channel_execution
              WHERE guild_id = $1 AND idempotency_key = $2 AND claim_token = $3",
        )
        .bind(&ticket.guild_id)
        .bind(&ticket.idempotency_key)
        .bind(&ticket.claim_token)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Record the result so a retry replays it instead of acting again.
    /// Returns false if the ticket is stale or already completed; never
    /// overwrites a newer generation or an immutable completed result.
    pub async fn complete(
        &self,
        ticket: &ChannelClaimTicket,
        outcome: &str,
        result_json: &str,
        completed_at: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE moderation_idempotency
                SET state = 'done', outcome = $1, result_json = $2, completed_at = $3
              WHERE guild_id = $4 AND idempotency_key = $5
                AND claim_token = $6 AND state = 'in_flight'",
        )
        .bind(outcome)
        .bind(result_json)
        .bind(completed_at)
        .bind(&ticket.guild_id)
        .bind(&ticket.idempotency_key)
        .bind(&ticket.claim_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Release only when the executor can prove no Discord mutation occurred.
    /// A timeout or ambiguous failure must retain the claim to prevent replay.
    /// Returns false for stale tickets or completed claims.
    pub async fn release(&self, ticket: &ChannelClaimTicket) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM moderation_idempotency
              WHERE guild_id = $1 AND idempotency_key = $2
                AND claim_token = $3 AND state = 'in_flight'",
        )
        .bind(&ticket.guild_id)
        .bind(&ticket.idempotency_key)
        .bind(&ticket.claim_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// One audit row per executed (or refused) channel action. Insert is
    /// `ON CONFLICT (request_id) DO NOTHING`: Discord already accepted the
    /// action, so a retry must never duplicate the mutation over an audit
    /// write failure (legacy `recordAudit`).
    pub async fn record_audit(&self, row: &ChannelAuditRow) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO moderation_audit
               (request_id, guild_id, actor_id, action, target_id, channel_id, reason,
                outcome, idempotency_key, metadata_json, created_at)
             VALUES ($1, $2, $3, $4, NULL, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (request_id) DO NOTHING",
        )
        .bind(&row.request_id)
        .bind(&row.guild_id)
        .bind(&row.actor_id)
        .bind(&row.action)
        .bind(&row.channel_id)
        .bind(&row.reason)
        .bind(&row.outcome)
        .bind(&row.idempotency_key)
        .bind(&row.metadata_json)
        .bind(&row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestStore {
        store: ChannelModerationStore,
        admin: PgPool,
        schema: String,
    }

    impl std::ops::Deref for TestStore {
        type Target = ChannelModerationStore;

        fn deref(&self) -> &Self::Target {
            &self.store
        }
    }

    impl TestStore {
        async fn cleanup(self) {
            self.store.pool.close().await;
            // Schema identifiers contain only the fixed prefix and local digits.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA {} CASCADE",
                self.schema
            )))
            .execute(&self.admin)
            .await
            .expect("drops only this test's schema");
            self.admin.close().await;
        }
    }

    // Deliberately reject arbitrary URLs before opening any connection. Only
    // the mandated empty-password test service (or its CI counterpart) is allowed.
    fn test_database_url_allowed(url: &str, github_actions: bool) -> bool {
        url == "postgres://agent_test@agent-testdb:5432/agent_test"
            || (github_actions && url == "postgres://agent_test@127.0.0.1:5432/agent_test")
    }

    async fn test_store() -> TestStore {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("set TWO_TEST_DATABASE_URL to the agent-testdb service");
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        assert!(
            test_database_url_allowed(&url, ci),
            "non-test database refused"
        );
        assert!(
            std::env::var_os("PGOPTIONS").is_none(),
            "PGOPTIONS is not allowed in isolated tests"
        );
        // Do not consult .pgpass or inherit a password, host, role, or TLS key.
        let host = if url.contains("@127.0.0.1:") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let base_options = PgConnectOptions::new_without_pgpass()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test")
            .ssl_mode(sqlx::postgres::PgSslMode::Disable)
            .application_name("channel_moderation_test");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(base_options.clone())
            .await
            .expect("connects to test service with the mandated empty password");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let schema = format!("cm_test_{}_{}_{}", std::process::id(), nonce, sequence);
        // No external input participates in this generated SQL identifier.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("creates isolated test schema");
        let options = base_options.options([("search_path", schema.clone())]);
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .expect("connects within isolated schema");
        let store = ChannelModerationStore::from_pool(pool);
        store.migrate().await.expect("migrations apply");
        TestStore {
            store,
            admin,
            schema,
        }
    }

    #[test]
    fn test_database_guard_refuses_non_test_urls() {
        assert!(test_database_url_allowed(
            "postgres://agent_test@agent-testdb:5432/agent_test",
            false
        ));
        assert!(test_database_url_allowed(
            "postgres://agent_test@127.0.0.1:5432/agent_test",
            true
        ));
        for url in [
            "postgres://agent_test@127.0.0.1:5432/agent_test",
            "postgres://agent_test@production:5432/agent_test",
            "postgres://admin@agent-testdb:5432/agent_test",
            "postgres://agent_test:secret@agent-testdb:5432/agent_test",
            "postgres://agent_test@agent-testdb:5432/production",
        ] {
            assert!(!test_database_url_allowed(url, false));
        }
    }

    async fn ticket(store: &ChannelModerationStore, key: &str) -> ChannelClaimTicket {
        ticket_for_action(store, key, "moderation.lockdown").await
    }

    async fn ticket_for_action(
        store: &ChannelModerationStore,
        key: &str,
        action: &str,
    ) -> ChannelClaimTicket {
        match store.claim("g1", key, action, "hash", "now").await.unwrap() {
            ChannelClaim::Claimed { ticket } => ticket,
            other => panic!("expected winning claim, got {other:?}"),
        }
    }

    fn finish_row(key: &str, outcome: &str) -> ChannelAuditRow {
        let mut row = audit_row(key, "moderation.lockdown", outcome);
        row.guild_id = "g1".to_owned();
        row.idempotency_key = key.to_owned();
        row.channel_id = Some("c1".to_owned());
        row
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn channel_lane_blocks_distinct_keys_until_safe_release_or_finish() {
        let store = test_store().await;
        let first = ticket(&store, "first").await;
        let second = ticket(&store, "second").await;
        assert!(store.claim_channel(&first, "c1").await.unwrap());
        assert!(!store.claim_channel(&second, "c1").await.unwrap());
        assert!(store.claim_channel(&second, "c2").await.unwrap());
        // Ambiguity simply retains first's claim/lane: a new key cannot act.
        assert!(!store.claim_channel(&second, "c1").await.unwrap());
        assert!(store.release(&first).await.unwrap());
        assert!(!store.claim_channel(&first, "c1").await.unwrap());
        assert!(store.claim_channel(&second, "c1").await.unwrap());
        let row = finish_row("second", "locked_down");
        assert!(store
            .finish(&second, &row, "{\"text\":\"done\"}", None)
            .await
            .unwrap());
        assert!(!store
            .finish(&second, &row, "overwrite", None)
            .await
            .unwrap());
        assert!(!store.release(&second).await.unwrap());
        assert!(!store.claim_channel(&second, "c1").await.unwrap());
        let third = ticket(&store, "third").await;
        assert!(store.claim_channel(&third, "c1").await.unwrap());
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(audits, 1);
        assert_eq!(
            store
                .claim("g1", "second", "moderation.lockdown", "hash", "now")
                .await
                .unwrap(),
            ChannelClaim::Replayed {
                outcome: "locked_down".to_owned(),
                result_json: "{\"text\":\"done\"}".to_owned()
            }
        );
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn finish_rolls_back_completion_and_lane_release_when_audit_fails() {
        let store = test_store().await;
        let winner = ticket(&store, "finish").await;
        assert!(store.claim_channel(&winner, "c1").await.unwrap());
        sqlx::query("ALTER TABLE moderation_audit ADD CONSTRAINT fail_audit CHECK (outcome <> 'locked_down')")
            .execute(store.pool()).await.unwrap();
        let row = finish_row("finish", "locked_down");
        assert!(store.finish(&winner, &row, "saved", None).await.is_err());
        assert_eq!(
            store
                .claim("g1", "finish", "moderation.lockdown", "hash", "now")
                .await
                .unwrap(),
            ChannelClaim::InFlight
        );
        let rival = ticket(&store, "rival").await;
        assert!(!store.claim_channel(&rival, "c1").await.unwrap());
        sqlx::query("ALTER TABLE moderation_audit DROP CONSTRAINT fail_audit")
            .execute(store.pool())
            .await
            .unwrap();
        // Retry only finalization with the original ticket, not the effect.
        assert!(store.finish(&winner, &row, "saved", None).await.unwrap());
        assert!(store.claim_channel(&rival, "c1").await.unwrap());
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn finish_restoration_cleanup_is_atomic_and_generation_fenced() {
        let store = test_store().await;
        let winner = ticket_for_action(&store, "unlock", "moderation.unlock").await;
        assert!(store.claim_channel(&winner, "c1").await.unwrap());
        let rec = store
            .record_lockdown(
                "c1",
                "g1",
                &crate::LockdownSeed {
                    prior_allow: "1024".to_owned(),
                    prior_deny: "8192".to_owned(),
                    prior_exists: true,
                },
                "raid",
                "now",
            )
            .await
            .unwrap();
        let mut stale = rec.clone();
        stale.recovery_generation = "stale".to_owned();
        let mut row = finish_row("unlock", "unlocked");
        row.action = "moderation.unlock".to_owned();
        assert!(!store
            .finish(&winner, &row, "saved", Some(&stale))
            .await
            .unwrap());
        assert_eq!(store.get_lockdown("c1").await.unwrap(), Some(rec.clone()));
        assert_eq!(
            store
                .claim("g1", "unlock", "moderation.unlock", "hash", "now")
                .await
                .unwrap(),
            ChannelClaim::InFlight
        );
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(audits, 0);
        assert!(store
            .finish(&winner, &row, "saved", Some(&rec))
            .await
            .unwrap());
        assert_eq!(store.get_lockdown("c1").await.unwrap(), None);
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn stale_finish_cannot_audit_or_release_a_new_claim_generation() {
        let store = test_store().await;
        let old = ticket(&store, "renewed").await;
        assert!(store.claim_channel(&old, "c1").await.unwrap());
        assert!(store.release(&old).await.unwrap());
        let new = ticket(&store, "renewed").await;
        assert!(store.claim_channel(&new, "c1").await.unwrap());
        let row = finish_row("renewed", "locked_down");
        let mut wrong_action = row.clone();
        wrong_action.action = "moderation.unlock".to_owned();
        assert!(!store
            .finish(&new, &wrong_action, "wrong", None)
            .await
            .unwrap());
        assert!(!store.finish(&old, &row, "old", None).await.unwrap());
        let rival = ticket(&store, "rival").await;
        assert!(!store.claim_channel(&rival, "c1").await.unwrap());
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(audits, 0);
        assert!(store.finish(&new, &row, "new", None).await.unwrap());
        store.cleanup().await;
    }

    fn audit_row(tag: &str, action: &str, outcome: &str) -> ChannelAuditRow {
        ChannelAuditRow {
            request_id: format!("req-{tag}"),
            guild_id: "111111111111111111".to_owned(),
            actor_id: "222222222222222222".to_owned(),
            action: action.to_owned(),
            channel_id: Some("333333333333333333".to_owned()),
            reason: "QA channel proof".to_owned(),
            outcome: outcome.to_owned(),
            idempotency_key: format!("key-{tag}"),
            metadata_json: "{}".to_owned(),
            created_at: "2026-09-30T00:00:00.000Z".to_owned(),
        }
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn lockdown_round_trip_stores_and_restores_exact_masks() {
        let store = test_store().await;
        let channel = "t-lock-roundtrip";
        let rec = store
            .record_lockdown(
                channel,
                "g1",
                &super::super::channel_moderation::LockdownSeed {
                    prior_allow: "1024".to_owned(),
                    prior_deny: "8192".to_owned(),
                    prior_exists: true,
                },
                "raid",
                "2026-09-30T00:00:00.000Z",
            )
            .await
            .expect("records");
        assert_eq!(rec.prior_allow, "1024");
        assert_eq!(rec.prior_deny, "8192");
        assert!(rec.prior_exists);
        assert!(!rec.recovery_generation.is_empty());
        // Unlock restores exactly what was stored.
        let got = store.get_lockdown(channel).await.expect("reads");
        assert_eq!(got, Some(rec.clone()));
        assert_eq!(
            crate::channel_moderation::plan_unlock(got.as_ref()),
            Ok(crate::channel_moderation::UnlockPlan::Restore {
                allow: "1024".to_owned(),
                deny: "8192".to_owned(),
            })
        );
        // Only after the restore lands does the caller clear.
        assert!(store
            .clear_lockdown(channel, &rec.recovery_generation)
            .await
            .expect("clears"));
        assert_eq!(store.get_lockdown(channel).await.expect("reads"), None);
        assert_eq!(
            crate::channel_moderation::plan_unlock(None),
            Err(crate::channel_moderation::UnlockError::NotLocked)
        );
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn repeated_lockdown_preserves_first_masks() {
        // Legacy `recordLockdown` is insert-only: a repeated lockdown refreshes
        // the reason but never replaces the original pre-lock masks with the
        // already-locked masks.
        let store = test_store().await;
        let channel = "t-lock-repeat";
        let first = store
            .record_lockdown(
                channel,
                "g1",
                &super::super::channel_moderation::LockdownSeed {
                    prior_allow: "1024".to_owned(),
                    prior_deny: "8192".to_owned(),
                    prior_exists: true,
                },
                "first",
                "2026-09-30T00:00:00.000Z",
            )
            .await
            .expect("records");
        let second = store
            .record_lockdown(
                channel,
                "g1",
                &super::super::channel_moderation::LockdownSeed {
                    prior_allow: "1024".to_owned(),
                    prior_deny: (8192u64 | 2048).to_string(),
                    prior_exists: true,
                },
                "second",
                "2026-09-30T00:01:00.000Z",
            )
            .await
            .expect("records again");
        assert_eq!(second.prior_allow, "1024");
        assert_eq!(second.prior_deny, "8192");
        assert_eq!(second.reason, "second");
        // A repeated lockdown preserves the original recovery generation
        // alongside the original seed; cleanup fences on it.
        assert!(!first.recovery_generation.is_empty());
        assert_eq!(second.recovery_generation, first.recovery_generation);
        assert!(store
            .clear_lockdown(channel, &second.recovery_generation)
            .await
            .expect("clears"));
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn delayed_unlock_cleanup_cannot_delete_a_later_lockdown() {
        // Two unlock requests with distinct idempotency keys both read the
        // same first-cycle recovery state. The second unlock's delayed
        // cleanup must not delete the new lockdown recorded after the first
        // unlock cleared its own generation. No Discord call is involved:
        // both first-cycle restorations are treated as already successful.
        let store = test_store().await;
        let channel = "t-lock-stale-cleanup";
        let time = "2026-09-30T00:00:00.000Z";
        let first = store
            .record_lockdown(
                channel,
                "g1",
                &super::super::channel_moderation::LockdownSeed {
                    prior_allow: "1024".to_owned(),
                    prior_deny: "8192".to_owned(),
                    prior_exists: true,
                },
                "first cycle",
                time,
            )
            .await
            .expect("records first cycle");
        // Distinct request keys are both granted; claim tokens fence only
        // the request ledger, not this channel's recovery row.
        for key in ["t-unlock-a", "t-unlock-b"] {
            assert!(
                matches!(
                    store
                        .claim("g1", key, "moderation.unlock", key, time)
                        .await
                        .expect("claims"),
                    ChannelClaim::Claimed { .. }
                ),
                "distinct unlock keys are both granted"
            );
        }
        let stale_a = store
            .get_lockdown(channel)
            .await
            .expect("reads")
            .expect("first cycle present");
        let stale_b = store
            .get_lockdown(channel)
            .await
            .expect("reads")
            .expect("first cycle present");
        assert_eq!(stale_a, first);
        assert_eq!(stale_b, first);
        // The first unlock restores its own generation and clears it.
        assert_eq!(
            crate::channel_moderation::plan_unlock(Some(&stale_a)),
            crate::channel_moderation::plan_unlock(Some(&stale_b))
        );
        assert!(store
            .clear_lockdown(channel, &stale_a.recovery_generation)
            .await
            .expect("clears first cycle"));
        let second = store
            .record_lockdown(
                channel,
                "g1",
                &super::super::channel_moderation::LockdownSeed {
                    prior_allow: "4096".to_owned(),
                    prior_deny: "16384".to_owned(),
                    prior_exists: true,
                },
                "new cycle",
                time,
            )
            .await
            .expect("records new cycle");
        assert_ne!(
            second.recovery_generation, first.recovery_generation,
            "a new lockdown cycle mints a new recovery generation"
        );
        // The delayed second cleanup restores only the old record, so it
        // reports stale and the new cycle's seed survives.
        assert!(!store
            .clear_lockdown(channel, &stale_b.recovery_generation)
            .await
            .expect("stale cleanup no-ops"));
        assert_eq!(
            store.get_lockdown(channel).await.expect("reads"),
            Some(second.clone())
        );
        assert!(
            crate::channel_moderation::plan_unlock(Some(&second)).is_ok(),
            "the new cycle still unlocks"
        );
        assert!(store
            .clear_lockdown(channel, &second.recovery_generation)
            .await
            .expect("clears new cycle"));
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn retried_cleanup_after_recreation_reports_stale() {
        // A cleanup whose earlier result was lost must not delete a
        // recreated row even when the masks are identical: generations
        // differ, so the retry no-ops and the live seed survives.
        let store = test_store().await;
        let channel = "t-lock-retry-cleanup";
        let seed = super::super::channel_moderation::LockdownSeed {
            prior_allow: "1024".to_owned(),
            prior_deny: "8192".to_owned(),
            prior_exists: true,
        };
        let time = "2026-09-30T00:00:00.000Z";
        let first = store
            .record_lockdown(channel, "g1", &seed, "first", time)
            .await
            .expect("records");
        assert!(store
            .clear_lockdown(channel, &first.recovery_generation)
            .await
            .expect("clears"));
        let second = store
            .record_lockdown(channel, "g1", &seed, "recreated", time)
            .await
            .expect("recreates with identical masks");
        assert_ne!(
            second.recovery_generation, first.recovery_generation,
            "recreation mints a new recovery generation"
        );
        // The lost-result retry of the first cleanup reports stale.
        assert!(!store
            .clear_lockdown(channel, &first.recovery_generation)
            .await
            .expect("retry no-ops"));
        assert_eq!(
            store.get_lockdown(channel).await.expect("reads"),
            Some(second.clone())
        );
        // A wrong generation on a live row deletes nothing either.
        assert!(!store
            .clear_lockdown(channel, "no-such-generation")
            .await
            .expect("unknown generation no-ops"));
        assert_eq!(
            store.get_lockdown(channel).await.expect("reads"),
            Some(second.clone())
        );
        assert!(store
            .clear_lockdown(channel, &second.recovery_generation)
            .await
            .expect("clears"));
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn claim_complete_replay_refuses_reuse() {
        let store = test_store().await;
        let (guild, key) = ("g-claim", "t-claim-key");
        let ticket = winning_ticket(
            store
                .claim(
                    guild,
                    key,
                    "moderation.lockdown",
                    "hash-a",
                    "2026-09-30T00:00:00.000Z",
                )
                .await
                .expect("claims"),
        );
        // Same key, same content, still uncertain: in-flight, not a replay.
        assert_eq!(
            store
                .claim(
                    guild,
                    key,
                    "moderation.lockdown",
                    "hash-a",
                    "2026-09-30T00:00:01.000Z"
                )
                .await
                .expect("claims"),
            ChannelClaim::InFlight
        );
        assert!(store
            .complete(
                &ticket,
                "locked_down",
                r#"{"outcome":"locked_down"}"#,
                "2026-09-30T00:00:02.000Z",
            )
            .await
            .expect("completes"));
        // Completed key replays the stored result instead of acting again.
        assert_eq!(
            store
                .claim(
                    guild,
                    key,
                    "moderation.lockdown",
                    "hash-a",
                    "2026-09-30T00:00:03.000Z"
                )
                .await
                .expect("claims"),
            ChannelClaim::Replayed {
                outcome: "locked_down".to_owned(),
                result_json: r#"{"outcome":"locked_down"}"#.to_owned(),
            }
        );
        // Same key, different content: caller bug, refused as mismatch.
        assert_eq!(
            store
                .claim(
                    guild,
                    key,
                    "moderation.unlock",
                    "hash-b",
                    "2026-09-30T00:00:04.000Z"
                )
                .await
                .expect("claims"),
            ChannelClaim::Mismatch
        );
        store.cleanup().await;
    }

    fn winning_ticket(claim: ChannelClaim) -> ChannelClaimTicket {
        let ChannelClaim::Claimed { ticket } = claim else {
            panic!("expected a winning claim, got {claim:?}");
        };
        ticket
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn release_returns_key_for_a_real_retry() {
        let store = test_store().await;
        let (guild, key) = ("g-release", "t-release-key");
        let time = "2026-09-30T00:00:00.000Z";
        let first = winning_ticket(
            store
                .claim(guild, key, "moderation.purge", "hash", time)
                .await
                .unwrap(),
        );
        // A proven no-op may release; the retry receives a fresh generation.
        assert!(store.release(&first).await.unwrap());
        let second = winning_ticket(
            store
                .claim(guild, key, "moderation.purge", "hash", time)
                .await
                .unwrap(),
        );
        assert_ne!(first.claim_token, second.claim_token);
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn stale_claim_writes_cannot_affect_a_new_winner() {
        let store = test_store().await;
        let (guild, key) = ("g-stale", "key-stale");
        let time = "2026-09-30T00:00:00.000Z";
        let first = winning_ticket(
            store
                .claim(guild, key, "moderation.purge", "hash", time)
                .await
                .unwrap(),
        );
        assert!(store.release(&first).await.unwrap());
        let second = winning_ticket(
            store
                .claim(guild, key, "moderation.purge", "hash", time)
                .await
                .unwrap(),
        );
        assert_ne!(first.claim_token, second.claim_token);
        // Delayed release AND completion from A must leave B in flight.
        assert!(!store.release(&first).await.unwrap());
        assert!(!store.complete(&first, "stale", "{}", time).await.unwrap());
        assert_eq!(
            store
                .claim(guild, key, "moderation.purge", "hash", time)
                .await
                .unwrap(),
            ChannelClaim::InFlight
        );
        assert!(store
            .complete(&second, "purged", r#"{"affected":3}"#, time)
            .await
            .unwrap());
        // Completed results are immutable, even for the current ticket.
        assert!(!store
            .complete(&second, "changed", "{}", time)
            .await
            .unwrap());
        assert!(!store.complete(&first, "stale", "{}", time).await.unwrap());
        assert!(!store.release(&first).await.unwrap());
        assert!(!store.release(&second).await.unwrap());
        assert_eq!(
            store
                .claim(guild, key, "moderation.purge", "hash", time)
                .await
                .unwrap(),
            ChannelClaim::Replayed {
                outcome: "purged".to_owned(),
                result_json: r#"{"affected":3}"#.to_owned()
            }
        );
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn action_only_reuse_is_mismatch_in_flight_and_done() {
        let store = test_store().await;
        let (guild, key) = ("g-action", "key-action");
        let time = "2026-09-30T00:00:00.000Z";
        let ticket = winning_ticket(
            store
                .claim(guild, key, "moderation.purge", "hash", time)
                .await
                .unwrap(),
        );
        for completed in [false, true] {
            if completed {
                assert!(store.complete(&ticket, "purged", "{}", time).await.unwrap());
            }
            for (action, hash) in [
                ("moderation.unlock", "hash"),
                ("moderation.purge", "different-hash"),
            ] {
                assert_eq!(
                    store.claim(guild, key, action, hash, time).await.unwrap(),
                    ChannelClaim::Mismatch
                );
            }
        }
        store.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn audit_rows_are_written_once() {
        let store = test_store().await;
        let row = audit_row("audit-once", "moderation.lockdown", "locked_down");
        store.record_audit(&row).await.expect("writes");
        // A retry replays; the audit write is DO NOTHING on request_id.
        let mut changed = row.clone();
        changed.outcome = "tampered".to_owned();
        store.record_audit(&changed).await.expect("no-ops");
        let outcome: String =
            sqlx::query_scalar("SELECT outcome FROM moderation_audit WHERE request_id = $1")
                .bind(&row.request_id)
                .fetch_one(store.pool())
                .await
                .expect("reads");
        assert_eq!(outcome, "locked_down");
        for (tag, action, outcome) in [
            ("purge", "moderation.purge", "purged"),
            ("slowmode", "moderation.slowmode", "slowmode_updated"),
            ("unlock", "moderation.unlock", "unlocked"),
            ("refused", "moderation.unlock", "refused"),
        ] {
            store
                .record_audit(&audit_row(tag, action, outcome))
                .await
                .expect("writes verb audit");
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_audit")
            .fetch_one(store.pool())
            .await
            .expect("counts isolated audit rows");
        assert_eq!(count, 5);
        store.cleanup().await;
    }
}
