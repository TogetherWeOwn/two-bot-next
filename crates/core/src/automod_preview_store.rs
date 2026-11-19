//! PostgreSQL adapter for one preview attempt, with no Discord I/O.

use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};
use tokio::time::{timeout_at, Instant};

use crate::automod_preview_attempt::PreviewAttemptStore;
use crate::automod_preview_audit::{
    insert_preview_audit, load_preview_audit, lock_preview_attempt, parse_preview_audit_id,
    PreviewAuditRow,
};
use crate::custom_commands::AuditRecord;

const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Holds the transaction only on a locked miss, bounded by the caller deadline.
pub struct SqlPreviewStore<'a> {
    pool: &'a PgPool,
    deadline: Instant,
    claim: Option<(String, Transaction<'static, Postgres>)>,
}

impl<'a> SqlPreviewStore<'a> {
    /// Construct without acquiring a connection. The caller supplies one
    /// database deadline, leaving time for the interaction reply.
    #[must_use]
    pub fn new(pool: &'a PgPool, deadline: Instant) -> Self {
        Self {
            pool,
            deadline,
            claim: None,
        }
    }
}

fn deadline_error() -> sqlx::Error {
    std::io::Error::from(std::io::ErrorKind::TimedOut).into()
}

async fn set_idle_timeout(
    tx: &mut Transaction<'_, Postgres>,
    timeout: Duration,
) -> Result<(), sqlx::Error> {
    let millis = timeout.as_millis();
    // PostgreSQL interprets zero as disabled, not expired.
    if millis == 0 {
        return Err(deadline_error());
    }
    sqlx::query("SELECT set_config('idle_in_transaction_session_timeout', $1, true)")
        .bind(format!("{millis}ms"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl PreviewAttemptStore for SqlPreviewStore<'_> {
    type Error = sqlx::Error;

    async fn claim_and_read(
        &mut self,
        audit_id: &str,
    ) -> Result<Option<PreviewAuditRow>, Self::Error> {
        let interaction_id = parse_preview_audit_id(audit_id)
            .ok_or_else(|| sqlx::Error::Protocol("invalid preview audit id".into()))?;
        if self.claim.is_some() {
            return Err(sqlx::Error::Protocol("preview claim already held".into()));
        }
        if Instant::now() >= self.deadline {
            return Err(deadline_error());
        }
        timeout_at(self.deadline, async {
            let mut tx = self.pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL statement_timeout = '1s'")
                .execute(&mut *tx)
                .await?;
            set_idle_timeout(&mut tx, MAX_IDLE_TIMEOUT).await?;
            sqlx::query("SET LOCAL lock_timeout = '750ms'")
                .execute(&mut *tx)
                .await?;
            lock_preview_attempt(&mut tx, interaction_id).await?;
            // A separate statement gets a fresh snapshot after any lock wait.
            let row = load_preview_audit(&mut *tx, audit_id).await?;
            if row.is_none() {
                let remaining = self.deadline.saturating_duration_since(Instant::now());
                if remaining < MAX_IDLE_TIMEOUT {
                    // Expire an idle claim even if the caller never polls insert or drops the store.
                    set_idle_timeout(&mut tx, remaining).await?;
                }
            }
            if Instant::now() >= self.deadline {
                return Err(deadline_error());
            }
            if row.is_none() {
                self.claim = Some((audit_id.to_owned(), tx));
            }
            Ok(row)
        })
        .await
        .map_err(|_| deadline_error())?
    }

    async fn insert_and_commit(
        self,
        record: AuditRecord,
        audit_id: &str,
        receipt_iso: &str,
    ) -> Result<String, Self::Error> {
        let Some((claimed_id, mut tx)) = self.claim else {
            return Err(sqlx::Error::Protocol("missing preview claim".into()));
        };
        if claimed_id != audit_id {
            return Err(sqlx::Error::Protocol("preview claim id mismatch".into()));
        }
        if Instant::now() >= self.deadline {
            return Err(deadline_error());
        }
        timeout_at(self.deadline, async {
            let stored = insert_preview_audit(&mut *tx, &record, &claimed_id, receipt_iso).await?;
            if Instant::now() >= self.deadline {
                return Err(deadline_error());
            }
            tx.commit().await?;
            if Instant::now() >= self.deadline {
                return Err(deadline_error());
            }
            Ok(stored)
        })
        .await
        .map_err(|_| deadline_error())?
    }
}
