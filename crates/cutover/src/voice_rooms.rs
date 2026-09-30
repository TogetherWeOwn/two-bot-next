//! Async voice-room persistence, sharing the existing sqlx migration history.
//!
//! Unlike the core's synchronous replay snapshot, this adapter returns database
//! errors to the caller. Callers must persist a created channel before moving
//! its owner and compensate a failed write by deleting that new channel.
//!
//! Parameter binding: https://docs.rs/sqlx/0.9.0/sqlx/fn.query.html

use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use two_bot_core::voice_rooms::{CreatorChannel, PermissionSource, RoomPosition, VoiceRoom};
use two_bot_core::{format_iso_millis, parse_iso_millis, Snowflake};

#[derive(Debug, Clone)]
pub struct PgRoomStore {
    pool: PgPool,
}

impl PgRoomStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn add_creator(&self, creator: &CreatorChannel) -> Result<(), sqlx::Error> {
        creator.validate().map_err(invalid_argument)?;
        let source = match creator.permission_source {
            PermissionSource::Creator => "creator",
            PermissionSource::Category => "category",
            PermissionSource::Channel(_) => "channel",
        };
        let position = match creator.position {
            RoomPosition::Above => "above",
            RoomPosition::Below => "below",
        };
        sqlx::query(
            "INSERT INTO voice_creators
             (guild_id, channel_id, name_template, permission_source,
              permission_channel_id, default_limit, private_default,
              text_channels, position, first_room_number)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT (guild_id, channel_id) DO UPDATE SET
              name_template = EXCLUDED.name_template,
              permission_source = EXCLUDED.permission_source,
              permission_channel_id = EXCLUDED.permission_channel_id,
              default_limit = EXCLUDED.default_limit,
              private_default = EXCLUDED.private_default,
              text_channels = EXCLUDED.text_channels,
              position = EXCLUDED.position,
              first_room_number = EXCLUDED.first_room_number",
        )
        .bind(creator.guild_id.to_string())
        .bind(creator.channel_id.to_string())
        .bind(&creator.name_template)
        .bind(source)
        .bind(creator.permission_channel_id.map(|id| id.to_string()))
        .bind(creator.default_limit as i32)
        .bind(creator.private_default)
        .bind(creator.text_channels)
        .bind(position)
        .bind(creator.first_room_number)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_creator(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
    ) -> Result<bool, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM voice_creators WHERE guild_id = $1 AND channel_id = $2")
                .bind(guild_id.to_string())
                .bind(channel_id.to_string())
                .execute(&self.pool)
                .await?
                .rows_affected()
                != 0,
        )
    }

    pub async fn creators(&self, guild_id: Snowflake) -> Result<Vec<CreatorChannel>, sqlx::Error> {
        sqlx::query("SELECT * FROM voice_creators WHERE guild_id = $1 ORDER BY channel_id")
            .bind(guild_id.to_string())
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(decode_creator)
            .collect()
    }

    pub async fn creator_for(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
    ) -> Result<Option<CreatorChannel>, sqlx::Error> {
        sqlx::query("SELECT * FROM voice_creators WHERE guild_id = $1 AND channel_id = $2")
            .bind(guild_id.to_string())
            .bind(channel_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(decode_creator)
            .transpose()
    }

    /// Insert once: replay must never overwrite ownership, the seed or timestamp.
    /// Returns false when this exact channel is already tracked.
    pub async fn add_room(&self, room: &VoiceRoom) -> Result<bool, sqlx::Error> {
        let millis = parse_iso_millis(&room.created_at)
            .ok_or_else(|| invalid_argument("invalid room creation timestamp"))?;
        let timestamp = OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
            .map_err(invalid_argument)?;
        Ok(sqlx::query(
            "INSERT INTO voice_rooms
             (guild_id, channel_id, creator_channel_id, owner_id,
              original_creator_id, name_seed, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (guild_id, channel_id) DO NOTHING",
        )
        .bind(room.guild_id.to_string())
        .bind(room.channel_id.to_string())
        .bind(room.creator_channel_id.to_string())
        .bind(room.owner_id.to_string())
        .bind(room.original_creator_id.to_string())
        .bind(room.name_seed.to_string())
        .bind(timestamp)
        .execute(&self.pool)
        .await?
        .rows_affected()
            != 0)
    }

    pub async fn room_for(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
    ) -> Result<Option<VoiceRoom>, sqlx::Error> {
        sqlx::query("SELECT * FROM voice_rooms WHERE guild_id = $1 AND channel_id = $2")
            .bind(guild_id.to_string())
            .bind(channel_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(decode_room)
            .transpose()
    }

    pub async fn rooms_in_guild(&self, guild_id: Snowflake) -> Result<Vec<VoiceRoom>, sqlx::Error> {
        sqlx::query("SELECT * FROM voice_rooms WHERE guild_id = $1 ORDER BY channel_id")
            .bind(guild_id.to_string())
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(decode_room)
            .collect()
    }

    pub async fn rooms_for_owner(
        &self,
        guild_id: Snowflake,
        owner_id: Snowflake,
    ) -> Result<Vec<VoiceRoom>, sqlx::Error> {
        sqlx::query(
            "SELECT * FROM voice_rooms WHERE guild_id = $1 AND owner_id = $2 ORDER BY channel_id",
        )
        .bind(guild_id.to_string())
        .bind(owner_id.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(decode_room)
        .collect()
    }

    pub async fn remove_room(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
    ) -> Result<Option<VoiceRoom>, sqlx::Error> {
        sqlx::query("DELETE FROM voice_rooms WHERE guild_id = $1 AND channel_id = $2 RETURNING *")
            .bind(guild_id.to_string())
            .bind(channel_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(decode_room)
            .transpose()
    }
}

fn invalid_argument(error: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::InvalidArgument(error.to_string())
}

fn decode_id(row: &PgRow, column: &str) -> Result<u64, sqlx::Error> {
    row.try_get::<String, _>(column)?
        .parse()
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

fn decode_creator(row: &PgRow) -> Result<CreatorChannel, sqlx::Error> {
    let permission_channel_id = row
        .try_get::<Option<String>, _>("permission_channel_id")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))
        })
        .transpose()?;
    let permission_source = match row.try_get::<&str, _>("permission_source")? {
        "creator" => PermissionSource::Creator,
        "category" => PermissionSource::Category,
        "channel" => PermissionSource::Channel(
            permission_channel_id.ok_or_else(|| invalid_argument("missing permission channel"))?,
        ),
        _ => return Err(invalid_argument("unknown permission source")),
    };
    let position = match row.try_get::<&str, _>("position")? {
        "above" => RoomPosition::Above,
        "below" => RoomPosition::Below,
        _ => return Err(invalid_argument("unknown room position")),
    };
    let creator = CreatorChannel {
        guild_id: decode_id(row, "guild_id")?,
        channel_id: decode_id(row, "channel_id")?,
        name_template: row.try_get("name_template")?,
        permission_source,
        permission_channel_id,
        default_limit: i64::from(row.try_get::<i32, _>("default_limit")?),
        private_default: row.try_get("private_default")?,
        text_channels: row.try_get("text_channels")?,
        position,
        first_room_number: row.try_get("first_room_number")?,
    };
    creator.validate().map_err(invalid_argument)?;
    Ok(creator)
}

fn decode_room(row: &PgRow) -> Result<VoiceRoom, sqlx::Error> {
    let timestamp: OffsetDateTime = row.try_get("created_at")?;
    Ok(VoiceRoom {
        guild_id: decode_id(row, "guild_id")?,
        channel_id: decode_id(row, "channel_id")?,
        creator_channel_id: decode_id(row, "creator_channel_id")?,
        owner_id: decode_id(row, "owner_id")?,
        original_creator_id: decode_id(row, "original_creator_id")?,
        name_seed: decode_id(row, "name_seed")?,
        created_at: format_iso_millis((timestamp.unix_timestamp_nanos() / 1_000_000) as i64),
    })
}
