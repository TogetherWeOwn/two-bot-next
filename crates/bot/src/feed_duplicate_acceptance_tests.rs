//! Protected failing-first saved-relay recovery contract. Run against the real
//! command runtime and store, never an implementation of duplicate handling.
#![cfg(test)]

use super::*;
use serde_json::Value;
use twilight_model::application::interaction::Interaction;
use two_bot_core::RouterGates;

use crate::command_runtime::{router_with_commands, CommandRuntime};

const YOUTUBE: &str =
    "https://www.youtube.com/feeds/videos.xml?channel_id=UCabcdefghijklmnopqrstuv";
const FAILURE: &str = "Feed command failed; try again.";

/// Test-only observation boundary, not a duplicate implementation. The baseline
/// store returns (); a Builder adds a match over its typed outcome in the parent
/// test module. Existing must come from that outcome, not a second DB lookup.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AddObservation {
    Created(String),
    Existing { id: String, enabled: bool },
}

pub(super) trait ObserveAdd {
    fn observe(&self, attempted: &FeedRelay) -> AddObservation;
}

impl ObserveAdd for () {
    fn observe(&self, attempted: &FeedRelay) -> AddObservation {
        AddObservation::Created(attempted.id.clone())
    }
}

fn observe<T: ObserveAdd, E>(
    result: Result<T, E>,
    attempted: &FeedRelay,
) -> Result<AddObservation, E> {
    result.map(|outcome| outcome.observe(attempted))
}

fn add(kind: &str, source: &str, channel: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"1111", "authorizing_integration_owners":{"0":GUILD},
        "id":"7007", "token":"synthetic-feed-duplicate-token", "type":2,
        "guild_id":GUILD, "channel_id":channel, "entitlements":[],
        "member":{"user":{"id":"88","username":"fixture-admin","discriminator":"0", "bot":false,"mfa_enabled":false},
                  "permissions":"32","roles":[],"deaf":false,"mute":false,"flags":0,"joined_at":null},
        "data":{"id":"1","name":"feed-add","type":1,"options":[
            {"name":"kind","type":3,"value":kind},
            {"name":"source","type":3,"value":source}
        ]}
    })).expect("synthetic slash interaction")
}

fn runtime(pool: &PgPool, mock: &MockRest) -> CommandRuntime {
    CommandRuntime::new(
        pool.clone(),
        executor(mock),
        router_with_commands(RouterGates {
            configured_guild: Some(2222),
            scorecard: false,
            automations: true,
            announcements: true,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        }),
        2222,
        true,
    )
}

async fn invoke(pool: &PgPool, interaction: &Interaction) -> String {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id":"9001"})),
    )
    .await;
    runtime(pool, &mock).on_interaction(interaction).await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        requests.len(),
        2,
        "only defer and completion, no channel post/read"
    );
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    assert_eq!(requests[1].method, "PATCH");
    assert!(requests[1].path.ends_with("/messages/@original"));
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    let content = body["content"].as_str().unwrap().to_owned();
    assert!(content.encode_utf16().count() <= 2000);
    content
}

async fn snapshot(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'relays', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY id), '[]'::jsonb) FROM feed_relays r),
          'deliveries', (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY feed_id, item_key), '[]'::jsonb) FROM feed_deliveries d))",
    ).fetch_one(pool).await.expect("snapshot every relay and delivery column")
}

type Audit = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

async fn audits(pool: &PgPool) -> Vec<Audit> {
    sqlx::query_as(
        "SELECT action, target_key, outcome, reason, actor_id FROM announcements_audit_log ORDER BY created_at, id",
    ).fetch_all(pool).await.expect("read truthful audit outcomes")
}

async fn seed_history(pool: &PgPool, relay: &FeedRelay) {
    let mut active = relay.clone();
    active.enabled = true;
    store::add_feed(pool, &active).await.unwrap();
    let post = seed_pending(pool, &active, 0).await;
    assert!(store::mark_delivered(
        pool,
        GUILD,
        &post,
        "pending-seed-0",
        "9001",
        now_millis_for_test(),
    )
    .await
    .unwrap());
    seed_pending(pool, &active, 1).await;
    store::mark_checked(pool, GUILD, &relay.id, 123456)
        .await
        .unwrap();
    sqlx::query("UPDATE feed_relays SET enabled = $2 WHERE id = $1")
        .bind(&relay.id)
        .bind(relay.enabled)
        .execute(pool)
        .await
        .unwrap();
}

