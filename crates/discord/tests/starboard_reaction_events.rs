//! Starboard PR-1 acceptance: reaction + message-delete events model through
//! the adapter into [`CoreEvent`](two_bot_core::CoreEvent) (TOG-36121, paired
//! build TOG-36122).
//!
//! Test files ONLY: this target compiles green but the reaction/delete tests
//! FAIL until the builder lands the event modelling (currently: `ReactionAdd`/
//! `ReactionRemove` map to `CoreEvent::Unmodelled`, `MessageDelete` maps to
//! `None`). The build card implements:
//!
//! * adapter arms in `crates/discord/src/adapter.rs` mapping
//!   `MESSAGE_REACTION_ADD` / `MESSAGE_REACTION_REMOVE` / `MESSAGE_DELETE` to
//!   modelled `CoreEvent` variants (new variants, not `Unmodelled`);
//! * an adapter-minted delivery identity on the modelled reaction events,
//!   exposed as `CoreEvent::starboard_delivery_key` returning
//!   `Some((delivery_id, order))` with both parts non-empty (same pattern as
//!   `SelfRoleRequest { event_id, event_order }` in
//!   `crates/bot/src/self_role_runtime.rs`);
//! * `guild_id()` returning `Some(guild)` for the modelled events so the
//!   router can dispatch by guild.
//!
//! What the tests prove (TOG-36121 AC1 + AC4-adapter):
//!
//! 1. `ReactionAdd` / `ReactionRemove` / `MessageDelete` yield `Some` modelled
//!    (non-`Unmodelled`) core events instead of `Unmodelled` / `None`;
//! 2. the modelled events carry the guild routing id;
//! 3. modelled reaction events carry an adapter-minted `(delivery_id, order)`
//!    pair, so gateway replay/resume can dedupe before counting.
//!
//! Deliberately NOT pinned here (builder's interface freedom, see the handoff
//! doc): the new variant names and payload shapes, deterministic-vs-random
//! delivery-id derivation, DM (`guild_id: None`) reaction handling, and
//! router-dispatch observability (no starboard handler exists in PR-1, so
//! dispatch has no observable effect yet — the PR-2 runtime card pins it).
//!
//! Run: `cargo test -p two-bot-discord --locked --test starboard_reaction_events`

use twilight_model::{
    channel::message::EmojiReactionType,
    gateway::{
        event::Event,
        payload::incoming::{MemberRemove, MessageDelete, ReactionAdd, ReactionRemove},
        GatewayReaction,
    },
    id::Id,
    user::User,
};
use two_bot_core::CoreEvent;
use two_bot_discord::event_to_core;

const GUILD: u64 = 100_000_000_000_000_001;
const CHANNEL: u64 = 100_000_000_000_000_007;
const MESSAGE: u64 = 100_000_000_000_000_008;
const USER: u64 = 100_000_000_000_000_002;

fn reaction() -> GatewayReaction {
    GatewayReaction {
        burst: false,
        burst_colors: vec![],
        channel_id: Id::new(CHANNEL),
        emoji: EmojiReactionType::Unicode {
            name: "⭐".to_owned(),
        },
        guild_id: Some(Id::new(GUILD)),
        member: None,
        message_author_id: None,
        message_id: Id::new(MESSAGE),
        user_id: Id::new(USER),
    }
}

/// Test-local view of the adapter-minted delivery identity for starboard
/// events. The builder adds an inherent
/// `CoreEvent::starboard_delivery_key(&self) -> Option<(String, String)>`
/// with this exact shape; inherent methods shadow this trait at GREEN.
/// The qualified call in [`unmodelled_kinds_keep_no_delivery_key_fallback`]
/// keeps the trait live in both states (clippy `-D warnings` would otherwise
/// flag it as never-used after GREEN).
trait StarboardDeliveryKey {
    fn starboard_delivery_key(&self) -> Option<(String, String)> {
        None
    }
}

impl StarboardDeliveryKey for CoreEvent {}

fn assert_modelled(event: &Event, what: &str) -> CoreEvent {
    let core =
        event_to_core(event).unwrap_or_else(|| panic!("{what} must map to a core event, not None"));
    assert!(
        !matches!(core, CoreEvent::Unmodelled { .. }),
        "{what} must be modelled, got {core:?}"
    );
    assert_eq!(
        core.guild_id(),
        Some(GUILD),
        "{what} must carry the guild routing id"
    );
    core
}

#[test]
fn reaction_add_models_through_adapter_with_routing_id() {
    // AC1: MESSAGE_REACTION_ADD flows adapter -> CoreEvent instead of Unmodelled.
    let event = Event::ReactionAdd(Box::new(ReactionAdd(reaction())));
    assert_modelled(&event, "MESSAGE_REACTION_ADD");
}

#[test]
fn reaction_remove_models_through_adapter_with_routing_id() {
    // AC1: MESSAGE_REACTION_REMOVE flows adapter -> CoreEvent instead of Unmodelled.
    let event = Event::ReactionRemove(Box::new(ReactionRemove(reaction())));
    assert_modelled(&event, "MESSAGE_REACTION_REMOVE");
}

#[test]
fn message_delete_models_through_adapter_with_routing_id() {
    // AC1: MESSAGE_DELETE is currently dropped (None); the builder models it
    // so the PR-2 runtime can mark board posts source-deleted.
    let event = Event::MessageDelete(MessageDelete {
        channel_id: Id::new(CHANNEL),
        guild_id: Some(Id::new(GUILD)),
        id: Id::new(MESSAGE),
    });
    assert_modelled(&event, "MESSAGE_DELETE");
}

#[test]
fn reaction_events_carry_adapter_minted_delivery_key() {
    // AC4 (adapter leg): the modelled reaction events expose the
    // (delivery id, order) pair the router/handler dedupes on, so gateway
    // replay/resume never double-counts.
    for (event, what) in [
        (
            Event::ReactionAdd(Box::new(ReactionAdd(reaction()))),
            "MESSAGE_REACTION_ADD",
        ),
        (
            Event::ReactionRemove(Box::new(ReactionRemove(reaction()))),
            "MESSAGE_REACTION_REMOVE",
        ),
    ] {
        let core = assert_modelled(&event, what);
        let (delivery_id, order) = core.starboard_delivery_key().unwrap_or_else(|| {
            panic!("{what} must carry an adapter-minted (delivery id, order) pair")
        });
        assert!(
            !delivery_id.is_empty() && !order.is_empty(),
            "{what} delivery key parts must both be non-empty"
        );
    }
}

#[test]
fn unmodelled_kinds_keep_no_delivery_key_fallback() {
    // Control (green RED and GREEN): kinds the adapter has not promoted keep
    // no delivery key through the fallback, and the qualified call keeps the
    // test-local trait live after the builder's inherent method lands.
    let untouched = CoreEvent::Unmodelled {
        kind: "interaction_create".to_owned(),
    };
    assert_eq!(
        <CoreEvent as StarboardDeliveryKey>::starboard_delivery_key(&untouched),
        None
    );
}

fn user(id: u64) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot: false,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "test".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

#[test]
fn member_spine_still_maps_through_adapter() {
    // Control (green RED and GREEN): the S1 member spine is unchanged by the
    // reaction slice, and this target's harness drives the real adapter.
    let event = Event::MemberRemove(MemberRemove {
        guild_id: Id::new(10),
        user: user(20),
    });
    assert_eq!(
        event_to_core(&event),
        Some(CoreEvent::MemberLeft {
            guild_id: 10,
            user_id: 20,
        })
    );
}
