//! Protected Test Author extension for `/game-mix`: picker metadata, missing
//! authority, fenced JSON safety and the complete Discord content budget.
//!
//! Consume this module unchanged alongside `game_mix_acceptance_tests.rs`.
//! Return test defects to the Test Author. No implementation lives here.
//! Names may be shortened and fewer than ten rows may fit; no arbitrary name
//! length, escaping policy or shortening algorithm is prescribed. Shown rows
//! must remain a nonempty top-N prefix with exact stored integer values.
//!
//! Breaks caught: hiding the picker behind a gate, publishing an unsafe DM or
//! permission default, trusting absent member authority, raw fence/mention
//! interpolation, scalar-count budgeting, truncating serialized JSON, rounding
//! large counts, changing shown row order, and logging member identity.

#![cfg(test)]

use super::*;
use std::collections::HashMap;
use two_bot_core::message_safety::contains_mass_mention;
use two_bot_testsupport::TestDatabase;

const COMMAND: &str = "game-mix";
const MEMBER_ID: u64 = 887_766_554_433_221_101;
const MEMBER_NAME: &str = "GameMixPrivateIdentityProbe";
const MEMBER_NICK: &str = "GameMixPrivateNicknameProbe";
const HUGE_PLAY: i64 = 9_223_372_036_854_775_807;
const HUGE_VOICE: i64 = 9_007_199_254_740_993;
const HUGE_SESSIONS: i64 = 9_007_199_254_740_995;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database; no application credential fallback")
}

async fn seed(pool: &PgPool, name: &str, rank: i64) {
    sqlx::query(
        "INSERT INTO game_activity_guild_aggregates
             (guild_id, game, total_play_ms, in_voice_ms, session_count)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(GUILD_S)
    .bind(name)
    .bind(HUGE_PLAY - rank)
    .bind(HUGE_VOICE - rank)
    .bind(HUGE_SESSIONS - rank)
    .execute(pool)
    .await
    .expect("seed stored TEXT titles and BIGINT counters");
}

async fn state(pool: &PgPool) -> (String, i64) {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.guild_id, t.game), '[]'::jsonb)::text
           FROM game_activity_guild_aggregates t",
    )
    .fetch_one(pool)
    .await
    .expect("snapshot all aggregate columns");
    let audits = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM automation_audit_log")
        .fetch_one(pool)
        .await
        .expect("snapshot audit count");
    (rows, audits)
}

fn staff() -> Interaction {
    let mut interaction = from_member(
        slash(COMMAND, Some(CHANNEL), Vec::new()),
        MEMBER_ID,
        Permissions::MANAGE_GUILD,
    );
    // Do not confuse an interaction/correlation id or a synthetic callback
    // token with a member identity when checking diagnostics.
    interaction.id = Id::new(774_411);
    interaction.token = "game-mix-content-test-token".to_owned();
    let member = interaction.member.as_mut().expect("guild member fixture");
    member.user.as_mut().expect("fixture user").name = MEMBER_NAME.to_owned();
    member.nick = Some(MEMBER_NICK.to_owned());
    interaction.user = Some(user(MEMBER_ID, false));
    interaction
}

async fn dispatch(pool: PgPool, interaction: Interaction) -> Vec<RestRequest> {
    dispatch_for_guild(pool, interaction, GUILD).await
}

async fn dispatch_for_guild(
    pool: PgPool,
    interaction: Interaction,
    guild: u64,
) -> Vec<RestRequest> {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(RouterGates {
            configured_guild: Some(guild),
            ..gates(false, false)
        }),
        guild,
        false,
    );
    let guard = runtime.dispatch_guard();
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
            guild,
            &HashMap::from([("TWO_AUTOMATIONS".to_owned(), "0".to_owned())]),
            false,
        )
        .expect("test gateway configuration"),
    );
    let _ = runtime.dispatch(&interaction_event(interaction));
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "game-mix content gateway work settles",
    )
    .await;
    let requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    requests
}

