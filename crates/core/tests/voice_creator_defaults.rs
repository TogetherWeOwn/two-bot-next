//! Hermetic V8 per-creator defaults acceptance against the public core API.
//!
//! Each creator channel carries its own validated settings (`first_number`,
//! `/position`, `/group`, `/defaultlimit`, `/alwaysprivate`, channel bitrate).
//! The runtime routes the *triggering* creator's settings into the pure cores;
//! this fixture pins that wiring contract without any database, Redis,
//! Discord, clock, or staging identity:
//!
//! - `next_room_number` starts from the triggering creator's `first_number`.
//! - `plan_placement` uses the triggering creator's `position`/`group` flag.
//! - `resolve_initial_state` + `room_bitrate` apply the triggering creator's
//!   limit/privacy/bitrate defaults inside the tier maximum.
//! - A creator without overrides resolves identically to the guild defaults.
//! - Two creators' defaults never leak into each other (pure calls, plus a
//!   codec round-trip proving persistence keeps them separate).

use std::collections::BTreeMap;

use two_bot_core::voice_config::{
    export_configuration, import_configuration, ChannelKind, ChannelReference,
    CreatorConfiguration, GuildInventory, GuildSettings, PermissionSource, RoomPosition,
    VoiceConfiguration,
};
use two_bot_core::voice_placement::{
    next_room_number, plan_placement, resolve_initial_state, CategoryChannel, CategoryEntryKind,
    PlacementRequest, RoomInitialState,
};
use two_bot_core::voice_room_controls::{
    room_bitrate, tier_max_bps, validate_bitrate_preference, BitrateTier,
};

// ---- fixtures: per-creator settings as the runtime reads them ----

fn creator_config(
    channel_id: &str,
    default_limit: u16,
    always_private: bool,
    position: RoomPosition,
    first_number: u32,
    grouped: bool,
) -> CreatorConfiguration {
    CreatorConfiguration {
        channel_id: channel_id.to_owned(),
        name_template: "@@owner@@ ##".to_owned(),
        status_template: None,
        default_limit,
        always_private,
        text_channels: false,
        position,
        first_number,
        group_by_category: grouped,
        permission_source: PermissionSource::Creator {},
    }
}

/// Guild-wide fallbacks the runtime substitutes when a creator has no
/// override. Neutral values only; every assertion below compares these
/// against the same pure calls made with a neutral creator's fields.
const GUILD_FIRST_NUMBER: u32 = 1;
const GUILD_DEFAULT_LIMIT: u16 = 0;
const GUILD_ALWAYS_PRIVATE: bool = false;
const GUILD_POSITION: RoomPosition = RoomPosition::Below;
const GUILD_GROUPED: bool = false;
const GUILD_BITRATE_DEFAULT: u32 = 32_000;

fn creator(id: u64, position: i32) -> CategoryChannel {
    CategoryChannel {
        id,
        position,
        kind: CategoryEntryKind::Creator,
    }
}

fn room(id: u64, position: i32) -> CategoryChannel {
    CategoryChannel {
        id,
        position,
        kind: CategoryEntryKind::Room,
    }
}

fn other(id: u64, position: i32) -> CategoryChannel {
    CategoryChannel {
        id,
        position,
        kind: CategoryEntryKind::Other,
    }
}

fn placement_for(
    creator: &CreatorConfiguration,
    creator_id: u64,
    group_room_ids: &[u64],
    category_order: &[CategoryChannel],
) -> Result<usize, two_bot_core::voice_placement::PlacementError> {
    plan_placement(PlacementRequest {
        creator_id,
        side: creator.position,
        grouped: creator.group_by_category,
        group_room_ids,
        category_order,
    })
}

// ---- (1) numbering starts from the triggering creator's first_number ----

#[test]
fn numbering_skips_taken_numbers_from_creator_first_number() {
    let creator_a = creator_config("101", 0, false, RoomPosition::Below, 5, false);
    // Taken numbers at and above the creator's start are skipped; the gap
    // at 7 is filled instead of appending.
    assert_eq!(next_room_number(&[5, 6, 8], creator_a.first_number), 7);
    assert_eq!(next_room_number(&[5, 7], creator_a.first_number), 6);
    // Numbers below the creator's start are ignored, so rooms created under
    // an older, lower start keep their numbers after an admin raises it.
    assert_eq!(next_room_number(&[1, 2, 3], creator_a.first_number), 5);
    assert_eq!(next_room_number(&[1, 2, 5, 6], creator_a.first_number), 7);
    // Empty rooms start exactly at the creator's first number.
    assert_eq!(next_room_number(&[], creator_a.first_number), 5);
}

