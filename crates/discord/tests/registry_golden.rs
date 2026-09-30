// Same golden comparator as core, at the real guild bulk-set wire boundary.
#[allow(dead_code)]
mod common;
#[path = "../../core/tests/support/registry_parity.rs"]
mod registry_parity;

use common::{MockDiscord, APP_ID, GUILD_ID};
use registry_parity::{all_on_router, assert_registry_parity};
use twilight_model::id::Id;
use two_bot_discord::publish_commands;

#[tokio::test]
async fn guild_bulk_set_wire_json_matches_frozen_legacy() {
    let definitions = all_on_router().publish_set(&[]).expect("full registry");
    let commands = publish_commands(&definitions);
    let mock = MockDiscord::start().await;
    let http = twilight_http::Client::builder()
        .token("registry-test-token".to_owned())
        .proxy(
            format!("{}:{}", mock.http_addr.ip(), mock.http_addr.port()),
            true,
        )
        .build();
    http.interaction(Id::new(APP_ID))
        .set_guild_commands(Id::new(GUILD_ID), &commands)
        .await
        .expect("guild bulk set against local mock");
    let requests: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|request| {
            request.method == "PUT"
                && request.path
                    == format!("/api/v10/applications/{APP_ID}/guilds/{GUILD_ID}/commands")
        })
        .collect();
    assert_eq!(requests.len(), 1, "one complete publish");
    assert_registry_parity(serde_json::from_slice(&requests[0].body).expect("publish JSON"));
}