fn content(requests: &[RestRequest]) -> String {
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "one initial response: {requests:?}");
    assert_eq!(callbacks[0].method, "POST");
    let initial: serde_json::Value =
        serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(initial["data"]["flags"], 64, "private acknowledgement");
    let body = if initial["type"] == 5 {
        assert_eq!(requests.len(), 2, "only defer plus original-message edit");
        assert!(requests[0].path.ends_with("/callback"));
        assert_eq!(requests[1].method, "PATCH");
        assert!(requests[1].path.ends_with("/messages/@original"));
        serde_json::from_slice::<serde_json::Value>(&requests[1].body).expect("completion JSON")
    } else {
        assert_eq!(initial["type"], 4);
        assert_eq!(requests.len(), 1, "no channel post, DM or second response");
        initial["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    let text = body["content"].as_str().expect("reply content");
    assert!(text.encode_utf16().count() <= 2000, "whole content budget");
    assert!(!contains_mass_mention(text), "no mass mentions on the wire");
    for pill in ["<@", "<#", "<:", "<a:", "<t:", "</", "<id:"] {
        assert!(!text.contains(pill), "no raw Discord pill {pill}");
    }
    assert_private(text);
    text.to_owned()
}

fn assert_private(text: &str) {
    for identity in [
        MEMBER_ID.to_string(),
        MEMBER_NAME.to_owned(),
        MEMBER_NICK.to_owned(),
    ] {
        assert!(!text.contains(&identity), "member identity leaked");
    }
}

fn report(text: &str) -> (serde_json::Value, String) {
    let fences: Vec<_> = text.match_indices("```").collect();
    assert_eq!(
        fences.len(),
        2,
        "exactly one fenced block, no title breakout"
    );
    let start = fences[0].0;
    assert!(text[start..].starts_with("```json\n"), "one JSON block");
    let end = fences[1].0;
    let value: serde_json::Value = serde_json::from_str(&text[start + 8..end])
        .expect("whole parsable JSON, never a clipped serialization");
    assert_eq!(value["guild_id"], GUILD_S);
    assert!(value["truncated"].is_boolean());
    let rows = value["rows"].as_array().expect("report rows");
    assert!(!rows.is_empty(), "populated guild must show a real row");
    assert!(rows.len() <= 10, "Discord top-N bound");
    for (rank, row) in rows.iter().enumerate() {
        let mut keys: Vec<_> = row
            .as_object()
            .expect("row object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["game", "in_voice_ms", "session_count", "total_play_ms"]
        );
        let name = row["game"].as_str().expect("game string");
        assert!(!name.is_empty(), "shortening must leave a readable label");
        assert!(!name.contains(['\n', '\r']), "single-line game label");
        // Hand-derived from the fixture's distinct descending totals, not
        // the renderer/CLI: no rounding, clamping, stringifying or reordering.
        assert_eq!(row["total_play_ms"].as_i64(), Some(HUGE_PLAY - rank as i64));
        assert_eq!(row["in_voice_ms"].as_i64(), Some(HUGE_VOICE - rank as i64));
        assert_eq!(
            row["session_count"].as_i64(),
            Some(HUGE_SESSIONS - rank as i64)
        );
    }
    (value, format!("{}{}", &text[..start], &text[end + 3..]))
}

fn assert_budget_notice(prose: &str) {
    let lower = prose.to_lowercase();
    assert!(
        [
            "shorten",
            "truncat",
            "omit",
            "remaining",
            "more",
            "limit",
            "top 10",
            "top ten"
        ]
        .iter()
        .any(|word| lower.contains(word)),
        "shortened titles or omitted rows need an explicit human-visible notice: {prose}"
    );
}

fn assert_picker(gates: RouterGates) {
    let definitions = router_with_commands(gates)
        .publish_set(&[])
        .expect("publish set");
    let definition = definitions
        .iter()
        .find(|command| command.name == COMMAND)
        .expect("game-mix core definition publishes");
    assert!(!definition.dm_permission, "guild-only core definition");
    let published = two_bot_discord::publish_commands(&definitions);
    let matches: Vec<_> = published
        .iter()
        .filter(|command| command.name == COMMAND)
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one published game-mix picker entry"
    );
    let wire = serde_json::to_value(matches[0]).expect("Discord registration wire shape");
    assert_eq!(wire["type"], 1, "chat input command");
    assert_eq!(
        wire["default_member_permissions"], "32",
        "Manage Guild default"
    );
    // Publication uses the guild endpoint, which supplies the guild-only
    // scope. Twilight omits this deprecated field; runtime DM denial is
    // exercised separately below, not inferred from an absent wire field.
    assert!(
        wire.get("dm_permission").is_none(),
        "deprecated DM field stays unset for guild publication"
    );
    assert!(
        wire.get("options")
            .is_none_or(|options| options.as_array().is_some_and(Vec::is_empty)),
        "no options: one guild-wide aggregate, no member selector"
    );
}

