//! S2 measured prototype ([TOG-9807](/TOG/issues/TOG-9807)): real twilight
//! gateway connect + command register + one slash-command answer against the
//! mock Discord double in `common`, with RSS/CPU sampled from `/proc`.
//!
//! Gate: process RSS < 200 MiB confirms the `lite` Container placement, else
//! `basic`. The measured numbers are printed by this test and recorded in
//! `docs/s2-prototype.md`; the test itself asserts the functional behaviour
//! (connects, registers, answers) so CI stays deterministic across hosts.
//!
//! Dev-only: never ships in the release binary.

mod common;

use std::time::{Duration, Instant};

use common::{MockDiscord, APP_ID, GUILD_ID, INTERACTION_ID, INTERACTION_TOKEN};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_model::{
    application::interaction::InteractionData,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::Id,
};

const OP_TIMEOUT: Duration = Duration::from_secs(20);

/// Resident set size of this process in MiB, from `/proc/self/status`.
fn rss_mib() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let kb: f64 = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .expect("VmRSS line")
        .split_whitespace()
        .nth(1)
        .expect("VmRSS value")
        .parse()
        .expect("VmRSS parses");
    kb / 1024.0
}

/// Total user + system CPU ticks of this process, from `/proc/self/stat`.
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    let after_comm = stat.rsplit(')').next().expect("comm");
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // utime is field 14, stime field 15; fields[0] here is field 3 (state).
    let utime: u64 = fields[11].parse().expect("utime parses");
    let stime: u64 = fields[12].parse().expect("stime parses");
    utime + stime
}

#[tokio::test]
async fn gateway_connect_register_answer_one_slash_command() {
    let wall_start = Instant::now();
    let cpu_start = cpu_ticks();

    let mut mock = MockDiscord::start().await;
    let proxy_host = format!("{}:{}", mock.http_addr.ip(), mock.http_addr.port());

    // 1. Register `/ping` through the real HTTP client against the double.
    let http = twilight_http::Client::builder()
        .token("s2-prototype-token".to_owned())
        .proxy(proxy_host, true)
        .build();
    let app_id = Id::new(APP_ID);
    let command = http
        .interaction(app_id)
        .create_guild_command(Id::new(GUILD_ID))
        .chat_input("ping", "S2 prototype ping")
        .await
        .expect("register /ping")
        .model()
        .await
        .expect("command body parses");
    assert_eq!(command.name, "ping");

    // 2. Connect the real gateway shard through the mock gateway double.
    let config =
        twilight_gateway::ConfigBuilder::new("s2-prototype-token".to_owned(), Intents::GUILDS)
            .proxy_url(format!("ws://{addr}", addr = mock.gw_addr))
            .build();
    let mut shard = Shard::with_config(ShardId::ONE, config);
    let wanted = EventTypeFlags::READY | EventTypeFlags::INTERACTION_CREATE;

    let event = tokio::time::timeout(OP_TIMEOUT, shard.next_event(wanted))
        .await
        .expect("READY in time")
        .expect("shard stream alive")
        .expect("READY parses");
    assert!(matches!(event, Event::Ready(_)), "first event is READY");

    // 3. Idle window: settle, then sample idle RSS and CPU.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let rss_idle = rss_mib();
    let idle_cpu_pct =
        (cpu_ticks() - cpu_start) as f64 / 100.0 / wall_start.elapsed().as_secs_f64() * 100.0;

    // 4. Fire the interaction, answer `/ping` through the real client.
    mock.fire_interaction();
    let event = tokio::time::timeout(OP_TIMEOUT, shard.next_event(wanted))
        .await
        .expect("INTERACTION_CREATE in time")
        .expect("shard stream alive")
        .expect("interaction parses");
    let Event::InteractionCreate(interaction) = event else {
        panic!("second event is INTERACTION_CREATE, got {event:?}");
    };
    let resolved_name = match interaction.data.as_ref().expect("command data") {
        InteractionData::ApplicationCommand(data) => data.name.clone(),
        other => panic!("expected application command, got {other:?}"),
    };
    assert_eq!(resolved_name, "ping");

    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some("pong (S2 prototype)".to_owned()),
            ..Default::default()
        }),
    };
    let callback = http
        .interaction(app_id)
        .create_response(interaction.id, &interaction.token, &response)
        .await
        .expect("interaction callback posts");
    assert_eq!(callback.status().get(), 204);

    // 5. Post-command peak RSS (a few samples; the answer path is synchronous).
    let mut rss_peak = rss_mib();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        rss_peak = rss_peak.max(rss_mib());
    }

    let requests = mock.requests();
    // Paths carry the `/api/v10` prefix: twilight builds
    // `{protocol}://{host}/api/v{API_VERSION}/{route}` around the route.
    assert!(
        requests.iter().any(|r| r.method == "POST"
            && r.path == format!("/api/v10/applications/{APP_ID}/guilds/{GUILD_ID}/commands")
            && String::from_utf8_lossy(&r.body).contains("\"ping\"")),
        "command registration hit the mock: {requests:?}",
    );
    assert!(
        requests.iter().any(|r| r.method == "POST"
            && r.path
                == format!("/api/v10/interactions/{INTERACTION_ID}/{INTERACTION_TOKEN}/callback")
            && String::from_utf8_lossy(&r.body).contains("pong (S2 prototype)")),
        "interaction callback hit the mock: {requests:?}",
    );

    mock.shutdown().await;

    eprintln!("S2 RSS/CPU (test-process bound, conservative: includes the mock double)");
    eprintln!("| phase | RSS MiB | CPU %1-core |");
    eprintln!("| idle (post-READY, 5 s settle) | {rss_idle:.1} | {idle_cpu_pct:.1}% |");
    eprintln!("| post-command peak | {rss_peak:.1} | — |");
    eprintln!(
        "GATE: peak {rss_peak:.1} MiB {} 200 MiB -> {}",
        if rss_peak < 200.0 { "<" } else { ">=" },
        if rss_peak < 200.0 { "lite" } else { "basic" },
    );
}
