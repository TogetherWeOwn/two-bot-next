//! Protected acceptance: the dispatched private active-draw list is a saved
//! projection, not a delivery receipt, countdown for paused time, or write.
//! Mutations caught: missing destinations/deadlines, fabricated pending links,
//! silent row loss, unmarked prize cuts, UTF-16 overflow, cross-guild disclosure,
//! empty-on-error, and even attempted writes that roll back or undo themselves.
#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use two_bot_testsupport::TestDatabase;

const DESTINATION: &str = "8800000000000000001";
const RECORDED_MESSAGE: &str = "8900000000000000001";
const ENDS: i64 = 1_900_000_000;
const PAUSED: i64 = 1_899_999_400;
const SAVED_ID: &str = "draw-reference-01";
const PENDING_ID: &str = "draw-reference-02";
const PAUSED_ID: &str = "draw-reference-03";
const FOREIGN_ID: &str = "foreign-draw-do-not-disclose";

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit disposable test bootstrap required; never use application credentials");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create owned disposable giveaway fixture")
}

async fn runtime(pool: &PgPool, enabled: bool) -> (Arc<CommandRuntime>, MockRest) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor_at(origin),
        router_with_commands(gates(enabled, enabled)),
        GUILD,
        true,
    );
    (runtime, mock)
}

fn list(id: u64, token: &str) -> Interaction {
    let mut interaction = slash(
        "giveaway",
        Some(CHANNEL),
        vec![option("list", CommandOptionValue::SubCommand(Vec::new()))],
    );
    interaction.id = Id::new(id);
    interaction.token = token.to_owned();
    interaction
}

async fn seed(
    pool: &PgPool,
    guild: &str,
    id: &str,
    prize: &str,
    destination: &str,
    message: &str,
    paused: bool,
) {
    // Seed facts rather than issuing create/pause: listing must work after a
    // restart, including a definition whose delivery has no recorded message.
    sqlx::query(
        "INSERT INTO giveaways
         (guild_id, id, prize, channel_id, message_id, created_by, winners_count,
          ends_at, status, requirements_json, max_entries, draw_nonce, paused_at,
          created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, '77', 1,
                 to_timestamp($6::double precision), $7, '{}'::jsonb, 100,
                 md5(random()::text), to_timestamp($8::double precision),
                 to_timestamp(1800000000), to_timestamp(1800000000))",
    )
    .bind(guild)
    .bind(id)
    .bind(prize)
    .bind(destination)
    .bind(message)
    .bind(ENDS as f64)
    .bind(if paused { "paused" } else { "open" })
    .bind(paused.then_some(PAUSED as f64))
    .execute(pool)
    .await
    .expect("seed saved giveaway facts in owned disposable database");
}

async fn snapshot(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'definitions', COALESCE((SELECT jsonb_agg(to_jsonb(g) ORDER BY guild_id, id)
                                 FROM giveaways g), '[]'::jsonb),
          'entries', COALESCE((SELECT jsonb_agg(to_jsonb(e) ORDER BY guild_id, giveaway_id, user_id)
                             FROM giveaway_entries e), '[]'::jsonb),
          'draws', COALESCE((SELECT jsonb_agg(to_jsonb(d) ORDER BY guild_id, giveaway_id, draw_no)
                           FROM giveaway_draws d), '[]'::jsonb),
          'audits', COALESCE((SELECT jsonb_agg(to_jsonb(a) ORDER BY id)
                            FROM automation_audit_log a), '[]'::jsonb))",
    )
    .fetch_one(pool)
    .await
    .expect("snapshot full giveaway state, nonce, leases, budget inputs and audits")
}

