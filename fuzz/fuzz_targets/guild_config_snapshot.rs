#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use two_bot_core::backup::{
    guild_config::{
        canonical_snapshot, config_hash, seal_snapshot, snapshot_counts, verify_snapshot_integrity,
        SealState,
    },
    guild_config_restore::plan_restore,
};

fuzz_target!(|data: &[u8]| {
    let Ok(Value::Object(snapshot)) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    // Mirror the restore CLI's object/version gate; no apply API is called.
    if snapshot.get("version").and_then(Value::as_u64) != Some(1) {
        return;
    }
    let _ = verify_snapshot_integrity(&snapshot);
    let canonical = canonical_snapshot(&snapshot);
    let _ = config_hash(&canonical);
    let _ = snapshot_counts(&snapshot);
    let sealed = seal_snapshot(snapshot.clone());
    assert_eq!(verify_snapshot_integrity(&sealed).unwrap(), SealState::Sealed);

    // A fixed in-memory current guild drives references/diff decoding. Match
    // only the identity, not the uploaded roles/channels or its integrity seal.
    let mut current = serde_json::json!({
        "version": 1, "guildId": "guild", "guild": {},
        "roles": [], "channels": [], "emojis": []
    })
    .as_object()
    .unwrap()
    .clone();
    if let Some(guild_id) = snapshot.get("guildId") {
        current.insert("guildId".to_owned(), guild_id.clone());
    }
    let _ = plan_restore(&snapshot, &current);
});