fn existing_reply(reply: &str, id: &str, enabled: bool) {
    let lower = reply.to_ascii_lowercase();
    assert!(
        lower.contains("already") || lower.contains("existing"),
        "identify saved relay, not an outage: {reply}"
    );
    assert!(
        reply.contains(&format!("`{id}`")),
        "copyable original relay ID: {reply}"
    );
    let state = if enabled { "enabled" } else { "paused" };
    assert!(
        lower.contains(state),
        "observed saved state {state}: {reply}"
    );
    assert!(
        reply.contains(&format!("/feed-inspect id:{id}")),
        "copyable inspect guidance: {reply}"
    );
    let command = if enabled { "feed-pause" } else { "feed-resume" };
    assert!(
        reply.contains(&format!("/{command} id:{id}")),
        "copyable state-appropriate remedy: {reply}"
    );
    for forbidden in [
        "https://",
        "http://",
        "@everyone",
        "@here",
        "sqlx",
        "postgres",
        "23505",
        "claim",
        "Feed relay created:",
        FAILURE,
    ] {
        assert!(
            !reply.contains(forbidden),
            "private reply must not leak source/error/capability or claim creation: {reply}"
        );
    }
}

fn no_creation_audit(rows: &[Audit]) {
    assert!(
        rows.iter().all(|row| row.0 != "feed.create"
            || !matches!(row.2.as_str(), "rss" | "youtube" | "twitch" | "rejected")),
        "duplicate is neither successful creation nor rejected storage: {rows:?}"
    );
}

// Catches INSERT-error-as-outage, upsert/resume/reset, leaked source/capability,
// and auditing a non-creation as feed.create. Both ledger states are seeded.
async fn duplicate_command(enabled: bool, source: &str) {
    let db = fixture().await;
    let mut relay = feed("original-relay");
    relay.source = source.to_owned();
    relay.enabled = enabled;
    seed_history(db.pool(), &relay).await;
    let before = snapshot(db.pool()).await;
    let reply = invoke(db.pool(), &add("rss", source, CHANNEL)).await;
    let after = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    assert_eq!(
        after, before,
        "preserve creator, timestamps, enabled, high water, claim/nonce/delivery ledger"
    );
    no_creation_audit(&rows);
    existing_reply(&reply, &relay.id, enabled);
}

#[tokio::test]
async fn enabled_duplicate_returns_original_id_without_any_relay_or_ledger_update() {
    duplicate_command(true, "https://example.org/original.xml?format=rss").await;
}

#[tokio::test]
async fn paused_duplicate_stays_paused_and_offers_resume_not_recreation() {
    duplicate_command(false, "https://example.org/paused.xml?format=rss").await;
}

#[tokio::test]
async fn duplicate_reply_does_not_echo_mention_bearing_or_long_source() {
    let source = format!(
        "https://example.org/@everyone/{}?key=synthetic-private-query",
        "x".repeat(1500)
    );
    duplicate_command(false, &source).await;
}

#[tokio::test]
async fn youtube_channel_id_and_canonical_feed_url_find_the_same_saved_relay() {
    let db = fixture().await;
    let mut relay = feed("youtube-original");
    relay.kind = FeedKind::Youtube;
    relay.source = YOUTUBE.to_owned();
    seed_history(db.pool(), &relay).await;
    let before = snapshot(db.pool()).await;
    let mut replies = Vec::new();
    for source in ["UCabcdefghijklmnopqrstuv", YOUTUBE] {
        replies.push(invoke(db.pool(), &add("youtube", source, CHANNEL)).await);
    }
    let after = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    assert_eq!(after, before);
    no_creation_audit(&rows);
    for reply in replies {
        existing_reply(&reply, &relay.id, true);
    }
}