#[test]
fn picker_is_published_with_all_gates_off_and_safe_metadata() {
    assert_picker(gates(false, false));
}

#[test]
fn picker_stays_published_once_with_all_gates_on() {
    assert_picker(RouterGates {
        scorecard: true,
        moderation: true,
        voice: true,
        voice_assistant: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
        ..gates(true, true)
    });
}

#[tokio::test]
async fn missing_member_or_permission_is_denied_through_gateway() {
    let db = database().await;
    seed(db.pool(), "PrivateAggregateProbe", 0).await;
    let before = state(db.pool()).await;
    let mut absent_member = staff();
    absent_member.member = None;
    let mut absent_permission = staff();
    absent_permission
        .member
        .as_mut()
        .expect("member")
        .permissions = None;
    let denied_member = dispatch(db.pool().clone(), absent_member).await;
    let denied_permission = dispatch(db.pool().clone(), absent_permission).await;
    let allowed = dispatch(db.pool().clone(), staff()).await;
    let after = state(db.pool()).await;
    db.close().await.expect("dispose only owned database");

    assert_eq!(before, after, "permission probes mutate nothing");
    for denied in [denied_member, denied_permission] {
        let reply = content(&denied);
        let lower = reply.to_lowercase();
        assert!(
            lower.contains("manage server") || lower.contains("manage guild"),
            "missing authority gets a staff-permission refusal: {reply}"
        );
        assert!(!reply.contains("PrivateAggregateProbe"));
        assert!(!reply.contains(&HUGE_PLAY.to_string()));
        assert!(!reply.contains("```"), "denial has no data report");
    }
    let (positive, _) = report(&content(&allowed));
    assert_eq!(positive["rows"][0]["game"], "PrivateAggregateProbe");
}

#[tokio::test]
async fn direct_message_gateway_is_silent_and_never_exposes_guild_data() {
    let db = database().await;
    seed(db.pool(), "GuildOnlyAggregateProbe", 0).await;
    let before = state(db.pool()).await;
    let mut dm = staff();
    set_guild(&mut dm, None);
    dm.member = None;
    dm.authorizing_integration_owners.guild = None;
    let requests = dispatch(db.pool().clone(), dm).await;
    let allowed = dispatch(db.pool().clone(), staff()).await;
    let after = state(db.pool()).await;
    db.close().await.expect("dispose only owned database");

    assert_eq!(before, after);
    assert!(
        requests.is_empty(),
        "configured-guild gateway fence rejects DMs"
    );
    let (positive, _) = report(&content(&allowed));
    assert_eq!(positive["rows"][0]["game"], "GuildOnlyAggregateProbe");
}

#[tokio::test]
async fn stored_backtick_fences_and_discord_markup_stay_in_one_inert_json_block() {
    let db = database().await;
    seed(db.pool(),
        "before```json\n{\"injected\":true}\n```after @everyone @here <@123456789012345678> <@&123456789012345678> <#123456789012345678> <:game:123456789012345678> <a:game:123456789012345678> <t:1234567890:R> </game:123456789012345678> <id:customize>",
        0).await;
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff()).await;
    let after = state(db.pool()).await;
    db.close().await.expect("dispose only owned database");

    assert_eq!(before, after);
    let (value, _) = report(&content(&requests));
    assert_eq!(value["rows"].as_array().expect("rows").len(), 1);
    assert!(
        value.get("injected").is_none(),
        "title never becomes report structure"
    );
}

#[tokio::test]
async fn stored_long_astral_unicode_title_respects_utf16_budget_and_reports_shortening() {
    let db = database().await;
    seed(
        db.pool(),
        &format!("LongUnicode-{}-end", "🧑🏽‍🚀ポケット".repeat(600)),
        0,
    )
    .await;
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff()).await;
    let after = state(db.pool()).await;
    db.close().await.expect("dispose only owned database");

    assert_eq!(before, after);
    let (value, prose) = report(&content(&requests));
    assert_eq!(value["rows"].as_array().expect("rows").len(), 1);
    assert_budget_notice(&prose);
}

