//! Protected RED acceptance: `/game-mix` surfaces the guild game mix to staff.
//!
//! TEST AUTHOR OWNERSHIP: this file is the RED handoff. The Builder consumes
//! the pushed branch, implements the command to GREEN these tests, and MUST
//! NOT edit this file. Return wrong tests to the Test Author; do not fix them
//! by editing.
//!
//! Contract under test (from the build card): one staff-gated private command
//! renders the existing per-game aggregate (play time, in-voice overlap,
//! sessions) for the invoking guild. No per-member data anywhere.
//!
//! Pinned surface (the Builder implements exactly this):
//! - Name `/game-mix`, no options, always published (no env gate), DM off,
//!   `default_member_permissions` ManageGuild. Handler admits ADMINISTRATOR or
//!   MANAGE_GUILD only (the `may_manage_server` precedent).
//! - Every outcome is one ephemeral reply (`flags` 64) with
//!   `allowed_mentions.parse == []`, at most 2000 UTF-16 units, via exactly
//!   one `/callback` plus at most one `PATCH .../messages/@original` (no
//!   channel posts, no DMs, no second reply).
//! - Data replies carry exactly one `json` fenced block whose parsed value
//!   has keys `{guild_id, rows, truncated}` plus `hint` iff `rows` is empty.
//!   Each row has exactly `{game, total_play_ms, in_voice_ms, session_count}`.
//! - With at most 10 games stored, `rows` EQUALS the CLI query path output
//!   (`read_guild_aggregates` + `render_game_activity_report`) as parsed JSON:
//!   same order (total desc, in-voice desc, title asc, untitled first among
//!   full ties), same numbers, `"(untitled)"` placeholder.
//! - At most 10 rows render; more stored rows set `truncated: true` plus a
//!   prose note naming the remainder (accepted phrasings: "N more", "+N",
//!   "N remaining", "top 10", "of TOTAL").
//! - Empty guild is success: empty `rows`, `truncated: false`, and the CLI
//!   hint text `no game activity recorded for this guild`.
//! - Hostile game names render inert: no raw mass mention anywhere in the
//!   reply, no raw mention pills/timestamps (`<@`, `<#`, `<:`, `<a:`, `<t:`,
//!   `</`, `<id:`), no newlines inside game cells, JSON still parses, numbers
//!   keep CLI parity. Plain and Unicode names render byte-identical.
//! - Store outage renders retry guidance ("try again"), never the empty hint,
//!   never a JSON block, never fabricated numbers; the missing table stays
//!   missing (no migrations, no self-create).
//! - Non-staff refusal names the Manage Server/Guild permission, carries no
//!   game data and no JSON block, and differs from the unknown-command reply.
//! - Foreign-guild interactions never reach the handler (router fence): zero
//!   REST requests, zero reads.
//!
//! Every scenario asserts zero mutations (aggregates table byte-identical,
//! audit count unchanged, REST shape only) and no per-member leakage (exact
//! row key sets, no invoker snowflake, no `<@` pills in content).
//!
//! Real gateway dispatch -> router -> store -> renderer -> loopback REST, on a
//! disposable test database only (the `custom_command_list_pages_acceptance`
//! pattern). No live Discord, no staging, no production data.

#![cfg(test)]

use super::*;
use std::collections::HashMap;
use two_bot_core::game_activity::render_game_activity_report;
use two_bot_core::game_activity_store::read_guild_aggregates;
use two_bot_core::message_safety::contains_mass_mention;
use two_bot_testsupport::TestDatabase;

const MIN_MS: i64 = 60_000;
/// Distinctive invoker id: large enough that no seeded number contains it as
/// a substring, so the no-leakage assertion cannot false-pass.
const MEMBER_ID: u64 = 887_766_554_433_221_100;
const FOREIGN_GUILD: u64 = 9999;
const FOREIGN_GUILD_S: &str = "9999";
const COMMAND: &str = "game-mix";
const EMPTY_HINT: &str = "no game activity recorded for this guild";
const UNTITLED: &str = "(untitled)";
/// Most per-game rows the Discord reply renders (the `/leaderboard` top-10
/// precedent; the CLI cap stays 100).
const DISCORD_TOP_N: usize = 10;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database; no application credential fallback")
}