// The test observation must come from the typed store outcome, not from a
// runtime query or from the newly attempted (never persisted) ID/state.
#[tokio::test]
async fn store_duplicate_reports_saved_id_and_state_instead_of_attempt_identity() {
    let db = fixture().await;
    let mut outcomes = Vec::new();
    for enabled in [true, false] {
        let mut original = feed(if enabled {
            "store-enabled"
        } else {
            "store-paused"
        });
        original.enabled = enabled;
        seed_history(db.pool(), &original).await;
        let before = snapshot(db.pool()).await;
        let mut attempted = original.clone();
        attempted.id = format!("attempt-{}", original.id);
        attempted.enabled = !enabled;
        attempted.created_by = "88".into();
        attempted.created_at = 999999;
        attempted.updated_at = 999999;
        let result = observe(store::add_feed(db.pool(), &attempted).await, &attempted);
        let after = snapshot(db.pool()).await;
        outcomes.push((result, original.id, enabled, before, after));
    }
    db.close().await.unwrap();
    for (result, id, enabled, before, after) in outcomes {
        assert_eq!(after, before);
        assert!(
            result.is_ok(),
            "exact saved tuple is an explicit non-creation, not a SQL error"
        );
        assert_eq!(result.unwrap(), AddObservation::Existing { id, enabled });
    }
}

#[tokio::test]
async fn healthy_new_add_keeps_creation_reply_creator_and_single_creation_audit() {
    let db = fixture().await;
    let reply = invoke(
        db.pool(),
        &add("rss", "https://example.org/new.xml", CHANNEL),
    )
    .await;
    let saved = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    let relays = saved["relays"].as_array().unwrap();
    assert_eq!(relays.len(), 1);
    let relay = &relays[0];
    let id = relay["id"].as_str().unwrap();
    assert_eq!(relay["source"], "https://example.org/new.xml");
    assert_eq!(relay["created_by"], "88");
    assert_eq!(relay["enabled"], true);
    assert_eq!(saved["deliveries"], json!([]));
    assert_eq!(reply, format!("Feed relay created: `{id}`."));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "feed.create");
    assert_eq!(rows[0].1.as_deref(), Some(id));
    assert_eq!(rows[0].2, "rss");
    assert_eq!(rows[0].4.as_deref(), Some("88"));
}

#[tokio::test]
async fn same_source_in_different_channel_is_a_distinct_allowed_relay() {
    let db = fixture().await;
    let original = feed("other-channel-original");
    store::add_feed(db.pool(), &original).await.unwrap();
    let reply = invoke(db.pool(), &add("rss", &original.source, "6666")).await;
    let saved = snapshot(db.pool()).await;
    db.close().await.unwrap();
    let relays = saved["relays"].as_array().unwrap();
    assert_eq!(relays.len(), 2);
    let second = relays.iter().find(|r| r["channel_id"] == "6666").unwrap();
    assert_ne!(second["id"], original.id);
    assert_eq!(second["source"], original.source);
    assert_eq!(
        reply,
        format!("Feed relay created: `{}`.", second["id"].as_str().unwrap())
    );
}

#[tokio::test]
async fn store_identity_keeps_guild_kind_and_exact_rss_source_distinct() {
    let db = fixture().await;
    let original = feed("tuple-original");
    store::add_feed(db.pool(), &original).await.unwrap();
    let mut candidates = Vec::new();
    let mut other = original.clone();
    other.id = "other-guild".into();
    other.guild_id = "7777".into();
    candidates.push(other);
    let mut other = original.clone();
    other.id = "other-kind".into();
    other.kind = FeedKind::Twitch;
    candidates.push(other);
    let mut other = original.clone();
    other.id = "other-rss-url".into();
    other.source.push_str("?variant=1");
    candidates.push(other);
    let mut outcomes = Vec::new();
    for candidate in &candidates {
        outcomes.push(observe(
            store::add_feed(db.pool(), candidate).await,
            candidate,
        ));
    }
    let saved = snapshot(db.pool()).await;
    db.close().await.unwrap();
    assert_eq!(saved["relays"].as_array().unwrap().len(), 4);
    for (candidate, outcome) in candidates.iter().zip(outcomes) {
        assert_eq!(
            outcome.unwrap(),
            AddObservation::Created(candidate.id.clone())
        );
    }
}

