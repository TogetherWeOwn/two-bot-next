//! Acceptance: an invalid `GUILD_ID` must not misreport as a missing token.
//!
//! `Config::from_env` fails on a non-numeric guild while the token and
//! database values stay present in the environment. The safe-defaults
//! fallback preserves those values so the EventSub delivery fence reports
//! the store refusal (whose catalog row names both `DATABASE_URL` and
//! `GUILD_ID`) instead of `receiver_token_missing` for a token that exists.
//!
//! Offline: the child refuses before any pool connect, so the fixture
//! database URL is inert, and every secret is a synthetic fixture.

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct SpawnedExit {
    code: Option<i32>,
    logs: String,
}

fn combined_logs(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn boot_with_invalid_guild() -> SpawnedExit {
    // Parse-only: the child refuses before binding, so the port is never
    // bound and a stolen reservation cannot flake the suite.
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let bind = reserved.local_addr().expect("test port addr").to_string();
    drop(reserved);
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("LISTEN_ADDR", "127.0.0.1:0")
        .env("GUILD_ID", "not-a-snowflake")
        .env("TWO_EVENTSUB", "1")
        .env("TWO_EVENTSUB_BIND", bind)
        .env("TWO_TWITCH_WEBHOOK_SECRET", "synthetic-webhook-secret-01")
        .env("TWO_EVENTSUB_DELIVERY", "1")
        .env("TWO_TWITCH_BROADCASTERS", "222,111")
        .env("TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID", "777")
        .env("TWO_EVENTSUB_INVITE_URL", "https://discord.gg/example")
        .env("DISCORD_TOKEN", "synthetic-boot-token")
        .env("DATABASE_URL", "synthetic-database-must-not-connect")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(mut profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        profile.push(".eventsub-child-%p.profraw");
        command.env("LLVM_PROFILE_FILE", profile);
    }
    let mut child = command.spawn().expect("start test bot");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("poll test bot").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill hung test bot");
            let output = child.wait_with_output().expect("reap hung test bot");
            panic!(
                "boot refusal never exited; partial logs: {}",
                combined_logs(&output)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("reap test bot");
    SpawnedExit {
        code: output.status.code(),
        logs: combined_logs(&output),
    }
}

#[test]
fn invalid_guild_with_present_token_refuses_with_the_store_class() {
    let exit = boot_with_invalid_guild();
    assert_eq!(
        exit.code,
        Some(1),
        "boot refusal must stay a fatal exit 1; logs: {}",
        exit.logs
    );
    let refusals: Vec<serde_json::Value> = exit
        .logs
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["message"] == "eventsub startup refused")
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "boot must emit exactly one JSON fatal refusal; logs: {}",
        exit.logs
    );
    let class = refusals[0]["error_class"]
        .as_str()
        .expect("fatal refusal must carry a scalar error_class");
    assert_eq!(
        class, "receiver_store_missing",
        "an invalid guild must surface through the store refusal, not the token class; logs: {}",
        exit.logs
    );
    assert_ne!(
        class, "receiver_token_missing",
        "a present token must never report receiver_token_missing; logs: {}",
        exit.logs
    );
}
