//! Temp-channel create + idle-sweep delete happy-path pins (offline).
//!
//! One file pinning the whole temporary voice-channel happy path through the
//! public core API only: admission allows a fresh create, a creator join
//! yields exactly one room spec, number allocation plus the default template
//! produce the channel name, caps refuse at (not before) their boundaries,
//! and the idle sweep deletes only empty manageable tracked rooms. No
//! database, Redis, Discord, staging guild, gateway session or production
//! identity is used.

use std::collections::HashMap;

use two_bot_core::voice_create_admission::{
    decide_admission, AdmissionConfigError, AdmissionDecision, AdmissionRequest,
    CreateAdmissionConfig, RefusalReason, DEFAULT_CREATE_COOLDOWN_SECS, DEFAULT_MAX_PER_GUILD,
    DEFAULT_MAX_PER_USER, MAX_CREATE_COOLDOWN_SECS, MAX_ROOMS_PER_GUILD, MAX_ROOMS_PER_USER,
};
use two_bot_core::voice_ghost_cleanup::plan_ghost_cleanup;
use two_bot_core::voice_naming::{
    allocate_room_number, resolve_room_name, ChannelKind, RoomContext, DEFAULT_NAME_TEMPLATE,
    MAX_NAME_LEN,
};
use two_bot_core::voice_rooms::{
    decide_room_join, decide_room_leave, reconcile, CreatorChannel, RoomJoinDecision,
    RoomJoinRequest, RoomLeaveDecision, RoomLeaveReport, SeenChannel, VoiceRoom,
    MAX_CHANNELS_PER_CATEGORY,
};

const GUILD: u64 = 7;
const CREATOR: u64 = 21;
const CATEGORY: u64 = 33;
const OWNER: u64 = 101;
const STAMP: &str = "2026-10-04T00:00:00.000Z";

fn creator() -> CreatorChannel {
    CreatorChannel::new(GUILD, CREATOR)
}

fn join_req(category_channel_count: usize) -> RoomJoinRequest {
    RoomJoinRequest {
        guild_id: GUILD,
        member_id: OWNER,
        channel_id: CREATOR,
        creator: Some(creator()),
        category_id: Some(CATEGORY),
        category_channel_count,
        seed: 42,
        now: STAMP.to_owned(),
    }
}

fn tracked(channel: u64) -> VoiceRoom {
    VoiceRoom {
        guild_id: GUILD,
        channel_id: channel,
        creator_channel_id: CREATOR,
        owner_id: OWNER,
        original_creator_id: OWNER,
        name_seed: 42,
        created_at: STAMP.to_owned(),
    }
}

fn ctx() -> RoomContext {
    RoomContext {
        channel_kind: ChannelKind::Temporary,
        room_number: 3,
        owner_name: "Ava".to_owned(),
        original_creator_name: "Ava".to_owned(),
        member_count: 1,
        owner_present: true,
        live_count: 0,
        user_limit: 0,
        game_name: String::new(),
        stream_title: String::new(),
        members_playing: 0,
        parties: Vec::new(),
        timestamp: 1_790_683_200,
        room_minutes: 0,
        game_minutes: 0,
        tz_offset_minutes: 0,
        seed: 42,
        named_lists: HashMap::new(),
        fallback_name: "Hangout".to_owned(),
    }
}

fn admission_request(owned_by_user: u32, rooms_in_guild: u32) -> AdmissionRequest<'static> {
    AdmissionRequest {
        user_id: OWNER,
        owned_by_user,
        rooms_in_guild,
        last_created_at_secs: None,
        accepted_reservations: &[],
    }
}

// --- create happy path ------------------------------------------------------

#[test]
fn defaults_match_legacy_and_allow_a_fresh_first_create() {
    assert_eq!(DEFAULT_MAX_PER_USER, 1);
    assert_eq!(DEFAULT_MAX_PER_GUILD, 40);
    assert_eq!(DEFAULT_CREATE_COOLDOWN_SECS, 30);
    let config = CreateAdmissionConfig::default();
    assert_eq!(
        decide_admission(&config, &admission_request(0, 0), 1_700_000_000),
        Ok(AdmissionDecision::Allow)
    );
}