/// Seed one aggregate row in stored form (`""` is the untitled key).
async fn seed(pool: &PgPool, guild: &str, game: &str, total: i64, voice: i64, sessions: i64) {
    sqlx::query(
        "INSERT INTO game_activity_guild_aggregates
             (guild_id, game, total_play_ms, in_voice_ms, session_count)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(guild)
    .bind(game)
    .bind(total)
    .bind(voice)
    .bind(sessions)
    .execute(pool)
    .await
    .expect("seed real aggregate store");
}

/// Whole-table witness plus audit count. A pure read leaves both identical.
async fn state(pool: &PgPool) -> (String, i64) {
    let aggregates = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.guild_id, t.game), '[]'::jsonb)::text
           FROM game_activity_guild_aggregates t",
    )
    .fetch_one(pool)
    .await
    .expect("snapshot every aggregate column, including updated_at");
    let audits = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM automation_audit_log")
        .fetch_one(pool)
        .await
        .expect("read-only audit count");
    (aggregates, audits)
}

/// The CLI query path as oracle: the store reader the bin uses plus the core
/// renderer. Returns the parsed report JSON.
async fn cli_report(pool: &PgPool, guild: &str) -> serde_json::Value {
    let rows = read_guild_aggregates(pool, guild)
        .await
        .expect("cli query path reads the seeded guild");
    let report = render_game_activity_report(guild, &rows);
    serde_json::from_str(&report).expect("cli report is one JSON object")
}

/// Dispatch one interaction through the real gateway path with every feature
/// gate off: `/game-mix` is always published, so gates must not matter.
async fn dispatch(pool: PgPool, interaction: Interaction) -> Vec<RestRequest> {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(false, false)),
        GUILD,
        false,
    );
    let guard = runtime.dispatch_guard();
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
            GUILD,
            &HashMap::from([("TWO_AUTOMATIONS".to_owned(), "0".to_owned())]),
            false,
        )
        .expect("test gateway configuration"),
    );
    let _ = runtime.dispatch(&interaction_event(interaction));
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "game-mix gateway work settles",
    )
    .await;
    let requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    requests
}

fn staff(permissions: Permissions) -> Interaction {
    from_member(
        slash(COMMAND, Some(CHANNEL), Vec::new()),
        MEMBER_ID,
        permissions,
    )
}

