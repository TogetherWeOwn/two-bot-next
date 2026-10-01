//! sqlx [`InviteSnapshotStore`]: `invite_snapshots` persistence.
//!
//! Ports the pipeline-owned mutex map (`PipelineSnapshots`) to Postgres:
//! `store_all` upserts each counter row, `delete_missing` removes codes that
//! vanished from the live listing, `load` reads the guild snapshot back.
//! Snowflakes persist as decimal TEXT, matching the funnel tables.

use std::collections::HashSet;

use sqlx::{Pool, Postgres};
use two_bot_core::{InviteSnapshotStore, InviteState, Snowflake};

/// Postgres-backed [`InviteSnapshotStore`]. Same sync→async bridge as
/// [`PgFunnelStore`](crate::PgFunnelStore): methods dispatch with
/// `block_in_place` and must not run on a `current_thread` runtime.
#[derive(Debug, Clone)]
pub struct PgInviteSnapshots {
    pool: Pool<Postgres>,
    handle: tokio::runtime::Handle,
}

impl PgInviteSnapshots {
    /// Build over `pool`. Must be called from inside a tokio runtime.
    #[must_use]
    pub fn new(pool: Pool<Postgres>) -> Self {
        Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        }
    }

    /// Borrow the pool for direct queries.
    #[must_use]
    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    /// Run an async query from the sync trait methods.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| self.handle.block_on(fut))
    }
}

/// Legacy `updated_at` shape: millis precision with a `Z` suffix.
/// A snapshot write is always "now"; the exact instant is bookkeeping, not
/// funnel data, so second precision is plenty and avoids a time dependency.
fn snapshot_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_millis().min(i64::MAX as u128)) as i64)
        .unwrap_or(0);
    two_bot_core::format_iso_millis(ms)
}

impl InviteSnapshotStore for PgInviteSnapshots {
    /// # Panics
    ///
    /// Propagates a database failure as a panic: the sync
    /// [`InviteSnapshotStore`] seam has no fallible channel, matching
    /// [`PgFunnelStore`](crate::PgFunnelStore) behaviour.
    fn load(&self, guild_id: Snowflake) -> Vec<InviteState> {
        let guild_id = guild_id.to_string();
        self.block_on(async {
            sqlx::query_as::<_, (String, i32, Option<String>, Option<String>)>(
                "SELECT code, uses, inviter_id, channel_id FROM invite_snapshots WHERE guild_id = $1",
            )
            .bind(&guild_id)
            .fetch_all(&self.pool)
            .await
        })
        .expect("invite snapshot load failed")
        .into_iter()
        .map(|(code, uses, inviter_id, channel_id)| InviteState {
            code,
            uses: u64::try_from(uses).unwrap_or(0),
            inviter_id: inviter_id.and_then(|s| s.parse().ok()),
            channel_id: channel_id.and_then(|s| s.parse().ok()),
        })
        .collect()
    }

    /// # Panics
    ///
    /// See [`InviteSnapshotStore::load`].
    fn store_all(&self, guild_id: Snowflake, states: &[InviteState]) {
        let guild_id = guild_id.to_string();
        let stamp = snapshot_stamp();
        self.block_on(async {
            let mut tx = self.pool.begin().await?;
            for s in states {
                sqlx::query(
                    "INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, channel_id, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
                     ON CONFLICT (guild_id, code) DO UPDATE SET
                       uses = excluded.uses, inviter_id = excluded.inviter_id,
                       channel_id = excluded.channel_id, updated_at = excluded.updated_at",
                )
                .bind(&guild_id)
                .bind(&s.code)
                .bind(i32::try_from(s.uses).expect("invite uses exceeds Postgres INTEGER range"))
                .bind(s.inviter_id.map(|id| id.to_string()))
                .bind(s.channel_id.map(|id| id.to_string()))
                .bind(&stamp)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await
        })
        .expect("invite snapshot store_all failed");
    }

    /// # Panics
    ///
    /// See [`InviteSnapshotStore::load`].
    fn delete_missing(&self, guild_id: Snowflake, live_codes: &HashSet<String>) {
        let guild_id = guild_id.to_string();
        let live: Vec<String> = live_codes.iter().cloned().collect();
        self.block_on(async {
            // `<> ALL(...)` with an empty array is true for every row, which
            // is exactly the "no invites live" case: clear the snapshot.
            sqlx::query("DELETE FROM invite_snapshots WHERE guild_id = $1 AND code <> ALL($2)")
                .bind(&guild_id)
                .bind(&live)
                .execute(&self.pool)
                .await
        })
        .expect("invite snapshot delete_missing failed");
    }
}