#[test]
fn creator_join_yields_exactly_one_room_spec_for_the_joiner() {
    let decision = decide_room_join(join_req(3));
    let RoomJoinDecision::CreateRoom { spec } = decision else {
        panic!("creator join must create a room");
    };
    assert_eq!(spec.guild_id, GUILD);
    assert_eq!(spec.creator_channel_id, CREATOR);
    assert_eq!(spec.owner_id, OWNER);
    assert_eq!(spec.seed, 42);
    // Completing the spec tracks the joiner as owner and original creator.
    let room = VoiceRoom::from_spec(spec, 500);
    assert_eq!(room.channel_id, 500);
    assert_eq!(room.owner_id, room.original_creator_id);
}

#[test]
fn created_room_gets_allocated_number_and_templated_name() {
    let number = allocate_room_number(&[1, 2], 1).expect("room 3 is free");
    assert_eq!(number, 3);
    let name = resolve_room_name("@@owner@@ ##", &ctx(), "Hangout");
    assert_eq!(name, "Ava #3");
    // The spec default template names the owner's room and stays bounded.
    let name = resolve_room_name(DEFAULT_NAME_TEMPLATE, &ctx(), "Hangout");
    assert!(name.contains("Ava's "), "unexpected {name:?}");
    assert!(!name.is_empty());
    assert!(name.chars().count() <= MAX_NAME_LEN);
    assert_eq!(MAX_NAME_LEN, 100);
    // Same seed and state re-renders the same name: renames never re-roll.
    assert_eq!(
        name,
        resolve_room_name(DEFAULT_NAME_TEMPLATE, &ctx(), &name)
    );
}

// --- caps: refuse at the boundary, never before --------------------------------

#[test]
fn config_bounds_match_legacy_ranges() {
    assert!(CreateAdmissionConfig::new(1, 1, 0).is_ok());
    assert!(CreateAdmissionConfig::new(
        MAX_ROOMS_PER_USER,
        MAX_ROOMS_PER_GUILD,
        MAX_CREATE_COOLDOWN_SECS
    )
    .is_ok());
    assert_eq!(MAX_ROOMS_PER_USER, 10);
    assert_eq!(MAX_ROOMS_PER_GUILD, 45);
    assert_eq!(MAX_CREATE_COOLDOWN_SECS, 3600);
    assert_eq!(
        CreateAdmissionConfig::new(11, 40, 30),
        Err(AdmissionConfigError::MaxPerUser)
    );
    assert_eq!(
        CreateAdmissionConfig::new(1, 46, 30),
        Err(AdmissionConfigError::MaxPerGuild)
    );
    assert_eq!(
        CreateAdmissionConfig::new(1, 40, 3601),
        Err(AdmissionConfigError::Cooldown)
    );
}

#[test]
fn caps_allow_below_and_refuse_at_the_limit() {
    let config = CreateAdmissionConfig::default();
    // User cap (default 1): zero rooms allows, one room refuses.
    assert_eq!(
        decide_admission(&config, &admission_request(0, 0), 1_700_000_000),
        Ok(AdmissionDecision::Allow)
    );
    assert_eq!(
        decide_admission(&config, &admission_request(1, 1), 1_700_000_000),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserCap
        })
    );
    // Guild cap (default 40): 39 allows, 40 refuses.
    let config = CreateAdmissionConfig::new(10, 40, 0).expect("valid config");
    assert_eq!(
        decide_admission(&config, &admission_request(0, 39), 1_700_000_000),
        Ok(AdmissionDecision::Allow)
    );
    assert_eq!(
        decide_admission(&config, &admission_request(0, 40), 1_700_000_000),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::GuildCap
        })
    );
}