#[test]
fn numbering_start_above_one_uses_each_creator_first_number() {
    let creator_a = creator_config("101", 0, false, RoomPosition::Below, 5, false);
    let creator_b = creator_config("102", 0, false, RoomPosition::Below, 10, false);
    // Same taken set, different starts: each creator numbers from its own
    // first_number, proving the start is per-creator input, not shared state.
    assert_eq!(next_room_number(&[10, 11], creator_b.first_number), 12);
    assert_eq!(next_room_number(&[10, 12], creator_b.first_number), 11);
    assert_eq!(next_room_number(&[5, 6, 10, 11], creator_a.first_number), 7);
    assert_eq!(
        next_room_number(&[5, 6, 10, 11], creator_b.first_number),
        12
    );
}

// ---- (2) placement uses the triggering creator's position/group ----

#[test]
fn placement_picks_configured_category_position_for_creator_with_defaults() {
    let creator_a = creator_config("101", 0, false, RoomPosition::Below, 1, false);
    let creator_b = creator_config("102", 0, false, RoomPosition::Above, 1, false);
    // Category: creator A, unrelated channel, creator B.
    let category = [creator(10, 0), other(30, 1), creator(20, 2)];
    // Each creator's own `/position` decides its own anchor; existing
    // channels never move.
    assert_eq!(placement_for(&creator_a, 10, &[], &category), Ok(1));
    assert_eq!(placement_for(&creator_b, 20, &[], &category), Ok(2));
    // Swapping the configs swaps the answers, so the decision follows the
    // triggering creator's settings, not the channel id.
    assert_eq!(
        plan_placement(PlacementRequest {
            creator_id: 10,
            side: creator_b.position,
            grouped: creator_b.group_by_category,
            group_room_ids: &[],
            category_order: &category,
        }),
        Ok(0)
    );
    assert_eq!(
        plan_placement(PlacementRequest {
            creator_id: 20,
            side: creator_a.position,
            grouped: creator_a.group_by_category,
            group_room_ids: &[],
            category_order: &category,
        }),
        Ok(3)
    );
}

#[test]
fn placement_group_flag_comes_from_triggering_creator() {
    let grouped = creator_config("101", 0, false, RoomPosition::Below, 1, true);
    let ungrouped = creator_config("102", 0, false, RoomPosition::Below, 1, false);
    // Shared group block spanning two creators plus an unrelated tail.
    let category = [
        creator(10, 0),
        room(11, 1),
        creator(20, 2),
        room(21, 3),
        other(30, 4),
    ];
    let group = [11, 21];
    // The grouped creator extends the block edge; the ungrouped creator
    // stays adjacent to its own channel even in the same category.
    assert_eq!(placement_for(&grouped, 10, &group, &category), Ok(4));
    assert_eq!(placement_for(&ungrouped, 20, &[], &category), Ok(3));
}

// ---- (3) initial state applies creator limit/privacy/bitrate in range ----

#[test]
fn initial_state_applies_creator_limit_and_privacy_defaults() {
    let creator_a = creator_config("101", 4, true, RoomPosition::Below, 1, false);
    assert_eq!(
        resolve_initial_state(creator_a.default_limit, creator_a.always_private),
        Ok(RoomInitialState {
            user_limit: 4,
            private: true,
        })
    );
    let creator_b = creator_config("102", 0, false, RoomPosition::Above, 1, false);
    assert_eq!(
        resolve_initial_state(creator_b.default_limit, creator_b.always_private),
        Ok(RoomInitialState {
            user_limit: 0,
            private: false,
        })
    );
    // Bounds are exact: 99 accepted, 100 refused rather than clamped.
    assert_eq!(
        resolve_initial_state(99, false),
        Ok(RoomInitialState {
            user_limit: 99,
            private: false,
        })
    );
    assert!(resolve_initial_state(100, false).is_err());
}