async fn fence_writes(pool: &PgPool) {
    // A sequence is nontransactional: an attempted write is evidence even if
    // the handler rolls back, restores its old value, or swallows an SQL error.
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE SEQUENCE list_write_attempt;
         CREATE FUNCTION observe_list_write() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM nextval('list_write_attempt');
           RETURN NULL;
         END;
         $$;
         CREATE TRIGGER observe_definition_write BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
           ON giveaways FOR EACH STATEMENT EXECUTE FUNCTION observe_list_write();
         CREATE TRIGGER observe_entry_write BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
           ON giveaway_entries FOR EACH STATEMENT EXECUTE FUNCTION observe_list_write();
         CREATE TRIGGER observe_draw_write BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
           ON giveaway_draws FOR EACH STATEMENT EXECUTE FUNCTION observe_list_write();
         CREATE TRIGGER observe_audit_write BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE
           ON automation_audit_log FOR EACH STATEMENT EXECUTE FUNCTION observe_list_write();",
    ))
    .execute(pool)
    .await
    .expect("install write observation in owned disposable fixture only");
}

async fn assert_read_only(pool: &PgPool, before: &Value) {
    assert_eq!(
        &snapshot(pool).await,
        before,
        "list preserves every saved fact"
    );
    let attempted: bool = sqlx::query_scalar("SELECT is_called FROM list_write_attempt")
        .fetch_one(pool)
        .await
        .expect("nontransactional attempt witness");
    assert!(
        !attempted,
        "list must not even attempt lifecycle, entry, claim, nonce or applied-audit writes"
    );
}

fn reply_text(body: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(content) = body["content"].as_str() {
        parts.push(content.to_owned());
    }
    if let Some(embeds) = body["embeds"].as_array() {
        for embed in embeds {
            for key in ["title", "description"] {
                if let Some(text) = embed[key].as_str() {
                    parts.push(text.to_owned());
                }
            }
            if let Some(fields) = embed["fields"].as_array() {
                for field in fields {
                    for key in ["name", "value"] {
                        if let Some(text) = field[key].as_str() {
                            parts.push(text.to_owned());
                        }
                    }
                }
            }
        }
    }
    parts.join("\n")
}

fn replies(mock: &MockRest, interaction: &Interaction) -> Vec<String> {
    let callback = format!(
        "/api/v10/interactions/{}/{}/callback",
        interaction.id, interaction.token
    );
    let original = format!(
        "/api/v10/webhooks/1111/{}/messages/@original",
        interaction.token
    );
    let followup = format!("/api/v10/webhooks/1111/{}", interaction.token);
    let mut messages = Vec::new();
    let mut acknowledged_private = false;
    for request in mock.requests() {
        let json: Value = serde_json::from_slice(&request.body).expect("outbound reply JSON");
        let body = if request.method == "POST" && request.path == callback {
            assert_eq!(
                json["data"]["flags"].as_u64().unwrap_or(0) & 64,
                64,
                "initial acknowledgement must be private"
            );
            acknowledged_private = true;
            &json["data"]
        } else if request.method == "PATCH" && request.path == original {
            assert!(
                acknowledged_private,
                "completion inherits a private acknowledgement"
            );
            &json
        } else if request.method == "POST" && request.path == followup {
            assert_eq!(
                json["flags"].as_u64().unwrap_or(0) & 64,
                64,
                "continuation is private"
            );
            &json
        } else {
            panic!("list may only answer its own interaction, no public sends/edits, channel reads or other REST: {} {}", request.method, request.path);
        };
        let text = reply_text(body);
        if text.is_empty() {
            continue; // a private defer carries no user-visible list text
        }
        assert_eq!(
            body["allowed_mentions"]["parse"],
            json!([]),
            "suppress automatic mentions"
        );
        for key in ["users", "roles"] {
            assert!(
                body["allowed_mentions"][key]
                    .as_array()
                    .is_none_or(Vec::is_empty),
                "no explicit mention allowlist"
            );
        }
        assert_ne!(body["allowed_mentions"]["replied_user"], true);
        assert!(
            text.encode_utf16().count() <= 2000,
            "complete reply exceeds Discord UTF-16 bound"
        );
        assert!(!text.contains('\u{fffd}'), "no lossy Unicode cuts");
        messages.push(text);
    }
    assert!(acknowledged_private, "dispatch must answer the interaction");
    assert!(!messages.is_empty(), "a defer alone is not a list answer");
    assert!(
        messages.len() <= 3,
        "ten active draws need a finite bounded reply sequence"
    );
    if messages.len() > 1 {
        let first = messages[0].to_lowercase();
        assert!(
            ["continu", "page", "more", "part"]
                .iter()
                .any(|word| first.contains(word)),
            "continuation must be explicit, not silently lost rows"
        );
    }
    messages
}

