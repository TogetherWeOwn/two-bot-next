#![cfg(test)]
//! Responder-level pins for the stored per-guild invite-code override.
//!
//! The [`invite_override`](super::invite_override) battery pins the pure
//! `stored_invite_code_with` seam and `invite_render` directly. These tests
//! pin the production wiring in `VoiceResponder::handle_received`: a
//! `VoiceRuntime` built with `with_live_source` serves a live snapshot
//! holding `TWO_GUILD_INVITE_CODE` rows, an `InteractionCreate` is fed
//! through the responder sink (which spawns `respond_named`), and the
//! completed reply must render the stored code — not the gateway vanity
//! code from the cache. The invalid-row twin, the other-guild isolation
//! case, and the non-invite warning-scope pins use the same boundary.
//!
//! Mutation contract: the stored-winner test must fail when the wiring is
//! reverted to `vanity_code_from_cache` only, when `None` is passed instead
//! of `snapshot.as_ref()`, and when the lookup guild is wrong. The
//! warning-scope tests are RED until the resolver runs only for
//! `VoiceCommand::Invite`.

use super::*;
use serde_json::json;
use twilight_model::gateway::payload::incoming::{GuildCreate, InteractionCreate};
use two_bot_core::settings::{live_channel, LiveSettings, SettingRow, SettingsSnapshot};
use two_bot_core::voice_utilities::INVALID_INVITE_CODE;

/// The stored per-guild invite-code key. Store-only.
const KEY: &str = "TWO_GUILD_INVITE_CODE";

/// The gateway vanity code carried by the cache in every test.
const VANITY: &str = "vanitycode";

/// A valid stored code, distinct from the vanity code.
const STORED: &str = "storedcode9";

/// A valid stored code belonging to another guild.
const OTHER_CODE: &str = "otherguild1";

/// Another guild's ID, distinct from the fixture guild.
const OTHER_GUILD: &str = "200";

/// A distinctive invalid stored value. It must never appear in a reply.
const REJECTED_CANARY: &str = "canary/rejected.value-zz9";

fn guild() -> String {
    GUILD.to_string()
}

fn setting_row(guild_id: &str, value: serde_json::Value) -> SettingRow {
    SettingRow {
        guild_id: guild_id.to_owned(),
        key: KEY.to_owned(),
        value,
        version: 1,
    }
}

/// A published live snapshot holding exactly `rows`.
fn live_with(rows: Vec<SettingRow>) -> LiveSettings {
    let (mut writer, reader) = live_channel();
    writer.publish(&SettingsSnapshot { revision: 1, rows });
    reader
}

/// A gateway cache whose fixture guild carries `vanity` as its vanity URL
/// code, so `vanity_code_from_cache` resolves it for the test interaction.
fn cache_with_vanity(vanity: &str) -> DefaultInMemoryCache {
    let cache = DefaultInMemoryCache::new();
    let mut guild_json: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json"))
            .expect("guild fixture parses");
    guild_json["vanity_url_code"] = json!(vanity);
    let guild: twilight_model::guild::Guild =
        serde_json::from_value(guild_json).expect("guild with vanity parses");
    cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(guild))));
    cache
}

/// An enabled runtime whose live-settings source serves `live`.
fn runtime_with_live(trace: Trace, live: LiveSettings) -> VoiceRuntime<Store, Http> {
    VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || (Store::new(trace.clone()), Http::new(trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
    .with_live_source(move || Some(live.clone()))
}

/// Wait for the spawned responder to complete the interaction.
async fn await_completed(replies: &Replies) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while replies.completed.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the responder must complete the interaction");
}

#[tokio::test]
async fn responder_renders_the_stored_invite_code_not_the_cached_vanity_code() {
    let trace = Trace::default();
    let live = live_with(vec![setting_row(&guild(), json!(STORED))]);
    let runtime = Arc::new(runtime_with_live(trace.clone(), live));
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let replies = Arc::new(Replies::new(trace.clone()));
    let sink = VoiceResponder::new(Arc::clone(&runtime), Arc::clone(&replies));
    let cache = cache_with_vanity(VANITY);
    let interaction = voice_interaction(Some(command_data("invite", Vec::new())), None, true);
    sink.handle(
        &Event::InteractionCreate(Box::new(InteractionCreate(interaction))),
        &cache,
    );
    await_completed(&replies).await;
    assert_eq!(*trace.lock().unwrap(), ["defer", "complete"]);
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1, "invite must complete exactly once");
    let text = response_text(&completed[0]);
    assert_eq!(
        text,
        format!("Join the server: https://discord.gg/{STORED}"),
        "the stored row must win over the cached vanity code"
    );
    assert!(
        !text.contains(VANITY),
        "the vanity code must not appear once a stored code wins: {text:?}"
    );
}