#[tokio::test]
async fn concurrent_identical_store_adds_return_one_created_and_one_existing() {
    let db = fixture().await;
    let peer = db.independent_pool().await.unwrap();
    let first = feed("race-first");
    let mut second = first.clone();
    second.id = "race-second".into();
    second.created_by = "88".into();
    let (a, b) = tokio::join!(
        store::add_feed(db.pool(), &first),
        store::add_feed(&peer, &second)
    );
    let a = observe(a, &first);
    let b = observe(b, &second);
    let saved = snapshot(db.pool()).await;
    peer.close().await;
    db.close().await.unwrap();
    assert_eq!(saved["relays"].as_array().unwrap().len(), 1);
    let id = saved["relays"][0]["id"].as_str().unwrap();
    assert!(
        a.is_ok() && b.is_ok(),
        "both callers need truthful successful store outcomes, never a race SQL error"
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| **v == AddObservation::Created(id.to_owned()))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| **v
                == AddObservation::Existing {
                    id: id.to_owned(),
                    enabled: true
                })
            .count(),
        1
    );
}

#[tokio::test]
async fn concurrent_identical_commands_report_one_creation_and_one_saved_relay() {
    let db = fixture().await;
    let peer = db.independent_pool().await.unwrap();
    let first = add("rss", "https://example.org/race.xml", CHANNEL);
    let mut second = first.clone();
    second.id = twilight_model::id::Id::new(7008);
    second.token = "synthetic-feed-duplicate-peer".into();
    let (a, b) = tokio::join!(invoke(db.pool(), &first), invoke(&peer, &second));
    let saved = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    peer.close().await;
    db.close().await.unwrap();
    assert_eq!(saved["relays"].as_array().unwrap().len(), 1);
    let id = saved["relays"][0]["id"].as_str().unwrap();
    let created = format!("Feed relay created: `{id}`.");
    let replies = [a, b];
    assert_eq!(replies.iter().filter(|v| **v == created).count(), 1);
    existing_reply(replies.iter().find(|v| **v != created).unwrap(), id, true);
    assert!(rows.iter().all(|row| row.2 != "rejected"));
    let creations: Vec<_> = rows
        .iter()
        .filter(|row| row.0 == "feed.create" && row.2 == "rss")
        .collect();
    assert_eq!(
        creations.len(),
        1,
        "one successful creation; duplicate is not a rejected create either"
    );
    assert_eq!(creations[0].1.as_deref(), Some(id));
    assert_eq!(creations[0].2, "rss");
}

#[tokio::test]
async fn guild_ceiling_keeps_capacity_first_even_for_an_existing_paused_tuple() {
    let db = fixture().await;
    for index in 0..25 {
        let mut relay = feed(&format!("capacity-{index:02}"));
        relay.enabled = false;
        store::add_feed(db.pool(), &relay).await.unwrap();
    }
    let before = snapshot(db.pool()).await;
    let original = feed("capacity-00");
    let reply = invoke(db.pool(), &add("rss", &original.source, CHANNEL)).await;
    let mut attempted = original.clone();
    attempted.id = "over-cap-attempt".into();
    let result = store::add_feed(db.pool(), &attempted).await;
    let after = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    assert_eq!(after, before);
    assert!(
        result.is_err(),
        "capacity precedes existing-tuple discovery"
    );
    assert!(
        reply.contains("25") && reply.to_ascii_lowercase().contains("remove"),
        "retain capacity remedy: {reply}"
    );
    assert!(!reply.contains(FAILURE));
    assert!(
        !reply.contains("/feed-resume"),
        "do not bypass the capacity-first refusal"
    );
    assert!(
        rows.is_empty(),
        "preserve existing capacity audit vocabulary"
    );
}

#[tokio::test]
async fn unrelated_unique_violation_is_still_a_bounded_rejected_storage_failure() {
    let db = fixture().await;
    let original = feed("unrelated-original");
    store::add_feed(db.pool(), &original).await.unwrap();
    sqlx::query("CREATE UNIQUE INDEX acceptance_unrelated_unique ON feed_relays (created_by)")
        .execute(db.pool())
        .await
        .unwrap();
    let before = snapshot(db.pool()).await;
    let mut attempted = original.clone();
    attempted.id = "unrelated-attempt".into();
    attempted.source = "https://example.org/different.xml".into();
    let direct = store::add_feed(db.pool(), &attempted).await;
    // Runtime attempts also collide with the unrelated index, not the tuple.
    let mut interaction = add("rss", &attempted.source, CHANNEL);
    interaction
        .member
        .as_mut()
        .unwrap()
        .user
        .as_mut()
        .unwrap()
        .id = twilight_model::id::Id::new(4444);
    let reply = invoke(db.pool(), &interaction).await;
    let after = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    assert!(
        direct.is_err(),
        "never classify arbitrary SQLSTATE 23505 as Existing"
    );
    assert_eq!(after, before);
    assert_eq!(reply, FAILURE);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "feed.create");
    assert_eq!(rows[0].2, "rejected");
    assert_eq!(rows[0].4.as_deref(), Some("4444"));
    let reason = rows[0].3.as_deref().unwrap();
    assert!(!reason.is_empty() && reason.len() <= 64 && !reason.chars().any(char::is_whitespace));
    for leaked in [
        "23505",
        "acceptance_unrelated_unique",
        "feed_relays",
        "example.org",
    ] {
        assert!(!reason.contains(leaked));
    }
}