/// The one ephemeral reply: exactly one `/callback`, ephemeral flags, no
/// parsed mentions, within the Discord content budget. Returns the content.
fn ephemeral_content(requests: &[RestRequest]) -> String {
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        1,
        "one initial acknowledgement: {requests:?}"
    );
    assert_eq!(callbacks[0].method, "POST");
    let initial: serde_json::Value =
        serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(initial["data"]["flags"], 64, "every outcome is ephemeral");
    let body = if initial["type"] == 5 {
        assert_eq!(
            requests.len(),
            2,
            "defer + completion, no mutation or second reply"
        );
        assert!(requests[0].path.ends_with("/callback"), "defer comes first");
        assert_eq!(requests[1].method, "PATCH");
        assert!(requests[1].path.ends_with("/messages/@original"));
        serde_json::from_slice::<serde_json::Value>(&requests[1].body).expect("completion JSON")
    } else {
        assert_eq!(initial["type"], 4);
        assert_eq!(requests.len(), 1, "immediate reply is the only response");
        initial["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    let content = body["content"].as_str().expect("human-readable reply");
    assert!(
        content.encode_utf16().count() <= 2000,
        "entire response budget"
    );
    content.to_owned()
}

/// Extract the single `json` fenced block. Triple-backtick game names are
/// out of contract (they cannot survive a fenced block); the hostile suite
/// pins every other injection shape instead.
fn json_block(content: &str) -> serde_json::Value {
    let opens: Vec<_> = content.match_indices("```json").collect();
    assert_eq!(
        opens.len(),
        1,
        "exactly one machine-readable block: {content}"
    );
    let rest = &content[opens[0].0 + "```json".len()..];
    let end = rest.find("```").expect("closing fence");
    serde_json::from_str(rest[..end].trim()).expect("block is JSON")
}

fn assert_row_keys(rows: &[serde_json::Value]) {
    for row in rows {
        let mut keys: Vec<&str> = row
            .as_object()
            .expect("row is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["game", "in_voice_ms", "session_count", "total_play_ms"],
            "no member/user/player key may exist: {row}"
        );
    }
}

fn assert_no_identity_leak(content: &str) {
    assert!(
        !content.contains(&MEMBER_ID.to_string()),
        "invoker snowflake never renders: {content}"
    );
    assert!(!content.contains("<@"), "no user pills anywhere: {content}");
}

/// Full leakage pin for data replies: no identity leak plus no
/// member-keyed field in the machine block. Refusal/outage prose uses only
/// [`assert_no_identity_leak`]: human guidance may legitimately contain
/// words like "user".
fn assert_no_member_leak(content: &str, block_text: &str) {
    assert_no_identity_leak(content);
    for key in ["member", "user", "player"] {
        assert!(
            !block_text.contains(key),
            "machine block carries no {key} field: {block_text}"
        );
    }
}

#[tokio::test]
async fn empty_guild_renders_valid_empty_shape_plus_hint() {
    let db = database().await;
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "empty read mutates nothing");
    let content = ephemeral_content(&requests);
    let block = json_block(&content);
    assert_eq!(block["guild_id"], GUILD_S);
    assert_eq!(block["rows"], serde_json::json!([]));
    assert_eq!(block["truncated"], false);
    assert_eq!(block["hint"], EMPTY_HINT, "CLI hint text, verbatim");
    assert_eq!(cli["rows"], serde_json::json!([]), "oracle agrees: empty");
    assert_eq!(cli["hint"], EMPTY_HINT);
    assert!(
        content.contains(EMPTY_HINT),
        "hint is human-visible, not JSON-only: {content}"
    );
    assert!(
        !content.to_lowercase().contains("try again"),
        "empty is success, not failure vocabulary: {content}"
    );
    assert_no_member_leak(&content, &block.to_string());
}

#[tokio::test]
async fn single_game_matches_cli_query_path_exactly() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "Solo Quest", 5 * MIN_MS, MIN_MS, 1).await;
    seed(db.pool(), FOREIGN_GUILD_S, "AwayGame", 99 * MIN_MS, 0, 9).await;
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "populated read mutates nothing");
    let content = ephemeral_content(&requests);
    let block = json_block(&content);
    assert_eq!(block["guild_id"], GUILD_S);
    assert_eq!(block["truncated"], false);
    assert!(
        block.get("hint").is_none(),
        "hint is an empty-report field only: {block}"
    );
    assert_eq!(
        block["rows"], cli["rows"],
        "byte-parity with the CLI query path"
    );
    assert_eq!(
        block["rows"],
        serde_json::json!([
            {"game": "Solo Quest", "total_play_ms": 5 * MIN_MS,
             "in_voice_ms": MIN_MS, "session_count": 1},
        ])
    );
    assert!(
        !content.contains("AwayGame") && !content.contains(FOREIGN_GUILD_S),
        "other guild never renders: {content}"
    );
    assert_row_keys(block["rows"].as_array().expect("rows array"));
    assert_no_member_leak(&content, &block.to_string());
}

