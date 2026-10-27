//! Builder-owned: the boot context passes its live handle into the whole-tick gate.
use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use std::sync::atomic::{AtomicUsize, Ordering};
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::Event;
use two_bot::voice_rooms::{LiveGuild, VoiceEventSink};

struct Sink(LiveGuild);
impl VoiceEventSink for Sink {
    fn handle(&self, _: &Event, _: &DefaultInMemoryCache) {}
    fn presence(&self, _: &twilight_model::gateway::payload::incoming::PresenceUpdate) {}
    fn disconnect(&self) {}
    fn needs_bootstrap(&self, _: &DefaultInMemoryCache) -> bool {
        false
    }
    fn event_voice_live(&self, guild: u64) -> Option<LiveGuild> {
        (guild == 100).then(|| self.0.clone())
    }
}

#[tokio::test]
async fn context_passes_boot_live_handle_and_holds_gate_before_database_work() {
    let document = serde_json::json!({"id":"200", "guild_id":"100", "type":4,
        "name":"events", "permission_overwrites":[]});
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, document)).await;
    crate::gateway::ensure_crypto_provider();
    let context = Context {
        url: "invalid fixture database; must never connect".into(),
        pool: OnceCell::new(),
        rest: ActionExecutor::with_proxy("synthetic-context-token".into(), Some(mock.origin()))
            .unwrap(),
        guild: "100".into(),
        observation: Mutex::new(()),
        events_gate: JobGate::default(),
        event_voice: event_voice_preflight::Tick::default(),
        event_voice_source: event_voice_source::Reader::default(),
        event_voice_reconcile: event_voice_reconcile::Reconciler::default(),
        voice: Some(Arc::new(Sink(super::event_voice_preflight_tests::live()))),
    };
    let (_stop, shutdown) = watch::channel(false);
    let calls = AtomicUsize::new(0);
    context
        .with_event_voice_tick(
            event_voice_preflight::Settings {
                category: Some(200),
                staff_channel: None,
            },
            &shutdown,
            |category| {
                assert_eq!(category, 200);
                async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert!(context.event_voice.gate.try_acquire().is_none());
                    assert!(
                        context.pool.get().is_none(),
                        "the continuation owns lazy DB access"
                    );
                    Ok(())
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "ready live evidence must reach the continuation"
    );
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}