#[test]
fn full_category_refuses_with_second_creator_hint() {
    assert_eq!(MAX_CHANNELS_PER_CATEGORY, 50);
    // One slot left: the join still creates a room.
    assert!(matches!(
        decide_room_join(join_req(MAX_CHANNELS_PER_CATEGORY - 1)),
        RoomJoinDecision::CreateRoom { .. }
    ));
    // At the Discord ceiling: refuse and point at another category.
    let decision = decide_room_join(join_req(MAX_CHANNELS_PER_CATEGORY));
    let RoomJoinDecision::RefuseCategoryFull {
        category_id,
        message,
    } = decision
    else {
        panic!("full category must refuse");
    };
    assert_eq!(category_id, CATEGORY);
    assert!(
        message.contains("second creator channel in another category"),
        "unexpected {message:?}"
    );
}

// --- idle-sweep delete happy path ----------------------------------------------

#[test]
fn sweep_threshold_is_strictly_zero_remaining_humans() {
    // Untracked channels are never touched.
    assert_eq!(
        decide_room_leave(RoomLeaveReport {
            room: None,
            remaining_humans: 0,
        }),
        RoomLeaveDecision::Ignore
    );
    // One human left: the room stays.
    assert_eq!(
        decide_room_leave(RoomLeaveReport {
            room: Some(tracked(500)),
            remaining_humans: 1,
        }),
        RoomLeaveDecision::Ignore
    );
    // Last human out: the sweep deletes within seconds.
    assert_eq!(
        decide_room_leave(RoomLeaveReport {
            room: Some(tracked(500)),
            remaining_humans: 0,
        }),
        RoomLeaveDecision::DeleteRoom { room: tracked(500) }
    );
}

#[test]
fn sweep_deletes_only_empty_manageable_tracked_rooms() {
    let gone = tracked(501);
    let empty = tracked(502);
    let locked = tracked(503);
    let lived_in = tracked(504);
    let plan = reconcile(
        &[
            gone.clone(),
            empty.clone(),
            locked.clone(),
            lived_in.clone(),
        ],
        &[
            SeenChannel {
                channel_id: 502,
                human_occupants: 0,
                manageable: true,
            },
            SeenChannel {
                channel_id: 503,
                human_occupants: 2,
                manageable: false,
            },
            SeenChannel {
                channel_id: 504,
                human_occupants: 2,
                manageable: true,
            },
            // Never tracked: must never appear in the plan.
            SeenChannel {
                channel_id: 999,
                human_occupants: 0,
                manageable: true,
            },
        ],
    );
    assert_eq!(plan.forget, vec![gone]);
    assert_eq!(plan.delete_empty, vec![empty]);
    assert_eq!(plan.suspend, vec![locked]);
    let mentioned: Vec<u64> = plan
        .forget
        .iter()
        .chain(plan.delete_empty.iter())
        .chain(plan.suspend.iter())
        .map(|room| room.channel_id)
        .collect();
    assert!(
        !mentioned.contains(&999),
        "untracked channel must be untouched"
    );
    assert!(!mentioned.contains(&504), "occupied room must be untouched");
}

#[test]
fn ghost_cleanup_routes_sweep_delete_and_lists_untracked_for_triage() {
    let plan = plan_ghost_cleanup(
        &[tracked(501), tracked(502), tracked(503), tracked(504)],
        &[
            SeenChannel {
                channel_id: 502,
                human_occupants: 0,
                manageable: true,
            },
            SeenChannel {
                channel_id: 503,
                human_occupants: 1,
                manageable: true,
            },
            SeenChannel {
                channel_id: 504,
                human_occupants: 0,
                manageable: false,
            },
            SeenChannel {
                channel_id: 601,
                human_occupants: 0,
                manageable: true,
            },
        ],
    );
    // 501 tracked but gone: forget the row, no Discord write.
    assert_eq!(plan.forget, vec![tracked(501)]);
    // 502 empty and manageable: the sweep's happy-path delete.
    assert_eq!(plan.delete, vec![tracked(502)]);
    // 503 occupied, 504 access lost: listed, never deleted.
    assert_eq!(plan.occupied, vec![tracked(503)]);
    assert_eq!(plan.suspended, vec![tracked(504)]);
    // 601 untracked: listed for manual triage even though empty, never deleted.
    assert_eq!(plan.untracked_present, vec![601]);
    assert_eq!(plan.action_count(), 2);
}