#[tokio::test]
async fn multi_game_order_isolation_and_untitled_match_cli() {
    let db = database().await;
    // The CLI fixture shape: a 40-minute tie broken by in-voice overlap, plus
    // the untitled row. Seeds for the other guild must never appear.
    seed(db.pool(), GUILD_S, "Apex", 40 * MIN_MS, MIN_MS, 3).await;
    seed(db.pool(), GUILD_S, "", MIN_MS, 0, 1).await;
    seed(db.pool(), GUILD_S, "Zelda", 40 * MIN_MS, 2 * MIN_MS, 2).await;
    seed(db.pool(), FOREIGN_GUILD_S, "Apex", 50 * MIN_MS, 0, 5).await;
    seed(
        db.pool(),
        FOREIGN_GUILD_S,
        "OtherGuildOnlyGame",
        99 * MIN_MS,
        0,
        1,
    )
    .await;
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "multi-row read mutates nothing");
    let content = ephemeral_content(&requests);
    let block = json_block(&content);
    assert_eq!(
        block["rows"], cli["rows"],
        "order, numbers and placeholder equal the CLI path"
    );
    assert_eq!(
        block["rows"],
        serde_json::json!([
            {"game": "Zelda", "total_play_ms": 40 * MIN_MS,
             "in_voice_ms": 2 * MIN_MS, "session_count": 2},
            {"game": "Apex", "total_play_ms": 40 * MIN_MS,
             "in_voice_ms": MIN_MS, "session_count": 3},
            {"game": UNTITLED, "total_play_ms": MIN_MS,
             "in_voice_ms": 0, "session_count": 1},
        ])
    );
    assert!(!content.contains("OtherGuildOnlyGame"), "{content}");
    assert!(!content.contains(FOREIGN_GUILD_S), "{content}");
    assert!(
        !content.contains(&(99 * MIN_MS).to_string()),
        "other guild totals leaked: {content}"
    );
    assert_row_keys(block["rows"].as_array().expect("rows array"));
    assert_no_member_leak(&content, &block.to_string());
}

#[tokio::test]
async fn unicode_game_names_preserved_with_cli_parity() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "Zelda", 40 * MIN_MS, 2 * MIN_MS, 2).await;
    seed(
        db.pool(),
        GUILD_S,
        "Slay the Spire 🔥",
        30 * MIN_MS,
        MIN_MS,
        4,
    )
    .await;
    seed(db.pool(), GUILD_S, "ポケットモンスター", 20 * MIN_MS, 0, 1).await;
    // ZWNJ inside ordinary text is meaningful, not an obfuscation gap: the
    // neutralizer preserves it and so must this surface.
    seed(db.pool(), GUILD_S, "ZWNJ\u{200c}test", 10 * MIN_MS, 0, 1).await;
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "unicode read mutates nothing");
    let content = ephemeral_content(&requests);
    let block = json_block(&content);
    assert_eq!(
        block["rows"], cli["rows"],
        "Unicode byte-identical to the CLI path"
    );
    let names: Vec<&str> = block["rows"]
        .as_array()
        .expect("rows array")
        .iter()
        .map(|row| row["game"].as_str().expect("game string"))
        .collect();
    assert_eq!(
        names,
        [
            "Zelda",
            "Slay the Spire 🔥",
            "ポケットモンスター",
            "ZWNJ\u{200c}test"
        ]
    );
    assert_no_member_leak(&content, &block.to_string());
}

