//! One guild configuration writer, revision and audit row per transaction.

use sqlx::{PgPool, Postgres, Transaction};

pub(crate) struct VoiceSettingsChange<'a> {
    pub tx: Transaction<'a, Postgres>,
    guild: String,
    actor: String,
    pub version: i64,
}

impl VoiceSettingsChange<'_> {
    pub async fn begin(
        pool: &PgPool,
        guild_id: u64,
        actor: &str,
        expected_version: Option<i64>,
    ) -> Result<Self, sqlx::Error> {
        if guild_id == 0
            || actor.trim().is_empty()
            || actor.chars().count() > 128
            || actor.contains('\0')
        {
            return Err(sqlx::Error::InvalidArgument(
                "invalid voice change attribution".into(),
            ));
        }
        let guild = guild_id.to_string();
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("voice_config:{guild}"))
            .execute(&mut *tx)
            .await?;
        let current: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(version), 0) FROM voice_settings_audit WHERE guild_id = $1",
        )
        .bind(&guild)
        .fetch_one(&mut *tx)
        .await?;
        if expected_version.is_some_and(|expected| expected != current) {
            return Err(sqlx::Error::RowNotFound);
        }
        let version: i64 = sqlx::query_scalar(
            "SELECT nextval(pg_get_serial_sequence('voice_settings_audit', 'version'))",
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "SELECT set_config('two.voice_version', $1, true),
                    set_config('two.voice_actor', $2, true),
                    set_config('two.voice_guild', $3, true)",
        )
        .bind(version.to_string())
        .bind(actor)
        .bind(&guild)
        .execute(&mut *tx)
        .await?;
        Ok(Self {
            tx,
            guild,
            actor: actor.to_owned(),
            version,
        })
    }

    pub async fn commit(mut self) -> Result<i64, sqlx::Error> {
        sqlx::query(
            "INSERT INTO voice_settings_audit (version, guild_id, actor) VALUES ($1,$2,$3)",
        )
        .bind(self.version)
        .bind(&self.guild)
        .bind(&self.actor)
        .execute(&mut *self.tx)
        .await?;
        self.tx.commit().await?;
        Ok(self.version)
    }
}