#[test]
fn creator_bitrate_default_applies_within_tier_max() {
    let tier_max = tier_max_bps(BitrateTier::Base);
    assert_eq!(tier_max, 64_000);
    // Creator A's channel bitrate is a valid preference inside the tier max.
    let creator_bitrate_a = 48_000;
    assert_eq!(
        validate_bitrate_preference(creator_bitrate_a, tier_max),
        Ok(48_000)
    );
    // No member preferences: the room falls back to the creator default.
    assert_eq!(
        room_bitrate(&[], creator_bitrate_a, tier_max),
        creator_bitrate_a
    );
    assert_eq!(
        room_bitrate(&[None, None], creator_bitrate_a, tier_max),
        creator_bitrate_a
    );
    // Member preferences average (floor) and stay inside the tier bounds.
    assert_eq!(
        room_bitrate(&[Some(32_000), Some(64_000)], creator_bitrate_a, tier_max),
        48_000
    );
    // Out-of-range values are refused at set time, while the room average
    // (including its creator-default fallback) clamps into range.
    assert!(validate_bitrate_preference(8_000, tier_max).is_err());
    assert!(validate_bitrate_preference(tier_max + 1, tier_max).is_err());
    assert_eq!(room_bitrate(&[], 1_000, tier_max), 8_001);
    assert_eq!(room_bitrate(&[], 96_000, tier_max), tier_max);
}

// ---- (4) a creator without defaults falls back to guild defaults ----

#[test]
fn creator_without_defaults_falls_back_to_guild_defaults() {
    let neutral = creator_config(
        "101",
        GUILD_DEFAULT_LIMIT,
        GUILD_ALWAYS_PRIVATE,
        GUILD_POSITION,
        GUILD_FIRST_NUMBER,
        GUILD_GROUPED,
    );
    // Limit/privacy: neutral creator fields resolve exactly like the guild
    // defaults through the same pure function.
    assert_eq!(
        resolve_initial_state(neutral.default_limit, neutral.always_private),
        resolve_initial_state(GUILD_DEFAULT_LIMIT, GUILD_ALWAYS_PRIVATE)
    );
    // Numbering: neutral first_number starts where the guild starts.
    assert_eq!(
        next_room_number(&[], neutral.first_number),
        next_room_number(&[], GUILD_FIRST_NUMBER)
    );
    assert_eq!(next_room_number(&[], GUILD_FIRST_NUMBER), 1);
    // Bitrate: with no per-creator bitrate configured the room falls back to
    // the guild bitrate default (in range, so it passes through unchanged).
    let tier_max = tier_max_bps(BitrateTier::Base);
    assert_eq!(
        room_bitrate(&[], GUILD_BITRATE_DEFAULT, tier_max),
        GUILD_BITRATE_DEFAULT
    );
    assert_eq!(
        room_bitrate(&[None, None], GUILD_BITRATE_DEFAULT, tier_max),
        GUILD_BITRATE_DEFAULT
    );
    // Placement: neutral position/group places where the guild default would.
    let category = [creator(10, 0), other(30, 1)];
    assert_eq!(
        placement_for(&neutral, 10, &[], &category),
        plan_placement(PlacementRequest {
            creator_id: 10,
            side: GUILD_POSITION,
            grouped: GUILD_GROUPED,
            group_room_ids: &[],
            category_order: &category,
        })
    );
}

// ---- (5) per-creator defaults never leak across creators ----

