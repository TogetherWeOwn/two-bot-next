//! Spawn pin: the boot admission config must reach the durable claim.
//!
//! The acceptance battery pins the loader and the runtime field, but the
//! actor-spawn hop was covered only by a source grep: a spawn installing
//! the `(1, 40, 0)` default instead of the boot config kept the suite
//! green. This test drives a runtime carrying a non-default config through
//! spawn and one creator join, and asserts the store claim saw the
//! configured knobs.

use super::*;

fn boot_config() -> CreateAdmissionConfig {
    let values: HashMap<String, String> = [
        ("TWO_TEMP_VOICE_MAX_PER_USER", "3"),
        ("TWO_TEMP_VOICE_MAX_PER_GUILD", "25"),
        ("TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS", "45"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    configured_admission(|key| values.get(key).cloned())
}

#[tokio::test]
async fn spawn_applies_the_boot_admission_config_to_claims() {
    let trace = Trace::default();
    let seen: Arc<Mutex<Vec<CreateAdmissionConfig>>> = Arc::new(Mutex::new(Vec::new()));
    let make_seen = seen.clone();
    let make_trace = trace.clone();
    let runtime = VoiceRuntime::new(
        move || {
            let mut store = Store::new(make_trace.clone());
            store.claim_configs = make_seen.clone();
            (store, Http::new(make_trace.clone()))
        },
        Duration::from_millis(10),
        true,
    )
    .with_admission_config(boot_config());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    assert!(runtime.voice_frame(GUILD, MEMBER, Some(CREATOR), Some(false), "x".to_owned()));
    wait_trace(&trace, "persist:500").await;
    assert_eq!(*seen.lock().unwrap(), vec![boot_config()]);
}
