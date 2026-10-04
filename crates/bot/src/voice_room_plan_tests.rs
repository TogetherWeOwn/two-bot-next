use super::*;
use serde_json::json;
use twilight_model::guild::Role;
use two_bot_core::voice_permissions::{OWNER_ALLOW_BITS, PERM_CONNECT};

const GUILD: u64 = 100;
const CATEGORY: u64 = 400;
const CREATOR: u64 = 200;
const OWNER: u64 = 300;
const BOT: u64 = 999;

fn full() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::CONNECT
        | Permissions::MANAGE_CHANNELS
        | Permissions::MOVE_MEMBERS
        | Permissions::MANAGE_ROLES
}

fn everyone(permissions: Permissions) -> Role {
    serde_json::from_value(json!({
        "id": "100", "name": "everyone", "color": 0, "hoist": false,
        "managed": false, "mentionable": false, "position": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "permissions": permissions.bits().to_string(), "flags": 0
    }))
    .unwrap()
}

fn overwrite(id: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
    PermissionOverwrite {
        id: Id::new(id),
        kind: PermissionOverwriteType::Role,
        allow,
        deny,
    }
}

fn channel(
    id: u64,
    kind: u8,
    parent: Option<u64>,
    position: i32,
    overwrites: &[PermissionOverwrite],
) -> Channel {
    let mut channel: Channel = serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": GUILD.to_string(), "type": kind,
        "name": "chan", "parent_id": parent.map(|id| id.to_string()),
        "position": position, "bitrate": 96000, "rtc_region": "rotterdam",
        "video_quality_mode": 2, "nsfw": false, "user_limit": 8,
        "permission_overwrites": []
    }))
    .unwrap();
    channel.permission_overwrites = Some(overwrites.to_vec());
    channel
}

struct World {
    settings: CreatorChannel,
    channels: HashMap<Snowflake, Channel>,
    creators: HashMap<Snowflake, CreatorChannel>,
    rooms: HashMap<Snowflake, VoiceRoom>,
    bot: BotAccess,
}

impl World {
    /// Category 400 holding the creator (200, position 2) and one unrelated
    /// voice channel (500, position 5).
    fn new(bot_permissions: Permissions) -> Self {
        let settings = CreatorChannel::new(GUILD, CREATOR);
        let mut channels = HashMap::new();
        for channel in [
            channel(CATEGORY, 4, None, 0, &[]),
            channel(CREATOR, 2, Some(CATEGORY), 2, &[]),
            channel(500, 2, Some(CATEGORY), 5, &[]),
        ] {
            channels.insert(channel.id.get(), channel);
        }
        Self {
            creators: HashMap::from([(CREATOR, settings.clone())]),
            settings,
            channels,
            rooms: HashMap::new(),
            bot: BotAccess {
                member_id: BOT,
                guild_owner_id: 998,
                system_channel_id: None,
                member_roles: vec![],
                roles: vec![everyone(bot_permissions)],
            },
        }
    }

    fn set_overwrites(&mut self, id: u64, overwrites: &[PermissionOverwrite]) {
        self.channels.get_mut(&id).unwrap().permission_overwrites = Some(overwrites.to_vec());
    }

    fn plan(&self) -> Result<RoomChannelAttributes, RoomHttpError> {
        let creator = &self.channels[&CREATOR];
        let bot_permissions = effective_permissions(
            GUILD,
            self.bot.guild_owner_id,
            self.bot.member_id,
            &self.bot.member_roles,
            &self.bot.roles,
            creator.permission_overwrites.as_deref().unwrap_or_default(),
        );
        let grouped = self.settings.group_by_category;
        let group_room_ids = if grouped {
            category_room_ids(creator, &self.channels, &self.rooms)
        } else {
            Vec::new()
        };
        plan_room(&RoomPlanInput {
            guild_id: GUILD,
            owner_id: OWNER,
            settings: &self.settings,
            creator,
            channels: &self.channels,
            creators: &self.creators,
            rooms: &self.rooms,
            bot: &self.bot,
            bot_permissions,
            grouped,
            group_room_ids: &group_room_ids,
        })
    }

