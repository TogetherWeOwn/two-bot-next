//! PostgreSQL adapter for one preview attempt, with no Discord I/O.

use sqlx::{pool::PoolConnection, Connection, PgPool, Postgres, Transaction};
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
    receipt_iso: String,
}

type ClaimRead = Result<Option<PreviewAuditRow>, sqlx::Error>;

impl PreviewClaim {
    fn start(
        audit_id: &str,
        interaction_id: &str,
        conn: PoolConnection<Postgres>,
        deadline: Instant,
    ) -> (Self, oneshot::Receiver<ClaimRead>) {
        let (ready, read) = oneshot::channel();
        let (insert, request) = oneshot::channel();
        let (mut reply, committed) = oneshot::channel();
        let id = audit_id.to_owned();
        let claim = Self {
            audit_id: id.clone(),
            insert,
            committed,
        };
        let interaction_id = interaction_id.to_owned();
        // Keep the connection and its borrowing transaction in the same task.
        tokio::spawn(async move {
            let attempt = timeout_at(deadline, async move {
                let mut conn = BeginGuard::new(conn);
                let prepared = prepare_claim(&mut conn, &id, &interaction_id, deadline).await;
                match prepared {
                    Ok((tx, None)) => {
                        ready.send(Ok(None)).map_err(|_| deadline_error())?;
                        let request = request.await.map_err(|_| deadline_error())?;
                        commit_preview(tx, &id, request, deadline).await
                    }
                    result => {
                        let _ = ready.send(result.map(|(_, row)| row));
                        Err(deadline_error())
                    }
                }
            });
            tokio::select! {
                biased;
                () = reply.closed() => {}
                result = attempt => {
                    let _ = reply.send(result.unwrap_or_else(|_| Err(deadline_error())));
                }
            }
        });
        (claim, read)
    }
}

struct BeginGuard {
    conn: PoolConnection<Postgres>,
    pending: bool,
}

impl BeginGuard {
    fn new(conn: PoolConnection<Postgres>) -> Self {
        Self {
            conn,
            pending: false,
        }
    }

    async fn begin(
        &mut self,
        statement: &'static str,
    ) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
        self.pending = true;
        let tx = self.conn.begin_with(statement).await?;
        self.pending = false;
        Ok(tx)
    }
}