fn row<'a>(text: &'a str, id: &str) -> &'a str {
    let rows: Vec<_> = text.lines().filter(|line| line.contains(id)).collect();
    assert_eq!(
        rows.len(),
        1,
        "exactly one complete row for stable id {id}: {text}"
    );
    rows[0]
}

fn has_any(text: &str, words: &[&str]) -> bool {
    let lower = text.to_lowercase();
    words.iter().any(|word| lower.contains(word))
}

fn has_message_link(text: &str) -> bool {
    text.split("https://discord.com/channels/")
        .skip(1)
        .any(|suffix| {
            let path = suffix
                .split([' ', '\n', ')', '>', ']'])
                .next()
                .unwrap_or("");
            path.split('/')
                .nth(2)
                .is_some_and(|message| !message.is_empty())
        })
}

fn assert_saved(row: &str) {
    assert!(
        has_any(row, &["saved", "recorded", "snapshot"]),
        "row describes stored state, not observed delivery: {row}"
    );
    for claim in [
        "successfully delivered",
        "delivery confirmed",
        "message exists",
        "posted successfully",
    ] {
        assert!(
            !row.to_lowercase().contains(claim),
            "saved state is not a live receipt: {row}"
        );
    }
}

#[tokio::test]
async fn recorded_open_row_exposes_saved_destination_message_and_local_deadline() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        SAVED_ID,
        "Small prize",
        DESTINATION,
        RECORDED_MESSAGE,
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    let (runtime, mock) = runtime(db.pool(), true).await;
    let interaction = list(94_001, "list-recorded");
    runtime.on_interaction(&interaction).await;
    let text = replies(&mock, &interaction).join("\n");
    assert_read_only(db.pool(), &before).await;
    mock.shutdown().await;
    db.close().await.expect("drop owned database");

    let row = row(&text, SAVED_ID);
    assert!(
        row.contains("Small prize"),
        "short prize is not elided: {row}"
    );
    assert!(
        row.contains(DESTINATION),
        "saved destination is visible: {row}"
    );
    assert!(
        row.contains(&format!(
            "https://discord.com/channels/{GUILD_S}/{DESTINATION}/{RECORDED_MESSAGE}"
        )),
        "link uses only the recorded guild/channel/message: {row}"
    );
    assert!(
        has_any(row, &["open", "running", "active"]),
        "running state: {row}"
    );
    assert!(
        ['f', 'F', 'R', 'd', 'D', 't', 'T']
            .iter()
            .any(|style| row.contains(&format!("<t:{ENDS}:{style}>")))
            || row.contains(&format!("<t:{ENDS}>")),
        "localized saved deadline: {row}"
    );
    assert_saved(&text);
}

#[tokio::test]
async fn pending_message_is_explicit_and_never_fabricates_a_message_link() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        PENDING_ID,
        "Second prize",
        DESTINATION,
        "",
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    let (runtime, mock) = runtime(db.pool(), true).await;
    let interaction = list(94_002, "list-pending");
    runtime.on_interaction(&interaction).await;
    let text = replies(&mock, &interaction).join("\n");
    assert_read_only(db.pool(), &before).await;
    mock.shutdown().await;
    db.close().await.expect("drop owned database");

    let row = row(&text, PENDING_ID);
    assert!(
        row.contains(DESTINATION),
        "pending message still has a saved destination: {row}"
    );
    assert!(
        has_any(
            row,
            &[
                "pending",
                "unconfirmed",
                "not recorded",
                "unknown",
                "unavailable"
            ]
        ),
        "missing message is distinguishable from delivery: {row}"
    );
    assert!(
        !has_message_link(row),
        "no message link without a recorded message: {row}"
    );
    assert_saved(&text);
}