    /// Track one room already in the creator's category.
    fn add_room(&mut self, id: u64, position: i32) {
        self.channels
            .insert(id, channel(id, 2, Some(CATEGORY), position, &[]));
        self.rooms.insert(
            id,
            VoiceRoom {
                guild_id: GUILD,
                channel_id: id,
                creator_channel_id: CREATOR,
                owner_id: 301,
                original_creator_id: 301,
                name_seed: 1,
                created_at: "2026-10-02T00:00:00.000000+00:00".to_owned(),
            },
        );
    }
}

fn entry(attributes: &RoomChannelAttributes, id: u64) -> Option<&PermissionOverwrite> {
    attributes
        .overwrites
        .iter()
        .find(|overwrite| overwrite.id.get() == id)
}

#[test]
fn owner_override_is_included_at_creation_and_never_escalates() {
    let attributes = World::new(full()).plan().unwrap();
    let owner = entry(&attributes, OWNER).expect("owner override");
    assert_eq!(owner.kind, PermissionOverwriteType::Member);
    assert_eq!(owner.allow.bits() & OWNER_ALLOW_BITS, OWNER_ALLOW_BITS);
    assert!(!owner
        .allow
        .intersects(Permissions::MANAGE_ROLES | Permissions::ADMINISTRATOR));
    // A public room leaves @everyone alone and the bot needs no grant.
    assert!(entry(&attributes, GUILD).is_none());
    assert!(entry(&attributes, BOT).is_none());
}

#[test]
fn private_default_denies_connect_and_keeps_the_bot_able_to_move_the_owner() {
    let mut world = World::new(full());
    world.settings.private_default = true;
    let attributes = world.plan().unwrap();
    let everyone = entry(&attributes, GUILD).expect("@everyone override");
    assert!(everyone.deny.contains(Permissions::CONNECT));
    assert_eq!(PERM_CONNECT, Permissions::CONNECT.bits());
    let bot = entry(&attributes, BOT).expect("bot keeps minimum access");
    assert_eq!(bot.kind, PermissionOverwriteType::Member);
    assert!(bot.allow.contains(
        Permissions::VIEW_CHANNEL
            | Permissions::CONNECT
            | Permissions::MANAGE_CHANNELS
            | Permissions::MOVE_MEMBERS
    ));
    assert!(!bot
        .allow
        .intersects(Permissions::MANAGE_ROLES | Permissions::ADMINISTRATOR));
}

#[test]
fn without_manage_roles_a_public_room_syncs_to_its_category() {
    let mut world = World::new(full() & !Permissions::MANAGE_ROLES);
    world.set_overwrites(
        CATEGORY,
        &[overwrite(
            555,
            Permissions::empty(),
            Permissions::SEND_MESSAGES,
        )],
    );
    // The bot cannot set overrides, so none are sent: Discord syncs the room.
    assert_eq!(world.plan().unwrap().overwrites, []);
}

#[test]
fn without_manage_roles_a_private_default_is_refused_not_made_public() {
    let mut world = World::new(full() & !Permissions::MANAGE_ROLES);
    world.settings.private_default = true;
    assert_eq!(world.plan(), Err(RoomHttpError::AccessDenied));
}

