//! V11b persistence for [`VoiceConfiguration`] (TOG-12875), the storage side of
//! `/export` and `/import` (`docs/voice-rooms.md` §V11).
//!
//! [`PgVoiceConfigStore::snapshot`] reads every configuration section in one
//! read-only `REPEATABLE READ` transaction, so the sections always belong to
//! one moment. [`PgVoiceConfigStore::apply`] replaces every section in ONE
//! transaction: a failing row rolls back all of them and the previous
//! configuration stays intact. Neither touches `voice_rooms` or
//! `voice_text_companions`: configuration never changes live rooms.
//!
//! The caller validates against a trusted guild inventory with
//! `two_bot_core::voice_config::validate_configuration` before applying; the
//! SQL CHECKs in `0228_voice_config.sql` are the last line of defense and the
//! reason a mid-apply failure is a meaningful rollback case.
//!
//! Canonical order: snapshots list every keyed section sorted (snowflakes
//! numerically, free-text keys bytewise, list choices by stored position), so
//! `snapshot -> export -> import -> apply -> snapshot` is the identity. The
//! unordered sets (logging mentions, a command's roles) are therefore stored
//! and returned in sorted order.
//!
//! Codec gap, kept deliberately: `CreatorConfiguration::default_limit` has no
//! "inherit the live creator's limit" value, while `voice_creators` stores
//! NULL for it (0225). A snapshot reports NULL as 0, and an apply that writes
//! 0 over an existing NULL keeps the NULL, so a plain export/import round trip
//! never turns "inherit" into "unlimited".
//!
//! Parameter binding: https://docs.rs/sqlx/0.9.0/sqlx/fn.query.html

use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};
use two_bot_core::voice_assistant_request::DEFAULT_NO_GAME_LABEL;
use two_bot_core::voice_config::{
    ChannelTemplates, CommandRoles, CreatorConfiguration, GameAlias, GuildSettings, LogDetail,
    LoggingConfiguration, PermissionSource, RandomList, RoomPosition, VoiceConfiguration,
    VOICE_CONFIG_VERSION,
};
use two_bot_core::voice_text_channel::DEFAULT_TEXT_CHANNEL_NAME;

/// Defaults reported for a guild that has never been configured.
const DEFAULT_TIME_ZONE: &str = "UTC";

#[derive(Debug, Clone)]
pub struct PgVoiceConfigStore {
    pool: PgPool,
}

