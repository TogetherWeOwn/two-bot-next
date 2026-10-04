//! V8 creation-time plan for one temporary voice room: where it goes, which
//! overrides it is created with, and its starting limit and privacy.
//!
//! This glues the pure cores (`voice_placement`, `voice_permissions`) to the
//! live guild snapshot. It performs no I/O: the worker passes in what it
//! already holds and gets the exact attributes for the single create call.
//! Overrides are included at creation and never patched afterwards.

use std::collections::HashMap;

use twilight_model::{
    channel::{
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
        Channel, ChannelType,
    },
    guild::Permissions,
    id::Id,
};
use two_bot_core::{
    voice_config::RoomPosition as PlacementSide,
    voice_permissions::{
        plan_room_overrides, ChannelOverride, InheritanceSource, OverrideKind, RoomPermissionInput,
        RoomPermissionPlan,
    },
    voice_placement::{
        plan_placement, position_for_index, resolve_initial_state, CategoryChannel,
        CategoryEntryKind, PlacementRequest,
    },
    voice_rooms::{CreatorChannel, PermissionSource, RoomPosition, VoiceRoom},
    Snowflake,
};
use two_bot_discord::voice_rooms::{
    can_manage_room, effective_permissions, RoomChannelAttributes, RoomHttpError,
};

use crate::voice_rooms::BotAccess;

/// What the bot itself needs on a room it creates so it can still move the
/// owner in. Granted only when the room's overrides would otherwise take it
/// away (a private room denies Connect to @everyone, which includes the bot's
/// guild-level grant).
const BOT_ROOM_ACCESS: u64 = (Permissions::VIEW_CHANNEL.bits())
    | Permissions::CONNECT.bits()
    | Permissions::MANAGE_CHANNELS.bits()
    | Permissions::MOVE_MEMBERS.bits();

pub(crate) struct RoomPlanInput<'a> {
    pub guild_id: Snowflake,
    /// The joining member: the new room's owner.
    pub owner_id: Snowflake,
    pub settings: &'a CreatorChannel,
    pub creator: &'a Channel,
    pub channels: &'a HashMap<Snowflake, Channel>,
    pub creators: &'a HashMap<Snowflake, CreatorChannel>,
    pub rooms: &'a HashMap<Snowflake, VoiceRoom>,
    pub bot: &'a BotAccess,
    /// The bot's effective permissions on the creator channel.
    pub bot_permissions: Option<Permissions>,
    /// That creator's `/group` flag: shared numbering and a contiguous room
    /// block per category. False keeps creator-adjacent placement.
    pub grouped: bool,
    /// Existing rooms in the same group (the creator's category). Must be
    /// empty when `grouped` is false.
    pub group_room_ids: &'a [Snowflake],
}

/// Tracked rooms still in the creator's category: the `/group` room set for
/// [`plan_placement`]. Rooms whose channel is gone (hand-deleted) or moved to
/// another category are left out, so every id here is a `Room` entry in the
/// category order [`placement`] builds.
pub(crate) fn category_room_ids(
    creator: &Channel,
    channels: &HashMap<Snowflake, Channel>,
    rooms: &HashMap<Snowflake, VoiceRoom>,
) -> Vec<Snowflake> {
    let mut ids: Vec<Snowflake> = rooms
        .keys()
        .filter(|id| {
            channels
                .get(*id)
                .is_some_and(|channel| channel.parent_id == creator.parent_id)
        })
        .copied()
        .collect();
    ids.sort_unstable();
    ids
}

fn to_core(overwrites: &[PermissionOverwrite]) -> Result<Vec<ChannelOverride>, RoomHttpError> {
    overwrites
        .iter()
        .map(|overwrite| {
            let kind = match overwrite.kind {
                PermissionOverwriteType::Role => OverrideKind::Role,
                PermissionOverwriteType::Member => OverrideKind::Member,
                _ => return Err(RoomHttpError::InvalidRequest),
            };
            Ok(ChannelOverride {
                id: overwrite.id.get(),
                kind,
                allow: overwrite.allow.bits(),
                deny: overwrite.deny.bits(),
            })
        })
        .collect()
}

fn to_twilight(list: &[ChannelOverride]) -> Result<Vec<PermissionOverwrite>, RoomHttpError> {
    list.iter()
        .map(|entry| {
            Ok(PermissionOverwrite {
                id: Id::new_checked(entry.id).ok_or(RoomHttpError::InvalidRequest)?,
                kind: match entry.kind {
                    OverrideKind::Role => PermissionOverwriteType::Role,
                    OverrideKind::Member => PermissionOverwriteType::Member,
                },
                allow: Permissions::from_bits_retain(entry.allow),
                deny: Permissions::from_bits_retain(entry.deny),
            })
        })
        .collect()
}

/// Give the bot its minimum room access. A deny already on the bot's own
/// entry still wins, so the caller re-checks access afterwards.
fn grant_bot(list: &mut Vec<ChannelOverride>, bot_id: Snowflake) {
    match list
        .iter_mut()
        .find(|entry| entry.id == bot_id && entry.kind == OverrideKind::Member)
    {
        Some(entry) => {
            entry.allow = (entry.allow | BOT_ROOM_ACCESS) & !entry.deny;
        }
        None => list.push(ChannelOverride {
            id: bot_id,
            kind: OverrideKind::Member,
            allow: BOT_ROOM_ACCESS,
            deny: 0,
        }),
    }
}