#[test]
fn inheritance_source_picks_which_channel_overrides_are_copied() {
    let creator_rule = overwrite(601, Permissions::empty(), Permissions::SEND_MESSAGES);
    let category_rule = overwrite(602, Permissions::empty(), Permissions::SPEAK);
    let chosen_rule = overwrite(603, Permissions::empty(), Permissions::STREAM);
    let mut world = World::new(full());
    world.set_overwrites(CREATOR, &[creator_rule]);
    world.set_overwrites(CATEGORY, &[category_rule]);
    let chosen = channel(700, 2, None, 0, &[chosen_rule]);
    world.channels.insert(700, chosen);

    let copied = |world: &World| -> Vec<u64> {
        world
            .plan()
            .unwrap()
            .overwrites
            .iter()
            .map(|overwrite| overwrite.id.get())
            .filter(|id| (601..=603).contains(id))
            .collect()
    };
    assert_eq!(copied(&world), [601]);
    world.settings.permission_source = PermissionSource::Category;
    assert_eq!(copied(&world), [602]);
    world.settings.permission_source = PermissionSource::Channel(700);
    world.settings.permission_channel_id = Some(700);
    assert_eq!(copied(&world), [603]);
}

#[test]
fn a_missing_chosen_source_channel_fails_closed() {
    let mut world = World::new(full());
    world.settings.permission_source = PermissionSource::Channel(701);
    world.settings.permission_channel_id = Some(701);
    assert_eq!(world.plan(), Err(RoomHttpError::AccessDenied));
}

#[test]
fn the_text_channel_toggle_does_not_change_the_voice_room_plan() {
    // The companion is created by the worker (V9c); the voice room itself is
    // planned identically whether or not the creator enables the toggle.
    let mut world = World::new(full());
    let without = world.plan().unwrap();
    world.settings.text_channels = true;
    assert_eq!(world.plan().unwrap(), without);
}

#[test]
fn starting_limit_comes_from_the_creator_default_else_the_creator_channel() {
    let mut world = World::new(full());
    assert_eq!(world.plan().unwrap().user_limit, 8);
    world.settings.default_limit = Some(0);
    assert_eq!(world.plan().unwrap().user_limit, 0);
    world.settings.default_limit = Some(5);
    assert_eq!(world.plan().unwrap().user_limit, 5);
    world.settings.default_limit = Some(100);
    assert_eq!(world.plan(), Err(RoomHttpError::InvalidRequest));
}

#[test]
fn placement_takes_the_slot_next_to_the_creator() {
    let mut world = World::new(full());
    // Above the creator (position 2): it takes the creator's slot.
    assert_eq!(world.plan().unwrap().position, Some(2));
    // Below the creator: it takes the slot of the channel that follows.
    world.settings.position = RoomPosition::Below;
    assert_eq!(world.plan().unwrap().position, Some(5));
}

#[test]
fn placement_below_the_last_channel_appends() {
    let mut world = World::new(full());
    world.settings.position = RoomPosition::Below;
    world.channels.remove(&500);
    assert_eq!(world.plan().unwrap().position, Some(3));
}

#[test]
fn rooms_and_other_categories_do_not_change_the_creators_slot() {
    let mut world = World::new(full());
    // A room already below the creator and a channel in another category.
    let existing = channel(510, 2, Some(CATEGORY), 3, &[]);
    world.channels.insert(510, existing);
    world.rooms.insert(
        510,
        VoiceRoom {
            guild_id: GUILD,
            channel_id: 510,
            creator_channel_id: CREATOR,
            owner_id: 301,
            original_creator_id: 301,
            name_seed: 1,
            created_at: "2026-10-02T00:00:00.000000+00:00".to_owned(),
        },
    );
    world
        .channels
        .insert(900, channel(900, 2, Some(901), 0, &[]));
    world.settings.position = RoomPosition::Below;
    // Existing rooms are never moved: the new room sits directly below the
    // creator and pushes room 510 (position 3) down.
    assert_eq!(world.plan().unwrap().position, Some(3));
}

#[test]
fn grouped_rooms_keep_a_contiguous_block_at_the_block_edge() {
    let mut world = World::new(full());
    world.settings.group_by_category = true;
    world.add_room(510, 3);
    world.add_room(511, 4);
    // Below: after the last group room, taking channel 500's slot (5) and
    // leaving [creator, 510, 511, new] contiguous.
    world.settings.position = RoomPosition::Below;
    assert_eq!(world.plan().unwrap().position, Some(5));
    // Above: before the first group room, taking room 510's slot (3).
    world.settings.position = RoomPosition::Above;
    assert_eq!(world.plan().unwrap().position, Some(3));
}

