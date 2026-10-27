#![cfg(test)]
//! Builder-owned: the website jobs hold the boot-composed voice handle.
//!
//! The T-15 ticker's category preflight and worker-guarded deletes both need
//! the gateway's live evidence. `main` clones the voice sink before moving it
//! into the gateway task and hands the clone to `serve`, which stores it on
//! the shared [`Context`]. A missing handle fails closed to `None`; the tick
//! treats that as unavailable, never as an empty grant.

use super::*;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::Event;
use two_bot::voice_rooms::{LiveGuild, VoiceEventSink};

use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD_TEXT: &str = "2222";
const GUILD_ID: u64 = 2222;

struct LiveSink {
    live: LiveGuild,
}

impl VoiceEventSink for LiveSink {
    fn handle(&self, _event: &Event, _cache: &DefaultInMemoryCache) {}
    fn presence(&self, _update: &twilight_model::gateway::payload::incoming::PresenceUpdate) {}
    fn disconnect(&self) {}
    fn needs_bootstrap(&self, _cache: &DefaultInMemoryCache) -> bool {
        false
    }
    fn event_voice_live(&self, guild: two_bot_core::Snowflake) -> Option<LiveGuild> {
        (guild == GUILD_ID).then(|| self.live.clone())
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-plumbing-test-token".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

fn plumbing_context(mock: &MockRest, voice: Option<Arc<dyn VoiceEventSink>>) -> Context {
    Context {
        url: "postgres://fixture:fixture-password@127.0.0.1:1/fixture?sslmode=disable".to_owned(),
        pool: OnceCell::new(),
        rest: executor(mock),
        guild: GUILD_TEXT.to_owned(),
        observation: Mutex::new(()),
        events_gate: JobGate::default(),
        event_voice: event_voice_preflight::Tick::default(),
        event_voice_source: event_voice_source::Reader::default(),
        event_voice_reconcile: event_voice_reconcile::Reconciler::default(),
        voice,
    }
}

#[tokio::test]
async fn context_exposes_voice_live_guild() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, serde_json::json!([]))).await;
    let voice: Arc<dyn VoiceEventSink> = Arc::new(LiveSink {
        live: LiveGuild::new(GUILD_ID),
    });
    let context = plumbing_context(&mock, Some(voice));
    assert!(
        context.event_voice_live_guild(GUILD_ID).is_some(),
        "context must expose the voice sink's live guild"
    );
    assert!(
        context.event_voice_live_guild(GUILD_ID + 1).is_none(),
        "context must forward the sink's fail-closed answer"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn context_without_voice_fails_closed() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, serde_json::json!([]))).await;
    let context = plumbing_context(&mock, None);
    assert!(
        context.event_voice_live_guild(GUILD_ID).is_none(),
        "no voice handle means no live evidence, never an empty grant"
    );
    mock.shutdown().await;
}