#[tokio::test]
async fn escape_heavy_long_titles_and_bigints_keep_a_complete_numeric_top_prefix() {
    let db = database().await;
    // JSON escaping grows quotes/backslashes/control characters on the wire;
    // astral symbols cost two UTF-16 units. Twelve distinct titles also force
    // top-N truncation independently of the chosen shortening strategy.
    for rank in 0..12 {
        let name = format!(
            "Escape-{rank:02}-{}",
            "\"\\\t\r\n```🦀@everyone<@123456789012345678>".repeat(300)
        );
        seed(db.pool(), &name, rank).await;
    }
    let before = state(db.pool()).await;
    let requests = dispatch(db.pool().clone(), staff()).await;
    let after = state(db.pool()).await;
    db.close().await.expect("dispose only owned database");

    assert_eq!(before, after, "even oversized stored rows stay unchanged");
    let (value, prose) = report(&content(&requests));
    assert_eq!(value["truncated"], true, "not all twelve rows can be shown");
    assert_budget_notice(&prose);
}

#[tokio::test]
async fn configured_live_guild_gets_an_activation_refusal_even_when_store_is_unavailable() {
    let db = database().await;
    let live: u64 = two_bot_core::backup::guild_config::LIVE_GUILD_ID
        .parse()
        .expect("canonical live-guild id");
    // The configured id is synthetic routing input only. All storage remains
    // this owned disposable database; no live connection or data is used.
    sqlx::query("DROP TABLE game_activity_guild_aggregates")
        .execute(db.pool())
        .await
        .expect("unavailable aggregate store in disposable fixture");
    let audits_before = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("audit count");
    let mut interaction = staff();
    set_guild(&mut interaction, Some(Id::new(live)));
    interaction.authorizing_integration_owners.guild = Some(AnonymizableId::Id(Id::new(live)));
    let requests = dispatch_for_guild(db.pool().clone(), interaction, live).await;
    let audits_after = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("audit count");
    db.close().await.expect("dispose only owned database");

    assert_eq!(audits_before, audits_after);
    let reply = content(&requests);
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("live") || lower.contains("production"),
        "refusal explains the activation boundary: {reply}"
    );
    assert!(
        !lower.contains("try again"),
        "activation refusal, not a store error"
    );
    assert!(
        !reply.contains("```"),
        "no implicit opt-in to a live report"
    );
    assert!(!reply.contains(&HUGE_PLAY.to_string()), "no report numbers");
    // Missing-table failure cannot masquerade as the required activation
    // refusal. This does not claim to instrument/count SQL read attempts.
}

#[tokio::test]
async fn success_denial_and_store_errors_do_not_log_or_render_member_identity() {
    let db = database().await;
    seed(db.pool(), "LoggingControl", 0).await;
    let before = state(db.pool()).await;
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    tracing::debug!("game-mix content acceptance capture active");
    let successful = dispatch(db.pool().clone(), staff()).await;
    let mut non_staff = staff();
    non_staff.member.as_mut().expect("member").permissions = Some(Permissions::empty());
    let denied = dispatch(db.pool().clone(), non_staff).await;
    let after = state(db.pool()).await;
    // Only this disposable fixture is damaged; no shared schema is touched.
    sqlx::query("DROP TABLE game_activity_guild_aggregates")
        .execute(db.pool())
        .await
        .expect("simulate unavailable report store");
    let failed = dispatch(db.pool().clone(), staff()).await;
    let logged = capture.text().split_off(start);
    db.close().await.expect("dispose only owned database");

    assert_eq!(before, after, "success and refusal mutate nothing");
    assert!(
        logged.contains("game-mix content acceptance capture active"),
        "existing process-wide DEBUG capture is active"
    );
    assert_private(&logged);
    report(&content(&successful));
    let denial = content(&denied);
    assert!(!denial.contains("LoggingControl"));
    let error = content(&failed);
    assert!(
        error.to_lowercase().contains("try again"),
        "fixed retry guidance"
    );
    assert!(!error.contains("```"), "store failure fabricates no report");
}