#[tokio::test]
async fn hostile_game_names_render_inert_without_losing_numbers() {
    let db = database().await;
    // Distinct totals keep render order deterministic, so numbers can be
    // checked positionally against the CLI oracle while names are sanitized.
    let hostile = [
        "@everyone",
        "@here",
        "@\u{200b}everyone",
        "<@123456789012345678>",
        "<#987654321098765432>",
        "line\nbreak",
        "quote\"back\\slash",
        "**bold** [link](http://example.invalid)",
    ];
    for (index, name) in hostile.iter().enumerate() {
        let total = (40 - index as i64) * MIN_MS;
        seed(db.pool(), GUILD_S, name, total, MIN_MS, 1).await;
    }
    seed(db.pool(), GUILD_S, "Control", MIN_MS, 0, 1).await;
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "hostile read mutates nothing");
    let content = ephemeral_content(&requests);
    let block = json_block(&content);
    let rows = block["rows"].as_array().expect("rows array").clone();
    let cli_rows = cli["rows"].as_array().expect("cli rows array").clone();
    assert_eq!(rows.len(), hostile.len() + 1);
    assert_eq!(rows.len(), cli_rows.len());
    for (position, (row, oracle)) in rows.iter().zip(cli_rows.iter()).enumerate() {
        assert_eq!(
            (
                row["total_play_ms"].clone(),
                row["in_voice_ms"].clone(),
                row["session_count"].clone()
            ),
            (
                oracle["total_play_ms"].clone(),
                oracle["in_voice_ms"].clone(),
                oracle["session_count"].clone()
            ),
            "numbers keep CLI parity at position {position}"
        );
        let game = row["game"].as_str().expect("game string");
        assert!(
            !game.contains(['\n', '\r']),
            "game cells stay single-line: {game:?}"
        );
    }
    assert_row_keys(&rows);
    // The ping risk is gone but the names are still recognizable, not dropped.
    assert!(
        !contains_mass_mention(&content),
        "no raw mass mention may reach the wire: {content}"
    );
    for pill in ["<@", "<#", "<:", "<a:", "<t:", "</", "<id:"] {
        assert!(
            !content.contains(pill),
            "raw {pill} pill renders literally, never as a pill: {content}"
        );
    }
    assert!(
        content.contains("@\u{200b}everyone"),
        "neutralized mention stays readable: {content}"
    );
    assert!(
        content.contains("Control"),
        "plain names pass through byte-identical: {content}"
    );
    assert_no_member_leak(&content, &block.to_string());
}

#[tokio::test]
async fn exactly_ten_games_show_all_with_no_truncation_flag() {
    let db = database().await;
    for index in 0..DISCORD_TOP_N {
        seed(
            db.pool(),
            GUILD_S,
            &format!("Game-{index:02}"),
            (DISCORD_TOP_N - index) as i64 * MIN_MS,
            MIN_MS,
            1,
        )
        .await;
    }
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "boundary read mutates nothing");
    let content = ephemeral_content(&requests);
    let block = json_block(&content);
    assert_eq!(
        block["rows"], cli["rows"],
        "at the boundary every row keeps CLI parity"
    );
    assert_eq!(
        block["rows"].as_array().expect("rows array").len(),
        DISCORD_TOP_N
    );
    assert_eq!(block["truncated"], false);
    assert_no_member_leak(&content, &block.to_string());
}

#[tokio::test]
async fn twelve_games_truncate_to_ten_with_remaining_note() {
    let db = database().await;
    for index in 0..DISCORD_TOP_N + 2 {
        seed(
            db.pool(),
            GUILD_S,
            &format!("Game-{index:02}"),
            (DISCORD_TOP_N + 2 - index) as i64 * MIN_MS,
            MIN_MS,
            1,
        )
        .await;
    }
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "truncated read mutates nothing");
    let content = ephemeral_content(&requests);
    let block = json_block(&content);
    let rows = block["rows"].as_array().expect("rows array").clone();
    let cli_rows = cli["rows"].as_array().expect("cli rows array").clone();
    assert_eq!(cli_rows.len(), DISCORD_TOP_N + 2, "oracle sees all twelve");
    assert_eq!(rows.len(), DISCORD_TOP_N, "Discord shows the top ten");
    assert_eq!(
        rows,
        cli_rows[..DISCORD_TOP_N].to_vec(),
        "shown rows keep CLI order"
    );
    assert_eq!(block["truncated"], true);
    let lower = content.to_lowercase();
    assert!(
        lower.contains("more")
            || lower.contains("truncat")
            || lower.contains("remaining")
            || lower.contains("top 10")
            || lower.contains("of 12"),
        "a truncation note names the remainder: {content}"
    );
    assert_no_member_leak(&content, &block.to_string());
}

