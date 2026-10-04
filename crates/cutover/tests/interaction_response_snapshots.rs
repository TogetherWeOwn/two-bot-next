//! Pinned interaction-response shapes for the staging smoke probes.
//!
//! The live staging suite asserts the ACK contract from
//! `docs/interaction-replies.md` for every probe (first callback in budget,
//! mentions suppressed, content within Discord's ceiling). These snapshots pin
//! that wire shape offline — no guild, network, or database — so a builder
//! regression fails here before it reaches staging. The mock harness slice
//! drives the same builders against these fixtures; the live run belongs to
//! the staging-guild smoke suite.
//!
//! The three probes: `rank` (feature success text, ephemeral), `help` (the
//! unknown-name guidance reply — neither `help` nor `ping` is a slash command
//! on this tree, so both take the unknown-name path), and `ping` (the latency
//! line render in the same ephemeral envelope).
//!
//! Dev-only: never ships in the release binary.

use two_bot_core::{
    leveling::{rank_reply, total_xp_for_level, LevelProfile},
    router::replies::{InteractionReply, UNKNOWN_COMMAND_REPLY},
    voice_utilities::ping_render,
    SlashOutcome,
};
use two_bot_discord::{response_for_slash, text_response};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("fixture reads");
    serde_json::from_str(&text).expect("fixture parses")
}

/// The shared ACK contract every probe owes: immediate type 4 callback,
/// ephemeral, exact content, mentions suppressed, within Discord's ceiling.
fn assert_ack_contract(json: &serde_json::Value, content: &str) {
    assert_eq!(json["type"], 4, "answers with a type 4 callback");
    assert_eq!(json["data"]["content"], content);
    assert_eq!(json["data"]["flags"], 64, "ephemeral");
    assert_eq!(
        json["data"]["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "mentions suppressed"
    );
    assert!(
        content.chars().count() <= 2000,
        "content within Discord's ceiling"
    );
}

#[test]
fn rank_snapshot_matches_pin() {
    let profile = LevelProfile {
        guild_id: "2222".to_owned(),
        member_id: "100000000000000001".to_owned(),
        xp: 114,
        level: 1,
        message_xp: 114,
        voice_xp: 0,
        imported_xp: 0,
        rank: Some(3),
        member_count: 50,
        next_level_xp: total_xp_for_level(2),
    };
    let reply = rank_reply(&profile, "FixtureMember");
    assert!(reply.ephemeral, "rank answers ephemerally");
    let response = text_response(InteractionReply::new(
        reply.content.clone(),
        reply.ephemeral,
    ));
    let json = serde_json::to_value(&response).expect("serializes");
    assert_ack_contract(&json, &reply.content);
    assert_eq!(json, fixture("interaction_rank_response.json"));
}

#[test]
fn help_snapshot_matches_pin() {
    // `help` is not a slash command: the router answers `Unknown`, and the
    // guidance reply tells the member to re-pick from the `/` list.
    let response = response_for_slash(&SlashOutcome::Unknown).expect("unknown names answer");
    let json = serde_json::to_value(&response).expect("serializes");
    assert_ack_contract(&json, UNKNOWN_COMMAND_REPLY);
    assert_eq!(json, fixture("interaction_help_response.json"));
}

#[test]
fn ping_snapshot_matches_pin() {
    let content = ping_render(42);
    assert_eq!(content, "Pong! 42ms");
    let response = text_response(InteractionReply::new(content.clone(), true));
    let json = serde_json::to_value(&response).expect("serializes");
    assert_ack_contract(&json, &content);
    assert_eq!(json, fixture("interaction_ping_response.json"));
}
