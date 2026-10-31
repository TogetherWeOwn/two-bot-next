//! Discord-native funnel acceptance: game-night auto-create plan and invite rotation.
//!
//! FAILING acceptance tests for the funnel build. The contract below names the
//! items the build adds; none of them exist yet, so this target stays red
//! until the build lands. The build must not edit this file: turn it green by
//! implementing the contract in library code only.
//!
//! Builder contract, all in library code:
//!
//! * `two_bot_core::internal_actions::GameNight`, a plain input struct with
//!   `name: String`, `starts_at: String`, `ends_at: String`, and
//!   `location: String` fields (timestamps in UTC-millis ISO-8601, the same
//!   rendering `EventInput` carries).
//! * `two_bot_core::internal_actions::MAX_ACTIVE_SCHEDULED_EVENTS: usize`,
//!   the Discord cap of 100 active scheduled events per guild.
//! * `two_bot_core::internal_actions::plan_game_night_creates(
//!     nights: &[GameNight], active_count: usize) -> Vec<EventInput>`:
//!   one EXTERNAL `EventInput` per game night (`EventPlace::Location` with
//!   the night's location, name and times carried over, no description),
//!   truncated from the tail so that
//!   `active_count + returned.len() <= MAX_ACTIVE_SCHEDULED_EVENTS`.
//! * `InviteTracker::rotate_code(
//!     &self, guild_id: Snowflake, old_code: &str, new_code: &str
//!   ) -> Option<InviteState>`: drop the `old_code` snapshot row and store
//!   the `new_code` row carrying the old row's inviter and channel with
//!   `uses` reset to zero (a rotated permanent invite is fresh in Discord
//!   but keeps its funnel attribution); `None` when `old_code` is unknown,
//!   storing nothing in that case.
//!
//! Offline only: pure domain logic, no Discord, network, or database.

use two_bot_core::internal_actions::{
    plan_game_night_creates, EventPlace, GameNight, MAX_ACTIVE_SCHEDULED_EVENTS,
};
use two_bot_core::invites::{InviteState, InviteTracker, MemSnapshots};
use two_bot_core::Snowflake;

const GUILD: Snowflake = 100000000000000001;
const INVITER: Snowflake = 100000000000000007;
const CHANNEL: Snowflake = 100000000000000003;

fn night(name: &str, location: &str) -> GameNight {
    GameNight {
        name: name.to_owned(),
        starts_at: "2026-09-01T19:00:00.000Z".to_owned(),
        ends_at: "2026-09-01T22:00:00.000Z".to_owned(),
        location: location.to_owned(),
    }
}

#[test]
fn plan_creates_one_external_input_per_game_night() {
    let nights = [
        night("Game Night", "The community hall"),
        night("Board Games", "The side room"),
    ];
    let planned = plan_game_night_creates(&nights, 0);
    assert_eq!(planned.len(), 2);
    assert_eq!(planned[0].name, "Game Night");
    assert_eq!(planned[0].starts_at, "2026-09-01T19:00:00.000Z");
    assert_eq!(planned[0].ends_at, "2026-09-01T22:00:00.000Z");
    assert_eq!(
        planned[0].place,
        EventPlace::Location("The community hall".to_owned())
    );
    assert_eq!(
        planned[1].place,
        EventPlace::Location("The side room".to_owned())
    );
}

#[test]
fn plan_stops_at_the_active_event_cap() {
    assert_eq!(MAX_ACTIVE_SCHEDULED_EVENTS, 100);
    let nights = [
        night("Game Night", "The community hall"),
        night("Board Games", "The side room"),
    ];
    assert_eq!(plan_game_night_creates(&nights, 99).len(), 1);
    assert!(plan_game_night_creates(&nights, 100).is_empty());
    assert_eq!(plan_game_night_creates(&nights, 0).len(), 2);
    assert!(plan_game_night_creates(&[], 100).is_empty());
}

#[test]
fn rotation_replaces_the_code_without_dropping_the_funnel_row() {
    let tracker = InviteTracker::new(MemSnapshots::default());
    tracker.seed(
        GUILD,
        InviteState {
            code: "old-code".to_owned(),
            uses: 5,
            inviter_id: Some(INVITER),
            channel_id: Some(CHANNEL),
            max_uses: 0,
            temporary: false,
        },
    );
    let rotated = tracker.rotate_code(GUILD, "old-code", "new-code");
    assert_eq!(
        rotated,
        Some(InviteState {
            code: "new-code".to_owned(),
            uses: 0,
            inviter_id: Some(INVITER),
            channel_id: Some(CHANNEL),
            max_uses: 0,
            temporary: false,
        })
    );
    assert_eq!(tracker.inviter_for(GUILD, "old-code"), None);
    assert_eq!(tracker.inviter_for(GUILD, "new-code"), Some(INVITER));
    assert_eq!(
        tracker.rotate_code(GUILD, "missing-code", "other-code"),
        None
    );
}
