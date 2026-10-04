//! Async voice-room persistence, sharing the existing sqlx migration history.
//!
//! Unlike the core's synchronous replay snapshot, this adapter returns database
//! errors to the caller. Callers must persist a created channel before moving
//! its owner and compensate a failed write by deleting that new channel.
//!
//! Parameter binding: <https://docs.rs/sqlx/0.9.0/sqlx/fn.query.html>

use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use two_bot_core::voice_access::{validate_access_controls, AccessControls};
use two_bot_core::voice_create_admission::{
    decide_admission, AcceptedReservation, AdmissionDecision, AdmissionRequest,
    CreateAdmissionConfig, RefusalReason, CREATE_BURST_WINDOW_SECS,
};
use two_bot_core::voice_logging::{parse_detail_level, LoggingSettings};
use two_bot_core::voice_rooms::{
    CreatorChannel, PermissionSource, RoomPosition, TextCompanion, VoiceRoom,
};
use two_bot_core::{format_iso_millis, parse_iso_millis, Snowflake};

use super::voice_config_store::PgVoiceConfigStore;

/// The verdict of [`PgRoomStore::claim_create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateClaim {
    /// Recorded durably; settle it with [`PgRoomStore::settle_create`].
    Admitted { reservation_id: String },
    /// Refused before any write; the first tripped limit.
    Refused(RefusalReason),
}

#[derive(Debug, Clone)]
pub struct PgRoomStore {
    pool: PgPool,
}

async fn insert_room(
    connection: &mut sqlx::PgConnection,
    room: &VoiceRoom,
) -> Result<bool, sqlx::Error> {
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
    .execute(connection)
    .await?
    .rows_affected()
        != 0)
}