fn bot_can_manage(
    guild_id: Snowflake,
    bot: &BotAccess,
    overwrites: &[PermissionOverwrite],
) -> bool {
    can_manage_room(effective_permissions(
        guild_id,
        bot.guild_owner_id,
        bot.member_id,
        &bot.member_roles,
        &bot.roles,
        overwrites,
    ))
}

/// Where in the creator's category the new room goes, as a Discord position.
/// Placement is best effort: a category whose order cannot be planned leaves
/// the position to Discord (the room is appended) rather than blocking the
/// join.
fn placement(input: &RoomPlanInput<'_>) -> Option<u64> {
    let parent = input.creator.parent_id;
    let order: Vec<CategoryChannel> = input
        .channels
        .values()
        .filter(|channel| channel.parent_id == parent && channel.kind != ChannelType::GuildCategory)
        .map(|channel| {
            let id = channel.id.get();
            CategoryChannel {
                id,
                position: channel.position.unwrap_or(0),
                kind: if input.creators.contains_key(&id) {
                    CategoryEntryKind::Creator
                } else if input.rooms.contains_key(&id) {
                    CategoryEntryKind::Room
                } else {
                    CategoryEntryKind::Other
                },
            }
        })
        .collect();
    let side = match input.settings.position {
        RoomPosition::Above => PlacementSide::Above,
        RoomPosition::Below => PlacementSide::Below,
    };
    let index = plan_placement(PlacementRequest {
        creator_id: input.creator.id.get(),
        side,
        grouped: input.grouped,
        group_room_ids: input.group_room_ids,
        category_order: &order,
    })
    .ok()?;
    Some(position_for_index(&order, index))
}

/// Plan one room. Errors are sanitized `RoomHttpError`s the worker already
/// knows how to record: `AccessDenied` when the bot cannot build the room as
/// configured (never silently weaker), `InvalidRequest` for settings or
/// snapshot data that cannot be honoured.
pub(crate) fn plan_room(input: &RoomPlanInput<'_>) -> Result<RoomChannelAttributes, RoomHttpError> {
    let settings = input.settings;
    // The companion text channel (`settings.text_channels`) is not part of the
    // voice-channel attributes: the worker creates it through the same
    // per-guild queue (V9c), so the toggle never blocks room planning.
    let base_limit = match settings.default_limit {
        Some(limit) => u16::try_from(limit).map_err(|_| RoomHttpError::InvalidRequest)?,
        None => u16::try_from(input.creator.user_limit.unwrap_or(0))
            .map_err(|_| RoomHttpError::InvalidRequest)?,
    };
    let initial = resolve_initial_state(base_limit, settings.private_default)
        .map_err(|_| RoomHttpError::InvalidRequest)?;

    let parent = input.creator.parent_id.map(Id::get);
    let bot_can_manage_roles = input
        .bot_permissions
        .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_ROLES));
    let (source, source_channel) = match settings.permission_source {
        PermissionSource::Creator => (
            InheritanceSource::CreatorChannel,
            Some(input.creator.id.get()),
        ),
        PermissionSource::Category => (InheritanceSource::Category, parent),
        PermissionSource::Channel(id) => (InheritanceSource::ChosenChannel, Some(id)),
    };
    let source_overrides = match (bot_can_manage_roles, source_channel) {
        (true, Some(id)) => to_core(
            input
                .channels
                .get(&id)
                .ok_or(RoomHttpError::AccessDenied)?
                .permission_overwrites
                .as_deref()
                .unwrap_or_default(),
        )?,
        _ => Vec::new(),
    };
    let plan = plan_room_overrides(&RoomPermissionInput {
        source,
        source_overrides: &source_overrides,
        bot_can_manage_roles,
        owner_id: input.owner_id,
        owner_extra_allow: 0,
        private: initial.private,
        everyone_role_id: input.guild_id,
        required_role: None,
    })
    .map_err(|_| RoomHttpError::InvalidRequest)?;

    let bot = input.bot;
    let overwrites = match plan {
        RoomPermissionPlan::Overrides(mut list) => {
            let mut evaluated = to_twilight(&list)?;
            if !bot_can_manage(input.guild_id, bot, &evaluated) {
                grant_bot(&mut list, bot.member_id);
                evaluated = to_twilight(&list)?;
                if !bot_can_manage(input.guild_id, bot, &evaluated) {
                    return Err(RoomHttpError::AccessDenied);
                }
            }
            evaluated
        }
        RoomPermissionPlan::SyncToCategory => {
            // Without Manage Roles the bot cannot set overrides at all, so it
            // cannot honour a private default or the owner's extra access. Do
            // not fall back to a public room.
            if initial.private {
                return Err(RoomHttpError::AccessDenied);
            }
            let category = parent
                .and_then(|id| input.channels.get(&id))
                .and_then(|channel| channel.permission_overwrites.as_deref())
                .unwrap_or_default();
            if !bot_can_manage(input.guild_id, bot, category) {
                return Err(RoomHttpError::AccessDenied);
            }
            Vec::new()
        }
    };

    let mut attributes = RoomChannelAttributes::from_creator(settings, input.creator, overwrites)?;
    attributes.user_limit = u16::from(initial.user_limit);
    attributes.position = placement(input);
    Ok(attributes)
}

#[cfg(test)]
#[path = "voice_room_plan_tests.rs"]
mod tests;
