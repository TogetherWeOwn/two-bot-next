//! Guild-scoped capacity admission for automation definition writes.

use sqlx::{Postgres, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationQuota {
    Schedules,
    Feeds,
    OpenLfgPosts,
}

impl AutomationQuota {
    pub const fn limit(self) -> i64 {
        match self {
            Self::Schedules | Self::Feeds => 25,
            Self::OpenLfgPosts => 20,
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Schedules => "schedules",
            Self::Feeds => "feeds",
            Self::OpenLfgPosts => "open-lfg",
        }
    }
}

impl std::fmt::Display for AutomationQuota {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Schedules => "This server has reached its limit of 25 schedules. Remove a schedule with /schedule-remove before creating another.",
            Self::Feeds => "This server has reached its limit of 25 feeds. Remove a feed with /feed-remove before creating another.",
            Self::OpenLfgPosts => "This server has reached its limit of 20 open LFG posts. Close a post with /lfg-close before creating another.",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QuotaWriteError {
    #[error("{0}")]
    Capacity(AutomationQuota),
    #[error("Automation database operation failed")]
    Database(#[from] sqlx::Error),
}

/// Count and write on this transaction only, after this separate statement.
/// READ COMMITTED gives the count a fresh snapshot after the previous holder
/// commits; transaction end releases the lock, including on rollback.
/// See https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS
/// and https://www.postgresql.org/docs/current/transaction-iso.html#XACT-READ-COMMITTED.
pub(crate) async fn lock_quota(
    tx: &mut Transaction<'_, Postgres>,
    guild_id: &str,
    quota: AutomationQuota,
) -> Result<(), sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("automation-quota:{}:{guild_id}", quota.key()))
        .execute(&mut **tx)
        .await?;
    Ok(())
}
