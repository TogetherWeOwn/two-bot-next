//! sqlx store for guild settings (TOG-10096, S6 slice of TOG-9811).
//!
//! Ports the database half of legacy `src/core/settings.ts`
//! (`SettingsStore.load`, `refreshIfChanged`, `set` + audit row, all against
//! `guild_settings` / `guild_settings_audit`, migration `0330`). All
//! classification, validation, caching and poll-change partitioning live in
//! [`two_bot_core::settings`] — framework-free and unit-testable without a
//! database. This module is the thin sqlx seam: run the queries, hand rows to
//! the core cache, execute validated writes transactionally.
//!
//! Poll loop (legacy `src/index.ts`): `SELECT max(version)` + `count(*)` every
//! [`two_bot_core::settings::POLL_SECONDS`]; on move, refetch the whole small
//! table and let [`two_bot_core::settings::SettingsCache`] partition the change
//! into hot (apply live, one `setting_changed` log line each), cold (stored,
//! restart to apply), and ignored env-only/unknown rows (one log line each,
//! never applied).

use sqlx::{Pool, Postgres};
use two_bot_core::settings::{
    validate_write, IgnoreReason, KeyChange, RefreshReport, SettingRow, ValidatedWrite,
    WriteAction, WriteRefusal,
};

/// Live poll view over the settings tables.
pub struct SettingsStore<'a> {
    pool: &'a Pool<Postgres>,
}

impl<'a> SettingsStore<'a> {
    /// Borrow the pool; migrations (including `0330`) are applied by
    /// [`crate::CutoverDb::migrate`], not here.
    #[must_use]
    pub fn new(pool: &'a Pool<Postgres>) -> Self {
        Self { pool }
    }

    /// Load every row the cache needs: guild, key, value, version.
    pub async fn load_rows(&self) -> Result<Vec<SettingRow>, sqlx::Error> {
        let rows: Vec<(String, String, serde_json::Value, i64)> =
            sqlx::query_as("SELECT guild_id, key, value, version FROM guild_settings")
                .fetch_all(self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(guild_id, key, value, version)| SettingRow {
                guild_id,
                key,
                value,
                version,
            })
            .collect())
    }

    /// The cheap poll: `max(version)` and `count(*)` in one query.
    /// `COALESCE` so an empty table reports `(0, 0)` rather than NULL.
    pub async fn poll_marks(&self) -> Result<(i64, i64), sqlx::Error> {
        let row: (Option<i64>, i64) =
            sqlx::query_as("SELECT max(version), count(*) FROM guild_settings")
                .fetch_one(self.pool)
                .await?;
        Ok((row.0.unwrap_or(0), row.1))
    }

    /// Write one setting and its audit row in one transaction, bumping the
    /// global version so every process's next poll picks it up (legacy
    /// `SettingsStore.set`).
    ///
    /// Validation runs first, before any SQL: a refusal writes no row and no
    /// audit row. `None` value deletes the row, handing the key back to the
    /// environment — the documented undo path, audited like any other change.
    /// No `nextval` on delete: the allocated number would be discarded with
    /// the row, so nothing observable would move; delete visibility comes from
    /// the poll's row count.
    ///
    /// Returns the validated write so the caller can log it by class (cold
    /// writes need a restart before consumers pick them up).
    pub async fn set(
        &self,
        guild_id: &str,
        key: &str,
        value: Option<serde_json::Value>,
        actor: &str,
    ) -> Result<ValidatedWrite, SettingsWriteError> {
        let validated = validate_write(guild_id, key, value, actor)?;
        let mut tx = self.pool.begin().await?;

        let previous: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT value FROM guild_settings WHERE guild_id = $1 AND key = $2")
                .bind(guild_id)
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?;

        match &validated.action {
            WriteAction::Delete => {
                sqlx::query("DELETE FROM guild_settings WHERE guild_id = $1 AND key = $2")
                    .bind(guild_id)
                    .bind(key)
                    .execute(&mut *tx)
                    .await?;
            }
            WriteAction::Upsert(value) => {
                sqlx::query(
                    "INSERT INTO guild_settings (guild_id, key, value, version, updated_at, updated_by)
                     VALUES ($1, $2, $3, nextval('guild_settings_version_seq'), now(), $4)
                     ON CONFLICT (guild_id, key) DO UPDATE
                       SET value = EXCLUDED.value,
                           version = EXCLUDED.version,
                           updated_at = EXCLUDED.updated_at,
                           updated_by = EXCLUDED.updated_by",
                )
                .bind(guild_id)
                .bind(key)
                .bind(value)
                .bind(&validated.actor)
                .execute(&mut *tx)
                .await?;
            }
        }

        let old_json = previous.map(|(v,)| v);
        let new_json = match &validated.action {
            WriteAction::Upsert(value) => Some(value.clone()),
            WriteAction::Delete => None,
        };
        sqlx::query(
            "INSERT INTO guild_settings_audit (guild_id, key, old_value, new_value, actor, at)
             VALUES ($1, $2, $3, $4, $5, now())",
        )
        .bind(guild_id)
        .bind(key)
        .bind(old_json)
        .bind(new_json)
        .bind(&validated.actor)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(validated)
    }
}

/// A settings write failure: refused before SQL vs database error.
#[derive(Debug, thiserror::Error)]
pub enum SettingsWriteError {
    #[error("{0}")]
    Refused(#[from] WriteRefusal),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Emit the per-change log lines for one poll that moved (legacy
/// `settings_reloaded` + `setting_changed`): one line per hot key with
/// from/to, one line naming stored-but-restart-needed cold keys, one line per
/// ignored env-only/unknown DB row.
pub fn log_refresh_report(report: &RefreshReport) {
    tracing::info!(
        from_version = report.from_version,
        to_version = report.to_version,
        from_rows = report.from_rows,
        to_rows = report.to_rows,
        "settings_reloaded"
    );
    for change in &report.hot {
        log_key_change(change);
    }
    for change in &report.cold {
        tracing::info!(
            key = change.key.as_str(),
            guild_id = change.guild_id.as_str(),
            "setting_stored_needs_restart"
        );
    }
    for ignored in &report.ignored {
        let reason = match ignored.reason {
            IgnoreReason::EnvOnly => "env_only",
            IgnoreReason::Unknown => "unknown",
        };
        tracing::info!(
            key = ignored.key.as_str(),
            guild_id = ignored.guild_id.as_str(),
            reason,
            "setting_ignored_not_applied"
        );
    }
}

fn log_key_change(change: &KeyChange) {
    tracing::info!(
        key = change.key.as_str(),
        guild_id = change.guild_id.as_str(),
        from = change.old.as_deref().unwrap_or("(unset)"),
        to = change.new.as_deref().unwrap_or("(unset)"),
        "setting_changed"
    );
}
