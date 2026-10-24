use serde_json::json;
use std::{collections::HashMap, time::Instant};
use two_bot_core::settings::{live_channel, OpsRecord, SettingRow, SettingsSnapshot};

#[test]
fn independently_expiring_ops_cannot_replace_a_drain_guarded_catalogue() {
    let key = "TWO_SELF_ROLE_PANELS";
    let (mut writer, live) = live_channel();
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![SettingRow {
            guild_id: "100".into(),
            key: key.into(),
            value: json!([]),
            version: 1,
        }],
    });
    writer.apply_ops(
        Some(&OpsRecord {
            epoch: 1,
            overrides: HashMap::from([
                (key.into(), "retire-without-draining".into()),
                ("TWO_TICKET_COOLDOWN_SECONDS".into(), "60".into()),
            ]),
        }),
        Instant::now(),
    );
    assert_eq!(live.get("100", key), Some(json!([])));
    // Empty arrays render as an empty env string, not JSON.
    assert_eq!(
        live.env_snapshot(Some("100")).get(key).map(String::as_str),
        Some("")
    );
    assert_eq!(
        live.get("100", "TWO_TICKET_COOLDOWN_SECONDS"),
        Some(json!("60"))
    );
}