impl Drop for BeginGuard {
    fn drop(&mut self) {
        if self.pending {
            // Until SQLx acknowledges BEGIN, it may not know to queue rollback.
            self.conn.close_on_drop();
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

fn check_deadline(deadline: Instant) -> Result<(), sqlx::Error> {
    (Instant::now() < deadline)
        .then_some(())
        .ok_or_else(deadline_error)
}

// Server-side backstops are installed as part of guarded startup.
const BEGIN_PREVIEW: &str = "BEGIN ISOLATION LEVEL READ COMMITTED;
    SET LOCAL statement_timeout = '1s';
    SET LOCAL idle_in_transaction_session_timeout = '2s';
    SET LOCAL lock_timeout = '750ms'";

async fn prepare_claim<'a>(
    conn: &'a mut BeginGuard,
    audit_id: &str,
    interaction_id: &str,
    deadline: Instant,
) -> Result<(Transaction<'a, Postgres>, Option<PreviewAuditRow>), sqlx::Error> {
    check_deadline(deadline)?;
    let mut tx = conn.begin(BEGIN_PREVIEW).await?;
    let row = lock_and_read(&mut tx, audit_id, interaction_id, deadline).await?;
    Ok((tx, row))
}

async fn lock_and_read(
    tx: &mut Transaction<'_, Postgres>,
    audit_id: &str,
    interaction_id: &str,
    deadline: Instant,
) -> ClaimRead {
    // A ready setup response can be polled before the timeout on the same wake.
    check_deadline(deadline)?;
    lock_preview_attempt(tx, interaction_id).await?;
    check_deadline(deadline)?;
    // A separate statement gets a fresh snapshot after any lock wait.
    load_preview_audit(&mut **tx, audit_id).await
}

async fn commit_preview(
    mut tx: Transaction<'_, Postgres>,
    audit_id: &str,
    insert: PreviewInsert,
    deadline: Instant,
) -> Result<String, sqlx::Error> {
    check_deadline(deadline)?;
    let stored =
        insert_preview_audit(&mut *tx, &insert.record, audit_id, &insert.receipt_iso).await?;
    check_deadline(deadline)?;
    tx.commit().await?;
    Ok(stored)
}

impl PreviewAttemptStore for SqlPreviewStore<'_> {
    type Error = sqlx::Error;

    async fn claim_and_read(&mut self, audit_id: &str) -> ClaimRead {
        let interaction_id = parse_preview_audit_id(audit_id)
            .ok_or_else(|| sqlx::Error::Protocol("invalid preview audit id".into()))?;
        if self.claim.is_some() {
            return Err(sqlx::Error::Protocol("preview claim already held".into()));
        }
        check_deadline(self.deadline)?;
        let conn = timeout_at(self.deadline, self.pool.acquire())
            .await
            .map_err(|_| deadline_error())??;
        let (claim, read) = PreviewClaim::start(audit_id, interaction_id, conn, self.deadline);
        let row = timeout_at(self.deadline, read)
            .await
            .map_err(|_| deadline_error())?
            .map_err(|_| deadline_error())??;
        check_deadline(self.deadline)?;
        if row.is_none() {
            self.claim = Some(claim);
        }
        Ok(row)
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
        check_deadline(self.deadline)?;
        claim
            .insert
            .send(PreviewInsert {
                record,
                receipt_iso: receipt_iso.to_owned(),
            })
            .map_err(|_| deadline_error())?;
        let result = timeout_at(self.deadline, claim.committed)
            .await
            .map_err(|_| deadline_error())?
            .map_err(|_| deadline_error())?;
        check_deadline(self.deadline)?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{migrate::Migrator, postgres::PgPoolOptions};
    use two_bot_testsupport::TestDatabase;

    #[tokio::test]
    async fn preview_startup_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let url = std::env::var("TWO_TEST_DATABASE_URL")?;
        let db = TestDatabase::create(&url, &Migrator::DEFAULT).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(db.pool().connect_options().as_ref().clone());
        let proof = tokio::spawn(check_startup(pool.clone())).await;
        pool.close().await;
        db.close().await?;
        proof?.map_err(Into::into)
    }

    async fn check_startup(pool: PgPool) -> Result<(), sqlx::Error> {
        let pid = "SELECT pg_backend_pid()";
        let mut conn = BeginGuard::new(pool.acquire().await?);
        let old: i32 = sqlx::query_scalar(pid).fetch_one(&mut *conn.conn).await?;
        // Delay the startup acknowledgment on the disposable database.
        let mut begin = Box::pin(conn.begin("BEGIN; SELECT pg_sleep(1)"));
        assert!(futures_util::poll!(&mut begin).is_pending());
        drop(begin);
        drop(conn);
        let mut conn = BeginGuard::new(pool.acquire().await?);
        let next: i32 = sqlx::query_scalar(pid).fetch_one(&mut *conn.conn).await?;
        assert_ne!(old, next, "unconfirmed BEGIN reused a connection");
        let mut tx = conn.begin(BEGIN_PREVIEW).await?;
        let id = "automod-preview:startup-999";
        let error = lock_and_read(&mut tx, id, "startup-999", Instant::now())
            .await
            .expect_err("an expired attempt must stop after setup");
        assert_eq!(error.to_string(), deadline_error().to_string());
        let locks: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
        )
        .fetch_one(&mut *tx)
        .await?;
        assert_eq!(locks, 0, "expired attempt acquired a lock");
        tx.rollback().await
    }
}