#[tokio::test]
async fn store_outage_renders_retry_guidance_without_fabrication() {
    let db = database().await;
    let audits_before: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("read-only audit count");
    // Surgical outage: only the aggregate table the report reads goes away.
    sqlx::query("DROP TABLE game_activity_guild_aggregates")
        .execute(db.pool())
        .await
        .expect("drop only the aggregate table");
    let requests = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let audits_after: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("read-only audit count");
    let restored: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('game_activity_guild_aggregates')::text")
            .fetch_one(db.pool())
            .await
            .expect("regclass probe");
    db.close().await.expect("drop only owned fixture");

    assert_eq!(
        audits_before, audits_after,
        "failed read writes no audit rows"
    );
    assert_eq!(restored, None, "no migrations, no self-create on failure");
    let content = ephemeral_content(&requests);
    assert!(
        content.to_lowercase().contains("try again"),
        "outage renders retry guidance: {content}"
    );
    assert!(
        !content.contains(EMPTY_HINT),
        "outage is never the empty shape: {content}"
    );
    assert!(
        !content.contains("```json"),
        "outage fabricates no report block: {content}"
    );
    assert_no_identity_leak(&content);
}

#[tokio::test]
async fn non_staff_refused_staff_and_admins_admitted() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        "RefusalProbe",
        777 * MIN_MS,
        7 * MIN_MS,
        7,
    )
    .await;
    let before = state(db.pool()).await;
    let denied_empty = dispatch(db.pool().clone(), staff(Permissions::empty())).await;
    let denied_channel = dispatch(db.pool().clone(), staff(Permissions::MANAGE_CHANNELS)).await;
    let allowed_manager = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let allowed_admin = dispatch(db.pool().clone(), staff(Permissions::ADMINISTRATOR)).await;
    let after = state(db.pool()).await;
    let cli = cli_report(db.pool(), GUILD_S).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "gate checks mutate nothing");
    for (label, requests) in [
        ("empty permissions", denied_empty),
        ("manage-channels-only", denied_channel),
    ] {
        let content = ephemeral_content(&requests);
        let lower = content.to_lowercase();
        assert!(
            lower.contains("manage server") || lower.contains("manage guild"),
            "{label} refusal names the staff permission: {content}"
        );
        assert!(
            !content.contains("RefusalProbe"),
            "{label} refusal leaks no game data: {content}"
        );
        assert!(
            !content.contains(&(777 * MIN_MS).to_string()),
            "{label} refusal leaks no numbers: {content}"
        );
        assert!(
            !content.contains("```json"),
            "{label} refusal carries no report block: {content}"
        );
        assert_no_identity_leak(&content);
    }
    for (label, requests) in [
        ("manage-guild", allowed_manager),
        ("administrator", allowed_admin),
    ] {
        let content = ephemeral_content(&requests);
        let block = json_block(&content);
        assert_eq!(block["rows"], cli["rows"], "{label} sees full CLI parity");
        assert_no_member_leak(&content, &block.to_string());
    }
}

#[tokio::test]
async fn foreign_guild_interaction_is_fenced_with_zero_reads() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "HomeGame", 5 * MIN_MS, MIN_MS, 1).await;
    seed(db.pool(), FOREIGN_GUILD_S, "AwayGame", 9 * MIN_MS, 0, 1).await;
    let before = state(db.pool()).await;
    // Positive control: the same staff member in the configured guild sees
    // the home rows. This is what fails RED before the command exists.
    let home = dispatch(db.pool().clone(), staff(Permissions::MANAGE_GUILD)).await;
    let mut foreign = staff(Permissions::MANAGE_GUILD);
    set_guild(&mut foreign, Some(Id::new(FOREIGN_GUILD)));
    let fenced = dispatch(db.pool().clone(), foreign).await;
    let after = state(db.pool()).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(before, after, "cross-guild probe mutates nothing");
    let content = ephemeral_content(&home);
    assert!(
        content.contains("HomeGame"),
        "in-guild control sees home rows: {content}"
    );
    assert!(
        !content.contains("AwayGame"),
        "in-guild control never sees away rows: {content}"
    );
    assert!(
        fenced.is_empty(),
        "foreign guild is fenced before any reply: {fenced:?}"
    );
}
