//! Bounded warning history projection for a future authorized caller.

use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder, Row};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use super::{db_error, PgMemberModerationStore, StoreError};
use crate::lfg_list::canonical_snowflake;

const PAGE_SIZE: usize = 5;
const READ_BUDGET: Duration = Duration::from_secs(2);

/// The display fields of a retained warning.
#[derive(Debug, PartialEq, Eq)]
pub struct WarningHistoryEntry {
    pub reference: String,
    pub recorded_at_utc: String,
    pub recorder_id: Option<u64>,
}

pub struct WarningHistoryPage {
    pub entries: Vec<WarningHistoryEntry>,
    pub next: Option<WarningHistoryCursor>,
}

/// Server-side continuation state. Contains raw ordering keys and must stay
/// in memory behind the private interaction's opaque handle.
pub struct WarningHistoryCursor {
    guild: String,
    target: String,
    high_water: Key,
    last_shown: Key,
}

#[derive(Clone)]
struct Key {
    created_at: OffsetDateTime,
    id: String,
}

impl PgMemberModerationStore {
    /// Read one page after the caller has authorized private staff access.
    /// The caller owns authorization, the interaction deadline and session limits.
    pub async fn warning_history(
        &self,
        guild: &str,
        target: &str,
        cursor: Option<&WarningHistoryCursor>,
    ) -> Result<WarningHistoryPage, StoreError> {
        self.ensure_guild(guild)?;
        if !canonical_snowflake(guild) || !canonical_snowflake(target) {
            return Err(StoreError::new("invalid warning history scope"));
        }
        if cursor.is_some_and(|cursor| cursor.guild != guild || cursor.target != target) {
            return Err(StoreError::new("warning history cursor scope mismatch"));
        }
        tokio::time::timeout(READ_BUDGET, self.read_warning_history(target, cursor))
            .await
            .map_err(|_| StoreError::new("warning history read timed out"))?
    }

    async fn read_warning_history(
        &self,
        target: &str,
        cursor: Option<&WarningHistoryCursor>,
    ) -> Result<WarningHistoryPage, StoreError> {
        let mut query = QueryBuilder::new("");
        push_history_query(&mut query, &self.guild_id, target, cursor);
        let mut tx = self
            .pool
            .begin_with("BEGIN READ ONLY")
            .await
            .map_err(db_error)?;
        // The server also bounds work if the client stops waiting.
        sqlx::query("SET LOCAL statement_timeout = '2s'")
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let rows = query.build().fetch_all(&mut *tx).await.map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;

        let has_more = rows.len() > PAGE_SIZE;
        let mut entries = Vec::with_capacity(rows.len().min(PAGE_SIZE));
        let mut high_water = cursor.map(|cursor| cursor.high_water.clone());
        let mut last_shown = None;
        for (index, row) in rows.iter().enumerate() {
            let key = Key {
                id: row.try_get("stored_id").map_err(db_error)?,
                created_at: row.try_get("recorded_at").map_err(db_error)?,
            };
            if index == PAGE_SIZE {
                break;
            }
            let mut digest = Sha256::new();
            digest.update(b"warning-history-v1\0");
            digest.update(self.guild_id.as_bytes());
            digest.update(b"\0");
            digest.update(key.id.as_bytes());
            let recorder: Option<&str> = row.try_get("recorder").map_err(db_error)?;
            entries.push(WarningHistoryEntry {
                reference: format!("W-{}", hex::encode(digest.finalize())),
                recorded_at_utc: key
                    .created_at
                    .format(&Rfc3339)
                    .map_err(|_| StoreError::new("invalid warning history timestamp"))?,
                recorder_id: recorder
                    .filter(|id| canonical_snowflake(id))
                    .and_then(|id| id.parse().ok()),
            });
            if has_more {
                high_water.get_or_insert_with(|| key.clone());
                last_shown = Some(key);
            }
        }
        let next =
            high_water
                .zip(last_shown)
                .map(|(high_water, last_shown)| WarningHistoryCursor {
                    guild: self.guild_id.clone(),
                    target: target.to_owned(),
                    high_water,
                    last_shown,
                });
        Ok(WarningHistoryPage { entries, next })
    }
}

fn push_history_query(
    query: &mut QueryBuilder<Postgres>,
    guild: &str,
    target: &str,
    cursor: Option<&WarningHistoryCursor>,
) {
    query
        .push(
            "SELECT CASE WHEN octet_length(id) <= 200 THEN id END AS stored_id,
                  CASE WHEN created_at >= TIMESTAMPTZ '0001-01-01 00:00:00+00'
                        AND created_at < TIMESTAMPTZ '10000-01-01 00:00:00+00'
                       THEN created_at END AS recorded_at,
                  CASE WHEN row_number() OVER history_order <= ",
        )
        .push_bind(PAGE_SIZE as i64)
        .push(
            " AND octet_length(actor_id) <= 20 THEN actor_id END AS recorder
           FROM (
               SELECT id, created_at, actor_id FROM moderation_warnings WHERE guild_id = ",
        )
        .push_bind(guild)
        .push(" AND user_id = ")
        .push_bind(target);
    if let Some(cursor) = cursor {
        push_bound(query, " <= ", &cursor.high_water);
        push_bound(query, " < ", &cursor.last_shown);
    }
    query
        .push(r#" ORDER BY created_at DESC, id COLLATE "C" DESC LIMIT "#)
        .push_bind((PAGE_SIZE + 1) as i64)
        .push(
            r#") AS bounded_page
            WINDOW history_order AS (ORDER BY created_at DESC, id COLLATE "C" DESC)
            ORDER BY created_at DESC, id COLLATE "C" DESC"#,
        );
}

fn push_bound(query: &mut QueryBuilder<Postgres>, operator: &'static str, key: &Key) {
    query
        .push(r#" AND (created_at, id COLLATE "C")"#)
        .push(operator)
        .push("(")
        .push_bind(key.created_at)
        .push(", ")
        .push_bind(&key.id)
        .push(r#"::text COLLATE "C")"#);
}

#[cfg(test)]
mod continuation;
#[cfg(test)]
mod tests;