impl PgRoomStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// V11 configuration persistence (`/export` reads, `/import` writes)
    /// over this store's pool. Snapshot and apply each run in one
    /// transaction; apply never touches live rooms or companions.
    #[must_use]
    pub fn voice_configs(&self) -> PgVoiceConfigStore {
        PgVoiceConfigStore::new(self.pool.clone())
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
              text_channels, text_channel_name, text_viewer_role_id,
              position, first_room_number, group_by_category)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
             ON CONFLICT (guild_id, channel_id) DO UPDATE SET
              name_template = EXCLUDED.name_template,
              permission_source = EXCLUDED.permission_source,
              permission_channel_id = EXCLUDED.permission_channel_id,
              default_limit = EXCLUDED.default_limit,
              private_default = EXCLUDED.private_default,
              text_channels = EXCLUDED.text_channels,
              text_channel_name = EXCLUDED.text_channel_name,
              text_viewer_role_id = EXCLUDED.text_viewer_role_id,
              position = EXCLUDED.position,
              first_room_number = EXCLUDED.first_room_number,
              group_by_category = EXCLUDED.group_by_category",
        )
        .bind(creator.guild_id.to_string())
        .bind(creator.channel_id.to_string())
        .bind(&creator.name_template)
        .bind(source)
        .bind(creator.permission_channel_id.map(|id| id.to_string()))
        .bind(creator.default_limit.map(|limit| limit as i32))
        .bind(creator.private_default)
        .bind(creator.text_channels)
        .bind(creator.text_channel_name.clone())
        .bind(creator.text_viewer_role_id.map(|id| id.to_string()))
        .bind(position)
        .bind(creator.first_room_number)
        .bind(creator.group_by_category)
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
        let mut connection = self.pool.acquire().await?;
        insert_room(&mut connection, room).await
    }

    /// Atomically transfer a claim's cap slot to its live room. Claimants use
    /// the same guild lock, so neither a missing slot nor double occupancy is
    /// visible. Any insert/settlement error rolls the whole transaction back,
    /// keeping the reservation held until confirmed compensation. Exact replay
    /// is idempotent; a mismatched or previously rolled-back claim refuses.
    pub async fn persist_create(
        &self,
        reservation_id: &str,
        room: &VoiceRoom,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("voice_create:{}", room.guild_id))
            .execute(&mut *tx)
            .await?;
        let claim = sqlx::query(
            "SELECT channel_id, settled_at IS NOT NULL AS settled
             FROM voice_create_reservations
             WHERE id = $1 AND guild_id = $2 AND user_id = $3 FOR UPDATE",
        )
        .bind(reservation_id)
        .bind(room.guild_id.to_string())
        .bind(room.original_creator_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| invalid_argument("create reservation does not match room"))?;
        let bound = claim.try_get::<Option<String>, _>("channel_id")?;
        let settled = claim.try_get::<bool, _>("settled")?;
        if bound.is_some() && bound != Some(room.channel_id.to_string()) {
            return Err(invalid_argument(
                "create reservation is bound to another channel",
            ));
        }
        if settled && bound.is_none() {
            return Err(invalid_argument("create reservation is already settled"));
        }
        if settled || !insert_room(&mut tx, room).await? {
            let stored =
                sqlx::query("SELECT * FROM voice_rooms WHERE guild_id = $1 AND channel_id = $2")
                    .bind(room.guild_id.to_string())
                    .bind(room.channel_id.to_string())
                    .fetch_one(&mut *tx)
                    .await?;
            let mut expected = room.clone();
            expected.created_at = format_iso_millis(
                parse_iso_millis(&room.created_at)
                    .ok_or_else(|| invalid_argument("invalid room creation timestamp"))?,
            );
            if decode_room(&stored)? != expected {
                return Err(invalid_argument("created room conflicts with tracked room"));
            }
        }
        sqlx::query(
            "UPDATE voice_create_reservations SET channel_id = $2, settled_at = now()
             WHERE id = $1 AND settled_at IS NULL",
        )
        .bind(reservation_id)
        .bind(room.channel_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    /// Persist a V2 ownership handoff on an already-tracked room. Only the
    /// owner fields move: seed, creator channel and timestamp stay insert-once
    /// like [`PgRoomStore::add_room`]. Returns false when no row exists.
    pub async fn update_ownership(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
        owner_id: Snowflake,
        original_creator_id: Snowflake,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE voice_rooms
             SET owner_id = $3, original_creator_id = $4, owner_touched_at = now()
             WHERE guild_id = $1 AND channel_id = $2",
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .bind(owner_id.to_string())
        .bind(original_creator_id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected()
            != 0)
    }

    /// Persist a V3 `/name` custom-name override on an already-tracked room,
    /// or clear it with `None` (restore). Only the name and its change stamp
    /// move: ownership, seed and creation stamp stay as they are. The text
    /// is stored as typed (template tokens intact); it must be 1-100
    /// characters, the same bound as the column's `CHECK`. Returns false when
    /// no row exists.
    pub async fn set_custom_name(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
        custom_name: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        if custom_name.is_some_and(|name| !(1..=100).contains(&name.chars().count())) {
            return Err(invalid_argument(
                "custom room name must be 1-100 characters",
            ));
        }
        Ok(sqlx::query(
            "UPDATE voice_rooms
             SET custom_name = $3, name_touched_at = now()
             WHERE guild_id = $1 AND channel_id = $2",
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .bind(custom_name)
        .execute(&self.pool)
        .await?
        .rows_affected()
            != 0)
    }

    /// Every custom-name override in a guild, by channel id. Rooms using
    /// their template name have no entry.
    pub async fn custom_names(
        &self,
        guild_id: Snowflake,
    ) -> Result<Vec<(Snowflake, String)>, sqlx::Error> {
        sqlx::query(
            "SELECT channel_id, custom_name FROM voice_rooms
             WHERE guild_id = $1 AND custom_name IS NOT NULL ORDER BY channel_id",
        )
        .bind(guild_id.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| Ok((decode_id(row, "channel_id")?, row.try_get("custom_name")?)))
        .collect()
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

    /// Claim one room create: serialize per guild, read the persisted history,
    /// run [`decide_admission`] and, when allowed, durably record the accepted
    /// reservation, all in one transaction. Nothing is written on a refusal.
    ///
    /// The per-guild advisory lock makes concurrent claims (two joins, two
    /// processes) see each other's reservation, so they cannot both pass the
    /// guild cap. `now_secs` is the caller's Unix-seconds clock; it stamps the
    /// reservation and anchors the window and cooldown, so the decision never
    /// depends on the database clock.
    ///
    /// Cap counts are live rooms (`voice_rooms`) plus all unsettled reservations.
    /// Age alone never proves absence: slow/repeated 429s, unknown POST outcomes
    /// and crashes retain capacity until evidence permits settlement.
    /// Burst and cooldown history comes from every reservation ever accepted,
    /// including rooms since deleted and rolled-back creates, so neither a
    /// deletion nor a restart frees a slot.
    pub async fn claim_create(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        config: &CreateAdmissionConfig,
        now_secs: i64,
    ) -> Result<CreateClaim, sqlx::Error> {
        if guild_id == 0 {
            return Err(invalid_argument("guild id must be nonzero"));
        }
        let guild = guild_id.to_string();
        let user = user_id.to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("voice_create:{guild}"))
            .execute(&mut *tx)
            .await?;
        let counts = sqlx::query(
            "SELECT
               (SELECT COUNT(*) FROM voice_rooms WHERE guild_id = $1)
                 + (SELECT COUNT(*) FROM voice_create_reservations
                     WHERE guild_id = $1 AND settled_at IS NULL) AS rooms_in_guild,
               (SELECT COUNT(*) FROM voice_rooms WHERE guild_id = $1 AND owner_id = $2)
                 + (SELECT COUNT(*) FROM voice_create_reservations
                     WHERE guild_id = $1 AND user_id = $2 AND settled_at IS NULL) AS owned_by_user,
               (SELECT floor(extract(epoch FROM max(created_at)))::bigint
                  FROM voice_create_reservations
                 WHERE guild_id = $1 AND user_id = $2) AS last_created_secs",
        )
        .bind(&guild)
        .bind(&user)
        .fetch_one(&mut *tx)
        .await?;
        let history = sqlx::query(
            "SELECT user_id, floor(extract(epoch FROM created_at))::bigint AS created_at_secs
             FROM voice_create_reservations
             WHERE guild_id = $1 AND created_at > to_timestamp($2::double precision)",
        )
        .bind(&guild)
        .bind(now_secs.saturating_sub(CREATE_BURST_WINDOW_SECS))
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|row| {
            Ok(AcceptedReservation {
                user_id: decode_id(row, "user_id")?,
                created_at_secs: row.try_get("created_at_secs")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let count = |column: &str| -> Result<u32, sqlx::Error> {
            Ok(u32::try_from(counts.try_get::<i64, _>(column)?).unwrap_or(u32::MAX))
        };
        let request = AdmissionRequest {
            user_id,
            owned_by_user: count("owned_by_user")?,
            rooms_in_guild: count("rooms_in_guild")?,
            last_created_at_secs: counts.try_get("last_created_secs")?,
            accepted_reservations: &history,
        };
        match decide_admission(config, &request, now_secs).map_err(invalid_argument)? {
            AdmissionDecision::Deny { reason } => Ok(CreateClaim::Refused(reason)),
            AdmissionDecision::Allow => {
                let reservation_id: String = sqlx::query_scalar(
                    "INSERT INTO voice_create_reservations (guild_id, user_id, created_at)
                     VALUES ($1, $2, to_timestamp($3::double precision))
                     RETURNING id",
                )
                .bind(&guild)
                .bind(&user)
                .bind(now_secs)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(CreateClaim::Admitted { reservation_id })
            }
        }
    }

    /// Bind a claim to the Discord channel its create returned, BEFORE the room
    /// row is written. This is the durable channel witness: if the room persist
    /// fails and the compensation delete is refused, a restarted worker still
    /// learns the channel from [`Self::orphaned_create_channels`] while the claim
    /// keeps holding its cap slot. Idempotent for the same channel; a claim that
    /// is settled or bound elsewhere refuses.
    pub async fn bind_create_channel(
        &self,
        guild_id: Snowflake,
        reservation_id: &str,
        channel_id: Snowflake,
    ) -> Result<(), sqlx::Error> {
        let bound = sqlx::query(
            "UPDATE voice_create_reservations SET channel_id = $3
             WHERE id = $1 AND guild_id = $2 AND settled_at IS NULL
               AND (channel_id IS NULL OR channel_id = $3)",
        )
        .bind(reservation_id)
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if bound == 0 {
            return Err(invalid_argument(
                "create reservation cannot be bound to this channel",
            ));
        }
        Ok(())
    }

    /// Unsettled claims bound to a created channel that has no tracked room:
    /// the channel exists (or may still exist) on Discord, its persist or
    /// compensation never finished, and the claim still holds its cap slot.
    /// Returns `(reservation id, channel id)` for worker load, so compensation
    /// survives a restart.
    pub async fn orphaned_create_channels(
        &self,
        guild_id: Snowflake,
    ) -> Result<Vec<(String, Snowflake)>, sqlx::Error> {
        sqlx::query(
            "SELECT r.id, r.channel_id FROM voice_create_reservations r
             WHERE r.guild_id = $1 AND r.settled_at IS NULL AND r.channel_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM voice_rooms v
                                WHERE v.guild_id = r.guild_id AND v.channel_id = r.channel_id)
             ORDER BY r.created_at, r.id",
        )
        .bind(guild_id.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("id")?,
                decode_id(row, "channel_id")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
    }

    /// Release a claim ONLY after confirmed no-side-effect failure, channel
    /// absence (404 or an authoritative snapshot) or a successful guarded
    /// delete. Unknown outcomes stay held. The row stays for burst/cooldown
    /// history. A claim whose channel is a tracked live room never settles
    /// here: that transfer is [`Self::persist_create`], not a separate room
    /// insert and settlement. Returns false when already settled or live.
    pub async fn settle_create(&self, reservation_id: &str) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE voice_create_reservations r
             SET settled_at = now()
             WHERE r.id = $1 AND r.settled_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM voice_rooms v
                                WHERE v.guild_id = r.guild_id AND v.channel_id = r.channel_id)",
        )
        .bind(reservation_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            != 0)
    }

    /// Record a companion created alongside its room, with the settings
    /// snapshot taken at creation. Insert-once like [`Self::add_room`]: later
    /// settings changes never update the snapshot in place.
    /// Returns false when a companion for this room is already tracked.
    pub async fn add_companion(&self, companion: &TextCompanion) -> Result<bool, sqlx::Error> {
        let millis = parse_iso_millis(&companion.created_at)
            .ok_or_else(|| invalid_argument("invalid companion creation timestamp"))?;
        let timestamp = OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
            .map_err(invalid_argument)?;
        Ok(sqlx::query(
            "INSERT INTO voice_text_companions
             (guild_id, room_channel_id, text_channel_id, text_channels,
              text_channel_name, text_viewer_role_id, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (guild_id, room_channel_id) DO NOTHING",
        )
        .bind(companion.guild_id.to_string())
        .bind(companion.room_channel_id.to_string())
        .bind(companion.text_channel_id.to_string())
        .bind(companion.settings.enabled)
        .bind(companion.settings.configured_name.clone())
        .bind(companion.settings.viewer_role_id.map(|id| id.to_string()))
        .bind(timestamp)
        .execute(&self.pool)
        .await?
        .rows_affected()
            != 0)
    }

    /// Every companion tracked in a guild, for worker load and startup
    /// reconciliation (V9c): each row carries its creation-time settings
    /// snapshot, so later `/textchannels` changes never alter it.
    pub async fn companions_in_guild(
        &self,
        guild_id: Snowflake,
    ) -> Result<Vec<TextCompanion>, sqlx::Error> {
        sqlx::query(
            "SELECT * FROM voice_text_companions WHERE guild_id = $1 ORDER BY room_channel_id",
        )
        .bind(guild_id.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(decode_companion)
        .collect()
    }

    pub async fn companion_for(
        &self,
        guild_id: Snowflake,
        room_channel_id: Snowflake,
    ) -> Result<Option<TextCompanion>, sqlx::Error> {
        sqlx::query(
            "SELECT * FROM voice_text_companions WHERE guild_id = $1 AND room_channel_id = $2",
        )
        .bind(guild_id.to_string())
        .bind(room_channel_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(decode_companion)
        .transpose()
    }

    /// Delete the companion tracked for a room (the runtime deletes the
    /// Discord text channel through the separate Discord call).
    pub async fn remove_companion(
        &self,
        guild_id: Snowflake,
        room_channel_id: Snowflake,
    ) -> Result<Option<TextCompanion>, sqlx::Error> {
        sqlx::query(
            "DELETE FROM voice_text_companions WHERE guild_id = $1 AND room_channel_id = $2 RETURNING *",
        )
        .bind(guild_id.to_string())
        .bind(room_channel_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(decode_companion)
        .transpose()
    }

    /// The guild's access controls, or the defaults when never configured.
    pub async fn access_controls(
        &self,
        guild_id: Snowflake,
    ) -> Result<AccessControls, sqlx::Error> {
        sqlx::query(
            "SELECT room_creation_enabled, required_role_id, command_roles::text AS command_roles
             FROM voice_access_controls WHERE guild_id = $1",
        )
        .bind(guild_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(decode_access_controls)
        .transpose()
        .map(Option::unwrap_or_default)
    }

    /// Replace the guild's access controls atomically (one upsert). Refuses
    /// unknown command names and zero role IDs before touching the database.
    pub async fn save_access_controls(
        &self,
        guild_id: Snowflake,
        controls: &AccessControls,
    ) -> Result<(), sqlx::Error> {
        validate_access_controls(controls).map_err(invalid_argument)?;
        let command_roles: BTreeMap<&str, Vec<String>> = controls
            .command_roles
            .iter()
            .map(|(command, roles)| {
                (
                    command.as_str(),
                    roles.iter().map(ToString::to_string).collect(),
                )
            })
            .collect();
        let command_roles = serde_json::to_string(&command_roles).map_err(invalid_argument)?;
        sqlx::query(
            "INSERT INTO voice_access_controls
             (guild_id, room_creation_enabled, required_role_id, command_roles)
             VALUES ($1,$2,$3,$4::jsonb)
             ON CONFLICT (guild_id) DO UPDATE SET
               room_creation_enabled = EXCLUDED.room_creation_enabled,
               required_role_id = EXCLUDED.required_role_id,
               command_roles = EXCLUDED.command_roles",
        )
        .bind(guild_id.to_string())
        .bind(controls.room_creation_enabled)
        .bind(controls.required_role.map(|role| role.to_string()))
        .bind(command_roles)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The guild's logging settings, or the defaults when never configured.
    pub async fn logging_settings(
        &self,
        guild_id: Snowflake,
    ) -> Result<LoggingSettings, sqlx::Error> {
        sqlx::query(
            "SELECT detail_level, log_channel_id, mention_role_id
             FROM voice_logging_settings WHERE guild_id = $1",
        )
        .bind(guild_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(decode_logging_settings)
        .transpose()
        .map(Option::unwrap_or_default)
    }

    /// Replace the guild's logging settings atomically (one upsert). Refuses
    /// zero channel and role IDs before touching the database.
    pub async fn save_logging_settings(
        &self,
        guild_id: Snowflake,
        settings: &LoggingSettings,
    ) -> Result<(), sqlx::Error> {
        if settings.channel_id == Some(0) || settings.mention_role_id == Some(0) {
            return Err(invalid_argument("logging ids must be nonzero"));
        }
        sqlx::query(
            "INSERT INTO voice_logging_settings
             (guild_id, detail_level, log_channel_id, mention_role_id)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (guild_id) DO UPDATE SET
               detail_level = EXCLUDED.detail_level,
               log_channel_id = EXCLUDED.log_channel_id,
               mention_role_id = EXCLUDED.mention_role_id",
        )
        .bind(guild_id.to_string())
        .bind(settings.level.as_str())
        .bind(settings.channel_id.map(|id| id.to_string()))
        .bind(settings.mention_role_id.map(|id| id.to_string()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn decode_access_controls(row: &PgRow) -> Result<AccessControls, sqlx::Error> {
    let parse_role = |value: &str| {
        value
            .parse::<u64>()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))
    };
    let required_role = row
        .try_get::<Option<String>, _>("required_role_id")?
        .map(|value| parse_role(&value))
        .transpose()?;
    let stored: BTreeMap<String, Vec<String>> =
        serde_json::from_str(row.try_get::<&str, _>("command_roles")?)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    let command_roles = stored
        .into_iter()
        .map(|(command, roles)| {
            roles
                .iter()
                .map(|role| parse_role(role))
                .collect::<Result<Vec<_>, _>>()
                .map(|roles| (command, roles))
        })
        .collect::<Result<_, _>>()?;
    let controls = AccessControls {
        room_creation_enabled: row.try_get("room_creation_enabled")?,
        required_role,
        command_roles,
    };
    validate_access_controls(&controls).map_err(invalid_argument)?;
    Ok(controls)
}

fn decode_logging_settings(row: &PgRow) -> Result<LoggingSettings, sqlx::Error> {
    let parse_id = |value: Option<String>| {
        value
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))
    };
    let level = parse_detail_level(row.try_get::<&str, _>("detail_level")?)
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    Ok(LoggingSettings {
        level,
        channel_id: parse_id(row.try_get("log_channel_id")?)?,
        mention_role_id: parse_id(row.try_get("mention_role_id")?)?,
    })
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
    let text_viewer_role_id = row
        .try_get::<Option<String>, _>("text_viewer_role_id")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))
        })
        .transpose()?;
    let creator = CreatorChannel {
        guild_id: decode_id(row, "guild_id")?,
        channel_id: decode_id(row, "channel_id")?,
        name_template: row.try_get("name_template")?,
        permission_source,
        permission_channel_id,
        default_limit: row
            .try_get::<Option<i32>, _>("default_limit")?
            .map(i64::from),
        private_default: row.try_get("private_default")?,
        text_channels: row.try_get("text_channels")?,
        text_channel_name: row.try_get("text_channel_name")?,
        text_viewer_role_id,
        position,
        first_room_number: row.try_get("first_room_number")?,
        group_by_category: row.try_get("group_by_category")?,
    };
    creator.validate().map_err(invalid_argument)?;
    Ok(creator)
}

fn decode_companion(row: &PgRow) -> Result<TextCompanion, sqlx::Error> {
    let timestamp: OffsetDateTime = row.try_get("created_at")?;
    let viewer_role_id = row
        .try_get::<Option<String>, _>("text_viewer_role_id")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))
        })
        .transpose()?;
    Ok(TextCompanion {
        guild_id: decode_id(row, "guild_id")?,
        room_channel_id: decode_id(row, "room_channel_id")?,
        text_channel_id: decode_id(row, "text_channel_id")?,
        settings: two_bot_core::voice_text_channel::TextChannelSettings {
            enabled: row.try_get("text_channels")?,
            configured_name: row.try_get("text_channel_name")?,
            viewer_role_id,
        },
        created_at: format_iso_millis((timestamp.unix_timestamp_nanos() / 1_000_000) as i64),
    })
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
