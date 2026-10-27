//! Builder-owned: expose the live guild handle through the voice sink.
//!
//! The T-15 ticker's category preflight needs fresh gateway evidence
//! (bot roles + generation fence) via [`LiveCategoryReader`]. The
//! boot-composed `Arc<dyn VoiceEventSink>` is the only production owner of
//! that evidence, so the sink exposes it per guild. Missing actors fail
//! closed to `None`; the preflight treats that as unavailable, never as an
//! empty grant.

use super::*;

struct BareSink;

impl VoiceEventSink for BareSink {
    fn handle(&self, _event: &Event, _cache: &DefaultInMemoryCache) {}
    fn presence(&self, _update: &twilight_model::gateway::payload::incoming::PresenceUpdate) {}
    fn disconnect(&self) {}
    fn needs_bootstrap(&self, _cache: &DefaultInMemoryCache) -> bool {
        false
    }
}

#[test]
fn default_sink_exposes_no_live_guild() {
    let sink = BareSink;
    assert!(
        sink.event_voice_live(GUILD).is_none(),
        "a sink without live state must fail closed, never invent evidence"
    );
}

#[tokio::test]
async fn runtime_without_actor_exposes_no_live_guild() {
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        move || (Store::new(trace.clone()), Http::new(trace.clone())),
        Duration::from_millis(250),
        true,
    );
    assert!(
        runtime.event_voice_live(GUILD).is_none(),
        "no actor means no live evidence"
    );
}

#[tokio::test]
async fn runtime_exposes_live_guild_after_actor_spawn() {
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        move || (Store::new(trace.clone()), Http::new(trace.clone())),
        Duration::from_millis(250),
        true,
    );
    let actor = runtime.ensure_actor(GUILD).expect("actor spawns");
    let live = runtime
        .event_voice_live(GUILD)
        .expect("spawned actor exposes its live guild");
    // The exposed handle shares state with the actor: a generation fence
    // read through one is visible through the other.
    assert_eq!(
        live.event_voice_category_generation(GUILD),
        actor.live.event_voice_category_generation(GUILD),
        "exposed handle must share the actor's live state"
    );
    assert!(
        runtime.event_voice_live(GUILD + 1).is_none(),
        "other guilds without actors stay fail-closed"
    );
}

#[tokio::test]
async fn responder_forwards_live_guild_to_runtime() {
    let trace = Trace::default();
    let runtime = Arc::new(VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || (Store::new(trace.clone()), Http::new(trace.clone()))
        },
        Duration::from_millis(250),
        true,
    ));
    let replies = Arc::new(Replies::new(trace));
    let responder = VoiceResponder::new(Arc::clone(&runtime), replies);
    assert!(
        responder.event_voice_live(GUILD).is_none(),
        "no actor yet: responder forwards the runtime's fail-closed answer"
    );
    runtime.ensure_actor(GUILD).expect("actor spawns");
    assert!(
        responder.event_voice_live(GUILD).is_some(),
        "responder must forward the runtime's live guild"
    );
}