#[test]
fn grouped_without_rooms_starts_the_block_next_to_the_creator() {
    let mut world = World::new(full());
    world.settings.group_by_category = true;
    assert_eq!(world.plan().unwrap().position, Some(2));
    world.settings.position = RoomPosition::Below;
    assert_eq!(world.plan().unwrap().position, Some(5));
}

#[test]
fn the_group_set_covers_only_live_rooms_in_the_category() {
    let mut world = World::new(full());
    world.settings.group_by_category = true;
    world.add_room(510, 3);
    // Another category: not in the group.
    world
        .channels
        .insert(900, channel(900, 2, Some(901), 0, &[]));
    world.rooms.insert(
        900,
        VoiceRoom {
            guild_id: GUILD,
            channel_id: 900,
            creator_channel_id: CREATOR,
            owner_id: 301,
            original_creator_id: 301,
            name_seed: 1,
            created_at: "2026-10-02T00:00:00.000000+00:00".to_owned(),
        },
    );
    // Tracked but hand-deleted (no live channel): not in the group.
    world.rooms.insert(
        911,
        VoiceRoom {
            guild_id: GUILD,
            channel_id: 911,
            creator_channel_id: CREATOR,
            owner_id: 301,
            original_creator_id: 301,
            name_seed: 1,
            created_at: "2026-10-02T00:00:00.000000+00:00".to_owned(),
        },
    );
    let creator = world.channels[&CREATOR].clone();
    assert_eq!(
        category_room_ids(&creator, &world.channels, &world.rooms),
        vec![510]
    );
    // ... so planning still lands at the block edge, not past the strangers.
    world.settings.position = RoomPosition::Below;
    assert_eq!(world.plan().unwrap().position, Some(5));
}

#[test]
fn a_bot_member_deny_surviving_the_grant_is_refused_not_weakened() {
    // A source deny on the bot still wins after `grant_bot` adds the minimum
    // room access, so the room is refused instead of created unmanageable.
    let mut world = World::new(full());
    world.set_overwrites(
        CREATOR,
        &[PermissionOverwrite {
            id: Id::new(BOT),
            kind: PermissionOverwriteType::Member,
            allow: Permissions::empty(),
            deny: Permissions::CONNECT,
        }],
    );
    assert_eq!(world.plan(), Err(RoomHttpError::AccessDenied));
}

#[test]
fn an_unknown_source_overwrite_kind_is_refused() {
    // An overwrite kind with no core mapping cannot be honoured: fail
    // closed instead of silently dropping the rule.
    let mut world = World::new(full());
    world.set_overwrites(
        CREATOR,
        &[PermissionOverwrite {
            id: Id::new(601),
            kind: PermissionOverwriteType::from(99u8),
            allow: Permissions::empty(),
            deny: Permissions::empty(),
        }],
    );
    assert_eq!(world.plan(), Err(RoomHttpError::InvalidRequest));
}

#[test]
fn without_manage_roles_an_unmanageable_category_is_refused_not_synced() {
    // Syncing would leave a room the bot cannot manage; refuse instead.
    let mut world = World::new(full() & !Permissions::MANAGE_ROLES);
    world.set_overwrites(
        CATEGORY,
        &[overwrite(GUILD, Permissions::empty(), Permissions::CONNECT)],
    );
    assert_eq!(world.plan(), Err(RoomHttpError::AccessDenied));
}

#[test]
fn a_zero_id_override_is_refused_at_conversion() {
    assert_eq!(
        to_twilight(&[ChannelOverride {
            id: 0,
            kind: OverrideKind::Member,
            allow: 1,
            deny: 0,
        }]),
        Err(RoomHttpError::InvalidRequest)
    );
}