#[tokio::test]
async fn responder_with_an_invalid_stored_row_renders_the_vanity_fallback() {
    let trace = Trace::default();
    let live = live_with(vec![setting_row(&guild(), json!(REJECTED_CANARY))]);
    let runtime = Arc::new(runtime_with_live(trace.clone(), live));
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let replies = Arc::new(Replies::new(trace.clone()));
    let sink = VoiceResponder::new(Arc::clone(&runtime), Arc::clone(&replies));
    let cache = cache_with_vanity(VANITY);
    let interaction = voice_interaction(Some(command_data("invite", Vec::new())), None, true);
    sink.handle(
        &Event::InteractionCreate(Box::new(InteractionCreate(interaction))),
        &cache,
    );
    await_completed(&replies).await;
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1, "invite must complete exactly once");
    let text = response_text(&completed[0]);
    assert_eq!(
        text,
        format!("Join the server: https://discord.gg/{VANITY}"),
        "an invalid stored row must fail closed to the vanity fallback"
    );
    assert_ne!(
        text, INVALID_INVITE_CODE,
        "an invalid stored row must not mask a usable vanity code"
    );
    assert!(
        !text.contains(REJECTED_CANARY),
        "the rejected value must never reach the reply: {text:?}"
    );
}

#[tokio::test]
async fn responder_ignores_a_stored_row_from_another_guild() {
    let trace = Trace::default();
    let live = live_with(vec![setting_row(OTHER_GUILD, json!(OTHER_CODE))]);
    let runtime = Arc::new(runtime_with_live(trace.clone(), live));
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let replies = Arc::new(Replies::new(trace.clone()));
    let sink = VoiceResponder::new(Arc::clone(&runtime), Arc::clone(&replies));
    let cache = cache_with_vanity(VANITY);
    let interaction = voice_interaction(Some(command_data("invite", Vec::new())), None, true);
    sink.handle(
        &Event::InteractionCreate(Box::new(InteractionCreate(interaction))),
        &cache,
    );
    await_completed(&replies).await;
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1, "invite must complete exactly once");
    let text = response_text(&completed[0]);
    assert_eq!(
        text,
        format!("Join the server: https://discord.gg/{VANITY}"),
        "another guild's row must not drive this guild's reply"
    );
    assert!(
        !text.contains(OTHER_CODE),
        "another guild's code must never reach the reply: {text:?}"
    );
}

#[tokio::test]
async fn invalid_row_does_not_warn_on_a_non_invite_slash_command() {
    super::tracing_capture::global();
    let trace = Trace::default();
    let live = live_with(vec![setting_row(&guild(), json!(REJECTED_CANARY))]);
    let runtime = Arc::new(runtime_with_live(trace.clone(), live));
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let replies = Arc::new(Replies::new(trace.clone()));
    let sink = VoiceResponder::new(Arc::clone(&runtime), Arc::clone(&replies));
    let cache = cache_with_vanity(VANITY);
    let interaction = voice_interaction(Some(command_data("ping", Vec::new())), None, true);
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Ping),
        "ping must be a recognized non-invite command"
    );
    let capture = super::tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        sink.handle(
            &Event::InteractionCreate(Box::new(InteractionCreate(interaction))),
            &cache,
        );
    });
    await_completed(&replies).await;
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1, "ping must complete");
    let text = response_text(&completed[0]);
    assert!(
        text.starts_with("Pong! "),
        "ping must be answered, got: {text:?}"
    );
    let logged = capture.text();
    assert!(
        !logged
            .lines()
            .any(|line| line.contains(KEY) && line.contains("invalid_invite_code")),
        "a non-invite command must not emit the invite warning: {logged}"
    );
}

#[tokio::test]
async fn invalid_row_does_not_warn_on_a_recognized_component() {
    super::tracing_capture::global();
    let trace = Trace::default();
    let live = live_with(vec![setting_row(&guild(), json!(REJECTED_CANARY))]);
    let runtime = Arc::new(runtime_with_live(trace.clone(), live));
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let replies = Arc::new(Replies::new(trace.clone()));
    let sink = VoiceResponder::new(Arc::clone(&runtime), Arc::clone(&replies));
    let cache = cache_with_vanity(VANITY);
    let interaction = component_interaction("votekick:7000:yes", None, MEMBER);
    assert!(
        matches!(
            parse_voice_command(&interaction),
            Some(VoiceCommand::Ballot { .. })
        ),
        "the ballot button must be a recognized non-invite component"
    );
    let capture = super::tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        sink.handle(
            &Event::InteractionCreate(Box::new(InteractionCreate(interaction))),
            &cache,
        );
    });
    let logged = capture.text();
    assert!(
        !logged
            .lines()
            .any(|line| line.contains(KEY) && line.contains("invalid_invite_code")),
        "a non-invite component must not emit the invite warning: {logged}"
    );
}