#[tokio::test]
async fn omitted_destination_detail_is_explicit_not_an_empty_list_or_invented_link() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        SAVED_ID,
        "Small prize",
        "",
        RECORDED_MESSAGE,
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    let (runtime, mock) = runtime(db.pool(), true).await;
    let interaction = list(94_008, "list-missing-detail");
    runtime.on_interaction(&interaction).await;
    let text = replies(&mock, &interaction).join("\n");
    assert_read_only(db.pool(), &before).await;
    mock.shutdown().await;
    db.close().await.expect("drop owned database");

    let row = row(&text, SAVED_ID);
    assert!(
        row.contains("Small prize"),
        "one missing display field does not erase the draw"
    );
    assert!(
        has_any(
            row,
            &[
                "unavailable",
                "unknown",
                "omitted",
                "not recorded",
                "invalid"
            ]
        ),
        "omitted saved destination is distinguishable from empty: {row}"
    );
    assert!(
        !has_message_link(row),
        "no link with an invented destination: {row}"
    );
    assert!(!has_any(&text, &["no active", "no open"]));
}

#[tokio::test]
async fn paused_row_has_no_live_deadline_countdown() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        PAUSED_ID,
        "Third prize",
        DESTINATION,
        RECORDED_MESSAGE,
        true,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    let (runtime, mock) = runtime(db.pool(), true).await;
    let interaction = list(94_003, "list-paused");
    runtime.on_interaction(&interaction).await;
    let text = replies(&mock, &interaction).join("\n");
    assert_read_only(db.pool(), &before).await;
    mock.shutdown().await;
    db.close().await.expect("drop owned database");

    let row = row(&text, PAUSED_ID);
    assert!(
        has_any(row, &["paused"]),
        "paused is not presented as open: {row}"
    );
    assert!(
        row.contains(DESTINATION),
        "saved destination survives pause: {row}"
    );
    assert!(
        !row.contains(":R>"),
        "paused time must not keep counting down: {row}"
    );
    assert!(
        !has_any(row, &["ends in", "ending in", "closes in"]),
        "no live countdown language: {row}"
    );
    assert_saved(&text);
}

