//! PostgreSQL adapter for one preview attempt, with no Discord I/O.

use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::oneshot;
use tokio::time::{timeout_at, Instant};

use crate::automod_preview_attempt::PreviewAttemptStore;
use crate::automod_preview_audit::{
    insert_preview_audit, load_preview_audit, lock_preview_attempt, parse_preview_audit_id,
    PreviewAuditRow,
};
use crate::custom_commands::AuditRecord;

/// Holds the transaction only on a locked miss, bounded by the caller deadline.
pub struct SqlPreviewStore<'a> {
    pool: &'a PgPool,
    deadline: Instant,
    claim: Option<PreviewClaim>,
}

struct PreviewClaim {
    audit_id: String,
    insert: oneshot::Sender<PreviewInsert>,
    committed: oneshot::Receiver<Result<String, sqlx::Error>>,
}

struct PreviewInsert {
    record: AuditRecord,
    audit_id: String,
    receipt_iso: String,
}

impl PreviewClaim {
    fn new(audit_id: String, tx: Transaction<'static, Postgres>, deadline: Instant) -> Self {
        let (insert, request) = oneshot::channel();
        let (mut reply, committed) = oneshot::channel();
        // The task keeps ownership through commit: the caller cannot retain the
        // connection by leaving the claim or pending commit future unpolled.
        tokio::spawn(async move {
            let attempt = timeout_at(deadline, async {
                let request = request.await.map_err(|_| deadline_error())?;
                commit_preview(tx, request, deadline).await
            });
            tokio::select! {
                biased;
                () = reply.closed() => {}
                result = attempt => {
                    let _ = reply.send(result.unwrap_or_else(|_| Err(deadline_error())));
                }
            }
        });
        Self {
            audit_id,
            insert,
            committed,
        }
    }
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

async fn commit_preview(
    mut tx: Transaction<'static, Postgres>,
    insert: PreviewInsert,
    deadline: Instant,
) -> Result<String, sqlx::Error> {
    if Instant::now() >= deadline {
        return Err(deadline_error());
    }
    let stored = insert_preview_audit(
        &mut *tx,
        &insert.record,
        &insert.audit_id,
        &insert.receipt_iso,
    )
    .await?;
    if Instant::now() >= deadline {
        return Err(deadline_error());
    }
    tx.commit().await?;
    Ok(stored)
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
            // Server-side backstop if the caller's rollback cannot reach PostgreSQL.
            sqlx::query("SET LOCAL idle_in_transaction_session_timeout = '2s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL lock_timeout = '750ms'")
                .execute(&mut *tx)
                .await?;
            lock_preview_attempt(&mut tx, interaction_id).await?;
            // A separate statement gets a fresh snapshot after any lock wait.
            let row = load_preview_audit(&mut *tx, audit_id).await?;
            if Instant::now() >= self.deadline {
                return Err(deadline_error());
            }
            if row.is_none() {
                self.claim = Some(PreviewClaim::new(audit_id.to_owned(), tx, self.deadline));
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
        let claim = self
            .claim
            .ok_or_else(|| sqlx::Error::Protocol("missing preview claim".into()))?;
        if claim.audit_id != audit_id {
            return Err(sqlx::Error::Protocol("preview claim id mismatch".into()));
        }
        if Instant::now() >= self.deadline {
            return Err(deadline_error());
        }
        claim
            .insert
            .send(PreviewInsert {
                record,
                audit_id: claim.audit_id,
                receipt_iso: receipt_iso.to_owned(),
            })
            .map_err(|_| deadline_error())?;
        let result = timeout_at(self.deadline, claim.committed)
            .await
            .map_err(|_| deadline_error())?
            .map_err(|_| deadline_error())?;
        if Instant::now() >= self.deadline {
            return Err(deadline_error());
        }
        result
    }
}