impl PgVoiceConfigStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The guild's stored configuration. A guild with no `voice_guild_settings`
    /// row reports the defaults, so `/export` works on a fresh guild.
    pub async fn snapshot(&self, guild_id: u64) -> Result<VoiceConfiguration, sqlx::Error> {
        let guild = guild_id.to_string();
        let mut tx = self.pool.begin().await?;
        // Must be the first statement of the transaction.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let creators = sqlx::query(
            "SELECT * FROM voice_creators WHERE guild_id = $1
             ORDER BY length(channel_id), channel_id",
        )
        .bind(&guild)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(decode_creator)
        .collect::<Result<Vec<_>, _>>()?;
        let templates = sqlx::query(
            "SELECT channel_id, name_template, status_template FROM voice_channel_templates
             WHERE guild_id = $1 ORDER BY length(channel_id), channel_id",
        )
        .bind(&guild)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|row| {
            Ok(ChannelTemplates {
                channel_id: row.try_get("channel_id")?,
                name_template: row.try_get("name_template")?,
                status_template: row.try_get("status_template")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let aliases = sqlx::query(
            r#"SELECT game, alias FROM voice_game_aliases
               WHERE guild_id = $1 ORDER BY game COLLATE "C""#,
        )
        .bind(&guild)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|row| {
            Ok(GameAlias {
                game: row.try_get("game")?,
                alias: row.try_get("alias")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let mut lists: Vec<RandomList> = Vec::new();
        for row in sqlx::query(
            r#"SELECT list_name, choice FROM voice_random_list_choices
               WHERE guild_id = $1 ORDER BY list_name COLLATE "C", position"#,
        )
        .bind(&guild)
        .fetch_all(&mut *tx)
        .await?
        {
            let name: String = row.try_get("list_name")?;
            let choice: String = row.try_get("choice")?;
            match lists.last_mut() {
                Some(list) if list.name == name => list.choices.push(choice),
                _ => lists.push(RandomList {
                    name,
                    choices: vec![choice],
                }),
            }
        }
        let logging =
            match sqlx::query("SELECT channel_id, detail FROM voice_logging WHERE guild_id = $1")
                .bind(&guild)
                .fetch_optional(&mut *tx)
                .await?
            {
                None => None,
                Some(row) => Some(LoggingConfiguration {
                    channel_id: row.try_get("channel_id")?,
                    detail: match row.try_get::<&str, _>("detail")? {
                        "errors" => LogDetail::Errors,
                        "lifecycle" => LogDetail::Lifecycle,
                        "verbose" => LogDetail::Verbose,
                        _ => return Err(invalid_argument("unknown logging detail")),
                    },
                    mention_member_ids: sqlx::query_scalar(
                        "SELECT member_id FROM voice_logging_mention_members
                     WHERE guild_id = $1 ORDER BY length(member_id), member_id",
                    )
                    .bind(&guild)
                    .fetch_all(&mut *tx)
                    .await?,
                    mention_role_ids: sqlx::query_scalar(
                        "SELECT role_id FROM voice_logging_mention_roles
                     WHERE guild_id = $1 ORDER BY length(role_id), role_id",
                    )
                    .bind(&guild)
                    .fetch_all(&mut *tx)
                    .await?,
                }),
            };
        let mut command_roles: Vec<CommandRoles> = Vec::new();
        // A command with no roles is a row in `voice_command_roles` alone.
        for row in sqlx::query(
            r#"SELECT c.command, m.role_id
               FROM voice_command_roles c
               LEFT JOIN voice_command_role_members m
                 ON m.guild_id = c.guild_id AND m.command = c.command
               WHERE c.guild_id = $1
               ORDER BY c.command COLLATE "C", length(m.role_id), m.role_id"#,
        )
        .bind(&guild)
        .fetch_all(&mut *tx)
        .await?
        {
            let command: String = row.try_get("command")?;
            let role: Option<String> = row.try_get("role_id")?;
            match command_roles.last_mut() {
                Some(entry) if entry.command == command => entry.role_ids.extend(role),
                _ => command_roles.push(CommandRoles {
                    command,
                    role_ids: role.into_iter().collect(),
                }),
            }
        }
        let settings = match sqlx::query("SELECT * FROM voice_guild_settings WHERE guild_id = $1")
            .bind(&guild)
            .fetch_optional(&mut *tx)
            .await?
        {
            Some(row) => GuildSettings {
                creation_enabled: row.try_get("creation_enabled")?,
                unique_names: row.try_get("unique_names")?,
                no_game_label: row.try_get("no_game_label")?,
                force_single_game: row.try_get("force_single_game")?,
                count_members_without_activity: row.try_get("count_members_without_activity")?,
                time_zone: row.try_get("time_zone")?,
                text_channel_name: row.try_get("text_channel_name")?,
                text_viewer_role_id: row.try_get("text_viewer_role_id")?,
                command_role_id: row.try_get("command_role_id")?,
                command_roles,
            },
            None => GuildSettings {
                creation_enabled: true,
                unique_names: false,
                no_game_label: DEFAULT_NO_GAME_LABEL.to_owned(),
                force_single_game: false,
                count_members_without_activity: false,
                time_zone: DEFAULT_TIME_ZONE.to_owned(),
                text_channel_name: DEFAULT_TEXT_CHANNEL_NAME.to_owned(),
                text_viewer_role_id: None,
                command_role_id: None,
                command_roles,
            },
        };
        tx.rollback().await?;
        Ok(VoiceConfiguration {
            version: VOICE_CONFIG_VERSION,
            guild_id: guild,
            creators,
            templates,
            aliases,
            lists,
            logging,
            settings,
        })
    }

    /// Replace every configuration section for `guild_id` in one transaction.
    /// Sections absent from `config` are removed. The caller must have run
    /// `validate_configuration` against a trusted inventory; this refuses a
    /// wrong version or guild, non-canonical snowflakes (the SQL format CHECK
    /// does not bound the u64 range) and anything the SQL CHECKs reject.
    pub async fn apply(
        &self,
        guild_id: u64,
        config: &VoiceConfiguration,
    ) -> Result<(), sqlx::Error> {
        let guild = guild_id.to_string();
        if config.version != VOICE_CONFIG_VERSION {
            return Err(invalid_argument("unsupported voice configuration version"));
        }
        if config.guild_id != guild {
            return Err(invalid_argument(
                "configuration belongs to a different guild",
            ));
        }
        let mut tx = self.pool.begin().await?;
        // One writer per guild: concurrent applies would otherwise interleave
        // their DELETE/INSERT pairs and trip primary keys.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("voice_config:{guild}"))
            .execute(&mut *tx)
            .await?;
        write_creators(&mut tx, &guild, config).await?;
        write_templates(&mut tx, &guild, config).await?;
        write_aliases(&mut tx, &guild, config).await?;
        write_lists(&mut tx, &guild, config).await?;
        write_logging(&mut tx, &guild, config).await?;
        // Settings last: the usual rejected row is a bad setting, which
        // exercises the rollback of every section written above.
        write_settings(&mut tx, &guild, config).await?;
        tx.commit().await
    }
}

async fn write_creators(
    tx: &mut Transaction<'_, Postgres>,
    guild: &str,
    config: &VoiceConfiguration,
) -> Result<(), sqlx::Error> {
    let keep = config
        .creators
        .iter()
        .map(|creator| snowflake(&creator.channel_id).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;
    sqlx::query("DELETE FROM voice_creators WHERE guild_id = $1 AND channel_id <> ALL($2::text[])")
        .bind(guild)
        .bind(&keep)
        .execute(&mut **tx)
        .await?;
    for creator in &config.creators {
        let (source, permission_channel) = match &creator.permission_source {
            PermissionSource::Creator {} => ("creator", None),
            PermissionSource::Category {} => ("category", None),
            PermissionSource::Channel { channel_id } => ("channel", Some(snowflake(channel_id)?)),
        };
        let position = match creator.position {
            RoomPosition::Above => "above",
            RoomPosition::Below => "below",
        };
        // Upsert, not delete + insert: columns the configuration does not
        // carry (a creator's V9b text-channel name and viewer role) survive.
        sqlx::query(
            "INSERT INTO voice_creators
             (guild_id, channel_id, name_template, status_template, permission_source,
              permission_channel_id, default_limit, private_default, text_channels,
              position, first_room_number, group_by_category)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT (guild_id, channel_id) DO UPDATE SET
              name_template = EXCLUDED.name_template,
              status_template = EXCLUDED.status_template,
              permission_source = EXCLUDED.permission_source,
              permission_channel_id = EXCLUDED.permission_channel_id,
              default_limit = CASE
                WHEN EXCLUDED.default_limit = 0 AND voice_creators.default_limit IS NULL
                THEN NULL ELSE EXCLUDED.default_limit END,
              private_default = EXCLUDED.private_default,
              text_channels = EXCLUDED.text_channels,
              position = EXCLUDED.position,
              first_room_number = EXCLUDED.first_room_number,
              group_by_category = EXCLUDED.group_by_category",
        )
        .bind(guild)
        .bind(snowflake(&creator.channel_id)?)
        .bind(&creator.name_template)
        .bind(&creator.status_template)
        .bind(source)
        .bind(permission_channel)
        .bind(i32::from(creator.default_limit))
        .bind(creator.always_private)
        .bind(creator.text_channels)
        .bind(position)
        .bind(i64::from(creator.first_number))
        .bind(creator.group_by_category)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn write_templates(
    tx: &mut Transaction<'_, Postgres>,
    guild: &str,
    config: &VoiceConfiguration,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM voice_channel_templates WHERE guild_id = $1")
        .bind(guild)
        .execute(&mut **tx)
        .await?;
    for template in &config.templates {
        sqlx::query(
            "INSERT INTO voice_channel_templates
             (guild_id, channel_id, name_template, status_template) VALUES ($1,$2,$3,$4)",
        )
        .bind(guild)
        .bind(snowflake(&template.channel_id)?)
        .bind(&template.name_template)
        .bind(&template.status_template)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn write_aliases(
    tx: &mut Transaction<'_, Postgres>,
    guild: &str,
    config: &VoiceConfiguration,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM voice_game_aliases WHERE guild_id = $1")
        .bind(guild)
        .execute(&mut **tx)
        .await?;
    for alias in &config.aliases {
        sqlx::query("INSERT INTO voice_game_aliases (guild_id, game, alias) VALUES ($1,$2,$3)")
            .bind(guild)
            .bind(&alias.game)
            .bind(&alias.alias)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn write_lists(
    tx: &mut Transaction<'_, Postgres>,
    guild: &str,
    config: &VoiceConfiguration,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM voice_random_list_choices WHERE guild_id = $1")
        .bind(guild)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM voice_random_lists WHERE guild_id = $1")
        .bind(guild)
        .execute(&mut **tx)
        .await?;
    for list in &config.lists {
        sqlx::query("INSERT INTO voice_random_lists (guild_id, name) VALUES ($1,$2)")
            .bind(guild)
            .bind(&list.name)
            .execute(&mut **tx)
            .await?;
        for (position, choice) in list.choices.iter().enumerate() {
            sqlx::query(
                "INSERT INTO voice_random_list_choices (guild_id, list_name, position, choice)
                 VALUES ($1,$2,$3,$4)",
            )
            .bind(guild)
            .bind(&list.name)
            .bind(i32::try_from(position).map_err(invalid_argument)?)
            .bind(choice)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

async fn write_logging(
    tx: &mut Transaction<'_, Postgres>,
    guild: &str,
    config: &VoiceConfiguration,
) -> Result<(), sqlx::Error> {
    for table in [
        "DELETE FROM voice_logging WHERE guild_id = $1",
        "DELETE FROM voice_logging_mention_members WHERE guild_id = $1",
        "DELETE FROM voice_logging_mention_roles WHERE guild_id = $1",
    ] {
        sqlx::query(table).bind(guild).execute(&mut **tx).await?;
    }
    let Some(logging) = &config.logging else {
        return Ok(());
    };
    let detail = match logging.detail {
        LogDetail::Errors => "errors",
        LogDetail::Lifecycle => "lifecycle",
        LogDetail::Verbose => "verbose",
    };
    sqlx::query("INSERT INTO voice_logging (guild_id, channel_id, detail) VALUES ($1,$2,$3)")
        .bind(guild)
        .bind(snowflake(&logging.channel_id)?)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
    for member in &logging.mention_member_ids {
        sqlx::query(
            "INSERT INTO voice_logging_mention_members (guild_id, member_id) VALUES ($1,$2)",
        )
        .bind(guild)
        .bind(snowflake(member)?)
        .execute(&mut **tx)
        .await?;
    }
    for role in &logging.mention_role_ids {
        sqlx::query("INSERT INTO voice_logging_mention_roles (guild_id, role_id) VALUES ($1,$2)")
            .bind(guild)
            .bind(snowflake(role)?)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn write_settings(
    tx: &mut Transaction<'_, Postgres>,
    guild: &str,
    config: &VoiceConfiguration,
) -> Result<(), sqlx::Error> {
    let settings = &config.settings;
    for table in [
        "DELETE FROM voice_command_role_members WHERE guild_id = $1",
        "DELETE FROM voice_command_roles WHERE guild_id = $1",
        "DELETE FROM voice_guild_settings WHERE guild_id = $1",
    ] {
        sqlx::query(table).bind(guild).execute(&mut **tx).await?;
    }
    sqlx::query(
        "INSERT INTO voice_guild_settings
         (guild_id, creation_enabled, unique_names, no_game_label, force_single_game,
          count_members_without_activity, time_zone, text_channel_name,
          text_viewer_role_id, command_role_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(guild)
    .bind(settings.creation_enabled)
    .bind(settings.unique_names)
    .bind(&settings.no_game_label)
    .bind(settings.force_single_game)
    .bind(settings.count_members_without_activity)
    .bind(&settings.time_zone)
    .bind(&settings.text_channel_name)
    .bind(
        settings
            .text_viewer_role_id
            .as_deref()
            .map(snowflake)
            .transpose()?,
    )
    .bind(
        settings
            .command_role_id
            .as_deref()
            .map(snowflake)
            .transpose()?,
    )
    .execute(&mut **tx)
    .await?;
    for restriction in &settings.command_roles {
        sqlx::query("INSERT INTO voice_command_roles (guild_id, command) VALUES ($1,$2)")
            .bind(guild)
            .bind(&restriction.command)
            .execute(&mut **tx)
            .await?;
        for role in &restriction.role_ids {
            sqlx::query(
                "INSERT INTO voice_command_role_members (guild_id, command, role_id)
                 VALUES ($1,$2,$3)",
            )
            .bind(guild)
            .bind(&restriction.command)
            .bind(snowflake(role)?)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

fn invalid_argument(error: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::InvalidArgument(error.to_string())
}

/// Canonical nonzero decimal u64, the same rule as `voice_config::snowflake`.
fn snowflake(id: &str) -> Result<&str, sqlx::Error> {
    if id.starts_with('0')
        || !id.bytes().all(|c| c.is_ascii_digit())
        || !id.parse::<u64>().is_ok_and(|n| n > 0)
    {
        return Err(invalid_argument(
            "snowflake must be a canonical nonzero decimal u64 string",
        ));
    }
    Ok(id)
}

fn decode_creator(row: &PgRow) -> Result<CreatorConfiguration, sqlx::Error> {
    let permission_channel: Option<String> = row.try_get("permission_channel_id")?;
    let permission_source = match row.try_get::<&str, _>("permission_source")? {
        "creator" => PermissionSource::Creator {},
        "category" => PermissionSource::Category {},
        "channel" => PermissionSource::Channel {
            channel_id: permission_channel
                .ok_or_else(|| invalid_argument("missing permission channel"))?,
        },
        _ => return Err(invalid_argument("unknown permission source")),
    };
    let position = match row.try_get::<&str, _>("position")? {
        "above" => RoomPosition::Above,
        "below" => RoomPosition::Below,
        _ => return Err(invalid_argument("unknown room position")),
    };
    Ok(CreatorConfiguration {
        channel_id: row.try_get("channel_id")?,
        name_template: row.try_get("name_template")?,
        status_template: row.try_get("status_template")?,
        // NULL (inherit the live creator's limit) has no codec value; see the
        // module docs.
        default_limit: row
            .try_get::<Option<i32>, _>("default_limit")?
            .map_or(Ok(0), u16::try_from)
            .map_err(invalid_argument)?,
        always_private: row.try_get("private_default")?,
        text_channels: row.try_get("text_channels")?,
        position,
        first_number: u32::try_from(row.try_get::<i64, _>("first_room_number")?)
            .map_err(invalid_argument)?,
        group_by_category: row.try_get("group_by_category")?,
        permission_source,
    })
}