#[tokio::test]
async fn ten_hostile_unicode_prizes_keep_every_id_and_complete_facts_reachable() {
    let db = database().await;
    let mut ids = Vec::new();
    for index in 0..10 {
        let id = format!("draw-{index:02}-{}", "x".repeat(56)); // 64-byte stable id
        let prize = format!("Prize{index:02} @everyone `\n{}", "🎁".repeat(178));
        assert!(prize.chars().count() <= 200, "within existing prize quota");
        let destination = format!("88000000000000000{index:02}");
        let message = format!("89000000000000000{index:02}");
        seed(
            db.pool(),
            GUILD_S,
            &id,
            &prize,
            &destination,
            if index % 2 == 0 { &message } else { "" },
            index % 3 == 0,
        )
        .await;
        ids.push((id, destination, message));
    }
    seed(
        db.pool(),
        "9999",
        FOREIGN_ID,
        "Foreign secret prize",
        "8800000000000000099",
        "8900000000000000099",
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    let (runtime, mock) = runtime(db.pool(), true).await;
    let interaction = list(94_004, "list-ten-unicode");
    runtime.on_interaction(&interaction).await;
    let text = replies(&mock, &interaction).join("\n");
    assert_read_only(db.pool(), &before).await;
    mock.shutdown().await;
    db.close().await.expect("drop owned database");

    assert!(
        !text.contains(FOREIGN_ID) && !text.contains("Foreign secret prize"),
        "guild-isolated list"
    );
    for (index, (id, destination, message)) in ids.iter().enumerate() {
        assert_eq!(
            text.matches(id).count(),
            1,
            "every complete stable id exactly once, never five-row or byte-budget loss: {id}"
        );
        let row = row(&text, id);
        assert!(
            row.contains(&format!("Prize{index:02}")),
            "row retains identifiable prize prefix: {row}"
        );
        assert!(
            row.contains('…')
                || row.contains("...")
                || has_any(row, &["elided", "truncated", "omitted"]),
            "bounded prize is explicitly elided: {row}"
        );
        assert!(
            row.contains(destination),
            "whole saved destination, never a cut snowflake: {row}"
        );
        assert_saved(&text);
        if index % 2 == 0 {
            assert!(
                row.contains(&format!(
                    "https://discord.com/channels/{GUILD_S}/{destination}/{message}"
                )),
                "recorded link belongs to its row: {row}"
            );
        } else {
            assert!(
                !has_message_link(row),
                "no invented pending-message link: {row}"
            );
            assert!(has_any(
                row,
                &[
                    "pending",
                    "unconfirmed",
                    "not recorded",
                    "unknown",
                    "unavailable"
                ]
            ));
        }
        if index % 3 == 0 {
            assert!(has_any(row, &["paused"]));
            assert!(!row.contains(":R>"), "paused row has no running countdown");
        } else {
            assert!(
                row.contains(&format!("<t:{ENDS}")),
                "running row retains localized deadline: {row}"
            );
        }
    }
}

#[tokio::test]
async fn empty_control_is_private_read_only_and_distinct_from_unavailable() {
    let db = database().await;
    seed(
        db.pool(),
        "9999",
        FOREIGN_ID,
        "Foreign prize",
        DESTINATION,
        RECORDED_MESSAGE,
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    let (runtime, mock) = runtime(db.pool(), true).await;
    let interaction = list(94_005, "list-empty");
    runtime.on_interaction(&interaction).await;
    let text = replies(&mock, &interaction).join("\n");
    assert_read_only(db.pool(), &before).await;
    mock.shutdown().await;
    db.close().await.expect("drop owned database");

    assert!(
        has_any(&text, &["no active", "no open", "none", "empty"]),
        "honest empty state: {text}"
    );
    assert!(!has_any(&text, &["unavailable", "try again", "failed"]));
    assert!(!text.contains(FOREIGN_ID) && !text.contains("Foreign prize"));
}

#[tokio::test]
async fn unavailable_control_never_masquerades_as_empty_or_leaks_storage_details() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        SAVED_ID,
        "Small prize",
        DESTINATION,
        RECORDED_MESSAGE,
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    // Remove only the read name inside this disposable database. Restore it
    // before checking state; no app credentials or real database is consulted.
    sqlx::query("ALTER TABLE giveaways RENAME TO fixture_unavailable_giveaways")
        .execute(db.pool())
        .await
        .expect("inject local read failure");
    let (runtime, mock) = runtime(db.pool(), true).await;
    let interaction = list(94_006, "list-unavailable");
    runtime.on_interaction(&interaction).await;
    let text = replies(&mock, &interaction).join("\n");
    sqlx::query("ALTER TABLE fixture_unavailable_giveaways RENAME TO giveaways")
        .execute(db.pool())
        .await
        .expect("restore local read name");
    assert_read_only(db.pool(), &before).await;
    mock.shutdown().await;
    db.close().await.expect("drop owned database");

    assert!(
        has_any(&text, &["unavailable", "try again", "could not", "failed"]),
        "failure is not empty: {text}"
    );
    assert!(!has_any(&text, &["no active", "no open"]));
    assert!(!text.contains(SAVED_ID) && !text.contains("Small prize"));
    assert!(!has_any(
        &text,
        &[
            "sqlx",
            "postgres",
            "relation",
            "fixture_unavailable",
            "draw_nonce"
        ]
    ));
}

#[tokio::test]
async fn authority_and_activation_controls_refuse_without_disclosure_or_effects() {
    for case in [
        "staff",
        "missing-permissions",
        "absent-guild",
        "foreign-guild",
        "disabled",
    ] {
        let db = database().await;
        seed(
            db.pool(),
            GUILD_S,
            SAVED_ID,
            "Private staff prize",
            DESTINATION,
            RECORDED_MESSAGE,
            false,
        )
        .await;
        seed(
            db.pool(),
            "9999",
            FOREIGN_ID,
            "Foreign prize",
            DESTINATION,
            RECORDED_MESSAGE,
            false,
        )
        .await;
        let before = snapshot(db.pool()).await;
        fence_writes(db.pool()).await;
        let (runtime, mock) = runtime(db.pool(), case != "disabled").await;
        let mut interaction = list(94_007, &format!("list-refuse-{case}"));
        match case {
            "staff" => {
                interaction.member.as_mut().expect("member").permissions =
                    Some(Permissions::empty())
            }
            "missing-permissions" => {
                interaction.member.as_mut().expect("member").permissions = None
            }
            "absent-guild" => {
                interaction.guild_id = None;
                interaction.member = None;
                interaction.channel_id = None;
                interaction.channel = None;
                if let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_mut() {
                    data.guild_id = None;
                }
            }
            "foreign-guild" => {
                interaction.guild_id = Some(Id::new(9999));
                if let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_mut() {
                    data.guild_id = Some(Id::new(9999));
                }
            }
            "disabled" => {}
            _ => unreachable!("finite gate matrix"),
        }
        runtime.on_interaction(&interaction).await;
        let text = if matches!(case, "absent-guild" | "foreign-guild") {
            // Preserve the existing non-moderation Ignore fence: replying
            // here would widen the configured-guild invocation contract.
            assert!(mock.requests().is_empty(), "{case}: Ignore stays silent");
            String::new()
        } else {
            replies(&mock, &interaction).join("\n")
        };
        assert_read_only(db.pool(), &before).await;
        mock.shutdown().await;
        db.close().await.expect("drop owned database");

        for secret in [
            SAVED_ID,
            FOREIGN_ID,
            "Private staff prize",
            "Foreign prize",
            DESTINATION,
            RECORDED_MESSAGE,
        ] {
            assert!(
                !text.contains(secret),
                "{case} must not disclose stored detail: {text}"
            );
        }
        assert!(
            !has_any(&text, &["no active", "no open"]),
            "refusal is not empty: {case}: {text}"
        );
    }
}

#[tokio::test]
async fn repeated_list_control_never_spends_entry_budget_or_changes_saved_state() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        SAVED_ID,
        "Small prize",
        DESTINATION,
        RECORDED_MESSAGE,
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    fence_writes(db.pool()).await;
    let (runtime, mock) = runtime(db.pool(), true).await;
    // More than the existing ten-write budget, same actor, distinct commands.
    // Every list must still answer; reads never enter that budget lane.
    for index in 0..12 {
        let interaction = list(94_100 + index, &format!("list-read-budget-{index}"));
        let start = mock.requests().len();
        runtime.on_interaction(&interaction).await;
        let requests = mock.requests();
        assert!(requests.len() > start, "every read is answered");
        let text: String = requests[start..]
            .iter()
            .map(|request| {
                let body: Value = serde_json::from_slice(&request.body).expect("reply JSON");
                reply_text(body.get("data").unwrap_or(&body))
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains(SAVED_ID),
            "read {index} is not write-budget exhausted: {text}"
        );
    }
    assert_read_only(db.pool(), &before).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "DROP TRIGGER observe_definition_write ON giveaways;
         DROP TRIGGER observe_entry_write ON giveaway_entries;
         DROP TRIGGER observe_draw_write ON giveaway_draws;
         DROP TRIGGER observe_audit_write ON automation_audit_log;",
    ))
    .execute(db.pool())
    .await
    .expect("finish read-only observation before positive entry control");
    // Now exercise the real entry consumer of the same actor's budget. Merely
    // seeing twelve list replies would miss a debit whose refusal is ignored.
    let mut entry = list(94_200, "entry-after-twelve-reads");
    entry.kind = InteractionType::MessageComponent;
    entry.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: format!("gw:{SAVED_ID}"),
            component_type: ComponentType::Button,
            resolved: None,
            values: Vec::new(),
        },
    )));
    entry.member.as_mut().expect("same member").joined_at =
        Some(Timestamp::from_secs(1_700_000_000).expect("old membership"));
    runtime.on_interaction(&entry).await;
    let entrants: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD_S)
    .bind(SAVED_ID)
    .fetch_all(db.pool())
    .await
    .expect("real entry after read-only list");
    assert_eq!(
        entrants,
        vec!["88".to_owned()],
        "twelve lists do not spend this actor's entry budget"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned database");
}
