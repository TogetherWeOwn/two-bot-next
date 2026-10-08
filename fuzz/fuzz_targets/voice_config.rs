#![no_main]

use std::collections::BTreeMap;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use two_bot_core::voice_config::{
    export_configuration, import_configuration, ChannelKind, ChannelReference, GuildInventory,
    VoiceConfigError,
};

fn inventory() -> &'static GuildInventory {
    static INVENTORY: OnceLock<GuildInventory> = OnceLock::new();
    INVENTORY.get_or_init(|| {
        let guild_id = u64::MAX.to_string();
        let channels = [
            ("101", ChannelKind::Voice),
            ("102", ChannelKind::Voice),
            ("103", ChannelKind::Stage),
            ("104", ChannelKind::Text),
            ("105", ChannelKind::Category),
        ]
        .into_iter()
        .map(|(id, kind)| {
            (
                id.to_owned(),
                ChannelReference {
                    guild_id: guild_id.clone(),
                    kind,
                },
            )
        })
        .chain(std::iter::once((
            "999".to_owned(),
            ChannelReference {
                guild_id: "2".to_owned(),
                kind: ChannelKind::Voice,
            },
        )))
        .collect();
        GuildInventory {
            guild_id: guild_id.clone(),
            channels,
            roles: BTreeMap::from([
                ("201".to_owned(), guild_id.clone()),
                (guild_id.clone(), guild_id.clone()),
                ("998".to_owned(), "2".to_owned()),
            ]),
            members: BTreeMap::from([("301".to_owned(), guild_id)]),
        }
    })
}

fuzz_target!(|data: &[u8]| {
    // The inventory is trusted and fixed, never derived from uploaded JSON.
    if let Ok(config) = import_configuration(data, inventory()) {
        let encoded = match export_configuration(&config, inventory()) {
            Ok(encoded) => encoded,
            // A compact upload near the cap grows when pretty-printed; export
            // refuses it rather than emit a file import would reject.
            Err(VoiceConfigError::ExportTooLarge { .. }) => return,
            Err(error) => panic!("an imported configuration must export: {error}"),
        };
        let decoded = import_configuration(&encoded, inventory()).unwrap();
        assert_eq!(decoded, config);
        assert_eq!(export_configuration(&decoded, inventory()).unwrap(), encoded);
    }
});