#[tokio::test]
async fn primary_key_collision_with_another_tuple_is_not_saved_tuple_recovery() {
    let db = fixture().await;
    let original = feed("primary-key-original");
    store::add_feed(db.pool(), &original).await.unwrap();
    let before = snapshot(db.pool()).await;
    let mut attempted = original.clone();
    attempted.source = "https://example.org/other.xml".into();
    attempted.channel_id = "6666".into();
    let result = store::add_feed(db.pool(), &attempted).await;
    let after = snapshot(db.pool()).await;
    db.close().await.unwrap();
    assert!(result.is_err());
    assert_eq!(after, before);
}

#[tokio::test]
async fn closed_pool_never_becomes_an_existing_relay_or_leaks_database_details() {
    let db = fixture().await;
    let relay = feed("outage-original");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let peer = db.independent_pool().await.unwrap();
    let before = snapshot(&peer).await;
    db.pool().close().await;
    let reply = invoke(db.pool(), &add("rss", &relay.source, CHANNEL)).await;
    let result = store::add_feed(db.pool(), &relay).await;
    let after = snapshot(&peer).await;
    peer.close().await;
    db.close().await.unwrap();
    assert!(result.is_err());
    assert_eq!(reply, FAILURE);
    assert_eq!(after, before);
}

#[tokio::test]
async fn malformed_add_options_cannot_discover_or_modify_a_saved_relay() {
    let db = fixture().await;
    let relay = feed("malformed-original");
    seed_history(db.pool(), &relay).await;
    let before = snapshot(db.pool()).await;
    let valid = serde_json::to_value(add("rss", &relay.source, CHANNEL)).unwrap();
    let mut replies = Vec::new();
    for options in [
        json!([{"name":"source","type":3,"value":relay.source}]),
        json!([{"name":"kind","type":3,"value":"rss"},{"name":"source","type":4,"value":42}]),
        json!([{"name":"kind","type":3,"value":"rss"},{"name":"kind","type":3,"value":"youtube"},{"name":"source","type":3,"value":relay.source}]),
        json!([{"name":"kind","type":3,"value":"rss"},{"name":"source","type":3,"value":relay.source},{"name":"source","type":3,"value":"https://example.org/other.xml"}]),
    ] {
        let mut wire = valid.clone();
        wire["data"]["options"] = options;
        let interaction = serde_json::from_value(wire).unwrap();
        replies.push(invoke(db.pool(), &interaction).await);
    }
    let after = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    assert_eq!(after, before);
    assert!(rows.is_empty());
    for reply in replies {
        assert!(
            !reply.contains(&relay.id),
            "decoding must precede saved-relay discovery"
        );
        assert!(!reply.contains("Feed relay created:"));
        assert_ne!(reply, FAILURE);
    }
}

#[tokio::test]
async fn foreign_guild_add_stays_silent_without_disclosing_saved_identity() {
    let db = fixture().await;
    let relay = feed("guild-fence-original");
    seed_history(db.pool(), &relay).await;
    let before = snapshot(db.pool()).await;
    let mut interaction = add("rss", &relay.source, CHANNEL);
    interaction.guild_id = Some(twilight_model::id::Id::new(7777));
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    runtime(db.pool(), &mock).on_interaction(&interaction).await;
    let requests = mock.requests();
    mock.shutdown().await;
    let after = snapshot(db.pool()).await;
    let rows = audits(db.pool()).await;
    db.close().await.unwrap();
    assert_eq!(after, before);
    assert!(rows.is_empty());
    assert!(
        requests.is_empty(),
        "foreign configured-guild fence stays silent"
    );
}