#[test]
fn per_creator_defaults_never_leak_across_creators() {
    let creator_a = creator_config("101", 4, true, RoomPosition::Below, 5, false);
    let creator_b = creator_config("102", 7, false, RoomPosition::Above, 10, false);
    let bitrate_a = 48_000;
    let bitrate_b = 32_000;
    let tier_max = tier_max_bps(BitrateTier::Base);

    // Limits/privacy: interleaved calls return each creator's own values,
    // and re-reading A after B is unchanged.
    let state_a = resolve_initial_state(creator_a.default_limit, creator_a.always_private);
    let state_b = resolve_initial_state(creator_b.default_limit, creator_b.always_private);
    assert_eq!(
        state_a,
        Ok(RoomInitialState {
            user_limit: 4,
            private: true,
        })
    );
    assert_eq!(
        state_b,
        Ok(RoomInitialState {
            user_limit: 7,
            private: false,
        })
    );
    assert_eq!(
        resolve_initial_state(creator_a.default_limit, creator_a.always_private),
        state_a
    );

    // Numbering: each creator's taken set and start stay independent even
    // when the calls interleave.
    assert_eq!(next_room_number(&[5, 6], creator_a.first_number), 7);
    assert_eq!(next_room_number(&[10], creator_b.first_number), 11);
    assert_eq!(next_room_number(&[5, 6], creator_a.first_number), 7);
    assert_eq!(next_room_number(&[10], creator_b.first_number), 11);

    // Bitrate fallback: each creator's default is used only for its own rooms.
    assert_eq!(room_bitrate(&[], bitrate_a, tier_max), 48_000);
    assert_eq!(room_bitrate(&[], bitrate_b, tier_max), 32_000);
    assert_eq!(room_bitrate(&[], bitrate_a, tier_max), 48_000);

    // Placement: each creator's side anchors its own channel.
    let category = [creator(10, 0), other(30, 1), creator(20, 2)];
    assert_eq!(placement_for(&creator_a, 10, &[], &category), Ok(1));
    assert_eq!(placement_for(&creator_b, 20, &[], &category), Ok(2));
    assert_eq!(placement_for(&creator_a, 10, &[], &category), Ok(1));
}

fn two_creator_configuration() -> (VoiceConfiguration, GuildInventory) {
    let guild_id = "9001".to_owned();
    let config = VoiceConfiguration {
        version: 1,
        guild_id: guild_id.clone(),
        creators: vec![
            creator_config("101", 4, true, RoomPosition::Below, 5, false),
            creator_config("102", 7, false, RoomPosition::Above, 10, true),
        ],
        templates: vec![],
        aliases: vec![],
        lists: vec![],
        logging: None,
        settings: GuildSettings {
            creation_enabled: true,
            unique_names: false,
            no_game_label: "General".to_owned(),
            force_single_game: false,
            count_members_without_activity: false,
            time_zone: "UTC".to_owned(),
            text_channel_name: "voice-chat".to_owned(),
            text_viewer_role_id: None,
            command_role_id: None,
            command_roles: vec![],
        },
    };
    let channels = [("101", ChannelKind::Voice), ("102", ChannelKind::Voice)]
        .into_iter()
        .map(|(id, kind)| {
            (
                id.to_owned(),
                ChannelReference {
                    guild_id: guild_id.clone(),
                    kind,
                },
            )
        })
        .collect();
    let inventory = GuildInventory {
        guild_id,
        channels,
        roles: BTreeMap::new(),
        members: BTreeMap::new(),
    };
    (config, inventory)
}

#[test]
fn codec_round_trip_keeps_each_creator_defaults_separate() {
    // Persistence must not leak either: A's limit/privacy/position/start/
    // group survive export/import without touching B's, and vice versa.
    let (config, inventory) = two_creator_configuration();
    let wire = export_configuration(&config, &inventory).expect("valid fixture");
    let decoded = import_configuration(&wire, &inventory).expect("round trip");
    assert_eq!(decoded, config);
    assert_eq!(decoded.creators.len(), 2);
    assert_eq!(decoded.creators[0].default_limit, 4);
    assert!(decoded.creators[0].always_private);
    assert_eq!(decoded.creators[0].position, RoomPosition::Below);
    assert_eq!(decoded.creators[0].first_number, 5);
    assert!(!decoded.creators[0].group_by_category);
    assert_eq!(decoded.creators[1].default_limit, 7);
    assert!(!decoded.creators[1].always_private);
    assert_eq!(decoded.creators[1].position, RoomPosition::Above);
    assert_eq!(decoded.creators[1].first_number, 10);
    assert!(decoded.creators[1].group_by_category);
    // The decoded per-creator values still resolve independently.
    assert_eq!(
        resolve_initial_state(
            decoded.creators[0].default_limit,
            decoded.creators[0].always_private
        ),
        Ok(RoomInitialState {
            user_limit: 4,
            private: true,
        })
    );
    assert_eq!(
        resolve_initial_state(
            decoded.creators[1].default_limit,
            decoded.creators[1].always_private
        ),
        Ok(RoomInitialState {
            user_limit: 7,
            private: false,
        })
    );
    assert_eq!(
        next_room_number(&[5, 6], decoded.creators[0].first_number),
        7
    );
    assert_eq!(
        next_room_number(&[10], decoded.creators[1].first_number),
        11
    );
}
