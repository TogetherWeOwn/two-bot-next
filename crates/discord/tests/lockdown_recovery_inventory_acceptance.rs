#![cfg(feature = "db")]

//! Protected saved-recovery inventory acceptance. Real runtime/router/store;
//! only Discord HTTP and the disposable database are external fixtures.
//!
//! Entry point: `/lockdown-records [page:1..advertised_max]`, success outcome
//! `lockdown_records`. Pages contain at most ten complete authorized channel
//! IDs, in ascending channel-ID order. These are saved records, NOT proof of
//! current overwrites. Inventory never fetches live overwrites.
//!
//! `with_recovery_visibility_cache(Arc<InMemoryCache>)` is the cache injection
//! seam. The fallback trait only lets the OLD runtime compile; its ignored
//! cache cannot satisfy the mixed-visibility tests. An inherent production
//! method wins normal Rust method resolution. Keep the trait and tests intact.
//! Cold guild/member/role/channel facts must not authorize a row. This seam
//! supplies gateway evidence, not a new permission or an actor-supplied allowlist.

#[allow(dead_code)]
mod common;

use std::sync::{Arc, Mutex};

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use tracing::{field::Visit, instrument::WithSubscriber, Subscriber};
use tracing_subscriber::{layer::Context, prelude::*, Layer};
use twilight_cache_inmemory::InMemoryCache;
use twilight_model::{
    application::interaction::Interaction,
    gateway::{event::Event, payload::incoming::GuildCreate},
};
use two_bot_core::{
    moderation::moderation_commands, ChannelModerationStore, InteractionRouter, RouterGates,
};
use two_bot_discord::{
    build_cache, register_channel_handlers, ActionExecutor, ChannelModerationRuntime, ChannelReply,
};
use two_bot_testsupport::TestDatabase;

const COMMAND: &str = "lockdown-records";
const GUILD: u64 = 111111111111111111;
const FOREIGN: u64 = 999999999999999999;
const CHANNEL: u64 = 222222222222222222;
const ACTOR: u64 = 333333333333333333;
const MANAGE: u64 = 16;
const VIEW: u64 = 1024;
const AT: &str = "2026-10-20T08:09:10.123456+00:00";

trait LegacyVisibilityCache: Sized {
    fn with_recovery_visibility_cache(self, cache: Arc<InMemoryCache>) -> Self;
}
impl LegacyVisibilityCache for ChannelModerationRuntime {
    fn with_recovery_visibility_cache(self, _cache: Arc<InMemoryCache>) -> Self {
        self
    }
}

#[derive(Clone, Default)]
struct Statements(Arc<Mutex<Vec<String>>>);
impl<S: Subscriber> Layer<S> for Statements {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        if !event.metadata().target().starts_with("sqlx::query") {
            return;
        }
        struct Visitor(Vec<String>);
        impl Visit for Visitor {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if matches!(field.name(), "db.statement" | "query") && !value.is_empty() {
                    self.0.push(value.to_owned());
                }
            }
            fn record_debug(
                &mut self,
                _field: &tracing::field::Field,
                _value: &dyn std::fmt::Debug,
            ) {
            }
        }
        let mut visitor = Visitor(Vec::new());
        event.record(&mut visitor);
        self.0.lock().expect("statement receipts").extend(visitor.0);
    }
}
impl Statements {
    fn moderation(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("statement receipts")
            .iter()
            .filter(|sql| sql.contains("moderation_"))
            .cloned()
            .collect()
    }
    fn assert_read_only(&self) {
        for sql in self.moderation() {
            let words = sql.to_ascii_uppercase();
            assert!(
                words.trim_start().starts_with("SELECT"),
                "non-read query: {sql}"
            );
            for verb in ["INSERT", "UPDATE", "DELETE", "CALL", "TRUNCATE"] {
                assert!(!words.split_whitespace().any(|word| word == verb), "{sql}");
            }
        }
    }
    fn assert_bounded_inventory(&self) {
        let statements = self.moderation();
        let inventory: Vec<_> = statements
            .iter()
            .filter(|sql| sql.contains("moderation_lockdowns"))
            .collect();
        assert!(
            !inventory.is_empty(),
            "must exercise the real persisted inventory query"
        );
        for sql in inventory {
            let words = sql.to_ascii_uppercase();
            assert!(
                words.contains("LIMIT") && words.contains("ORDER BY"),
                "inventory SQL must limit and order in the database: {sql}"
            );
            assert!(words.contains("GUILD_ID"), "guild-scoped query: {sql}");
        }
        self.assert_read_only();
    }
}

struct Database {
    fixture: TestDatabase,
    store: ChannelModerationStore,
}
impl Database {
    async fn open() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit disposable test database required");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable fixture");
        let store = ChannelModerationStore::from_pool(fixture.pool().clone());
        Self { fixture, store }
    }
    async fn seed(&self, guild: u64, channel: u64, at: &str, setter: Option<&str>, reason: &str) {
        sqlx::query("INSERT INTO moderation_lockdowns (channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason, locked_at) VALUES ($1,$2,'1049600','8192',true,$3,$4)")
            .bind(channel.to_string()).bind(guild.to_string()).bind(reason).bind(at)
            .execute(self.fixture.pool()).await.expect("saved recovery fixture");
        if let Some(setter) = setter {
            sqlx::query("INSERT INTO moderation_audit (request_id,guild_id,actor_id,action,channel_id,reason,outcome,idempotency_key,metadata_json,created_at) VALUES ($1,$2,$3,'moderation.lockdown',$4,$5,'locked_down',$1,'{}',$6::text::timestamptz)")
                .bind(format!("fixture-{channel}")).bind(guild.to_string()).bind(setter)
                .bind(channel.to_string()).bind(reason).bind(at)
                .execute(self.fixture.pool()).await.expect("matching saved setter fixture");
        }
    }
    async fn snapshot(&self) -> Vec<Value> {
        let mut rows = Vec::new();
        for table in [
            "moderation_lockdowns",
            "moderation_audit",
            "moderation_idempotency",
            "moderation_channel_executions",
        ] {
            let value: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(jsonb_agg(row ORDER BY row::text), '[]'::jsonb) FROM (SELECT to_jsonb(t) AS row FROM {table} t) snapshot"
            )))
            .fetch_one(self.fixture.pool()).await.expect("complete before/after table snapshot");
            rows.push(value);
        }
        rows
    }
    async fn close(self) {
        self.fixture
            .close()
            .await
            .expect("verify disposable teardown");
    }
}

fn router(enabled: bool) -> InteractionRouter {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD),
        moderation: enabled,
        scorecard: false,
        automations: false,
        announcements: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    register_channel_handlers(&mut router);
    router
}

fn interaction(name: &str, page: Option<i64>, permissions: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": "777777777777777777", "application_id": "444444444444444444", "type": 2,
        "guild_id": GUILD.to_string(),
        "channel": {"id": CHANNEL.to_string(), "type": 0, "permissions": (permissions | VIEW).to_string()},
        "data": {"id": "555555555555555555", "type": 1, "name": name,
            "options": page.map_or_else(|| json!([]), |n| json!([{"name":"page", "type":4, "value":n}]))},
        "member": {"user":{"id": ACTOR.to_string(), "username":"fixture-member", "discriminator":"0"},
            "roles":[], "flags":0, "deaf":false, "mute":false, "permissions": permissions.to_string()},
        "token":"synthetic-interaction-token", "version":1,
        "authorizing_integration_owners":{"0": GUILD.to_string()}, "entitlements":[]
    })).expect("complete synthetic interaction")
}

// Each supplied channel is independently visible, explicitly denied, or cold
// (omitted). Manage Channels is deliberately NOT Administrator or ViewChannel.
fn visibility(channels: &[(u64, bool)]) -> Arc<InMemoryCache> {
    let cache = Arc::new(build_cache());
    let mut guild: Value =
        serde_json::from_str(include_str!("../../bot/tests/fixtures/voice_guild.json"))
            .expect("existing complete guild fixture");
    guild["id"] = json!(GUILD.to_string());
    guild["owner_id"] = json!("888888888888888888");
    guild["roles"][0]["id"] = json!(GUILD.to_string());
    guild["roles"][0]["permissions"] = json!((MANAGE | VIEW).to_string());
    guild["channels"] = json!(channels
        .iter()
        .map(|(id, visible)| json!({
            "id":id.to_string(), "guild_id":GUILD.to_string(), "type":0, "name":"fixture-text",
            "permission_overwrites": if *visible { json!([]) } else {json!([{
                "id": ACTOR.to_string(), "type":1, "allow":"0", "deny":VIEW.to_string()
            }])}
        }))
        .collect::<Vec<_>>());
    guild["members"] = json!([{
        "user":{"id":ACTOR.to_string(), "username":"fixture-member", "discriminator":"0"},
        "roles":[], "deaf":false, "mute":false, "flags":0,
        "joined_at":"2026-10-01T00:00:00+00:00"
    }]);
    cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(
        serde_json::from_value(guild).expect("available guild fixture"),
    ))));
    cache
}

fn runtime(
    store: ChannelModerationStore,
    mock: &MockRest,
    cache: Arc<InMemoryCache>,
) -> ChannelModerationRuntime {
    ChannelModerationRuntime::new(
        store,
        ActionExecutor::with_proxy("synthetic-token".to_owned(), Some(mock.origin()))
            .expect("loopback executor"),
    )
    .with_recovery_visibility_cache(cache)
}

async fn execute(
    runtime: &ChannelModerationRuntime,
    router: &InteractionRouter,
    interaction: &Interaction,
) -> (ChannelReply, Statements) {
    let statements = Statements::default();
    let subscriber = tracing_subscriber::registry().with(statements.clone());
    let reply = runtime
        .execute(router, interaction)
        .with_subscriber(subscriber)
        .await
        .expect("read failures must become safe replies, not persistence errors")
        .expect("runtime must handle the private saved recovery command");
    statements.assert_read_only();
    assert!(
        !reply.replayed,
        "reads never consume the mutation replay ledger"
    );
    assert!(
        reply.text.encode_utf16().count() <= 2000,
        "UTF-16 response bound"
    );
    (reply, statements)
}

fn require(text: &str, words: &[&str]) {
    let lower = text.to_lowercase();
    for word in words {
        assert!(
            lower.contains(&word.to_lowercase()),
            "missing {word:?}: {text}"
        );
    }
}
fn max_page() -> i64 {
    moderation_commands()
        .iter()
        .find(|def| def.name == COMMAND)
        .and_then(|def| def.options.iter().find(|opt| opt.name == "page"))
        .and_then(|opt| opt.max_value)
        .expect("inventory advertises a finite page range")
}
fn success(reply: &ChannelReply) {
    assert_eq!(reply.outcome, "lockdown_records");
    require(&reply.text, &["saved", "recover", "channel-inspect"]);
    assert!(
        !reply.text.to_lowercase().contains("lockdown: active"),
        "saved != effective live lock"
    );
    for secret in [
        "prior_allow",
        "prior_deny",
        "recovery_generation",
        "1049600",
        "8192",
    ] {
        assert!(
            !reply.text.contains(secret),
            "recovery capabilities/masks are not inventory fields"
        );
    }
}

#[test]
fn inventory_is_owned_by_existing_read_runtime() {
    assert!(
        ChannelModerationRuntime::accepts(&interaction(COMMAND, None, MANAGE)),
        "missing inventory read dispatch"
    );
}

#[test]
fn control_current_channel_read_and_manual_mutations_remain_owned() {
    for name in ["channel-inspect", "purge-preview", "lockdown", "unlock"] {
        assert!(ChannelModerationRuntime::accepts(&interaction(
            name, None, MANAGE
        )));
    }
}

#[tokio::test]
async fn control_explicit_legacy_adapter_preserves_existing_runtime_ownership() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_unused")
        .expect("lazy pool");
    pool.close().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let baseline = ChannelModerationRuntime::new(
        ChannelModerationStore::from_pool(pool),
        ActionExecutor::with_proxy("synthetic-token".to_owned(), Some(mock.origin()))
            .expect("executor"),
    );
    let baseline = LegacyVisibilityCache::with_recovery_visibility_cache(
        baseline,
        visibility(&[(CHANNEL, true)]),
    );
    let (reply, statements) = execute(
        &baseline,
        &router(true),
        &interaction("channel-inspect", None, 0),
    )
    .await;
    assert_eq!(reply.outcome, "refused");
    assert!(statements.moderation().is_empty());
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn control_sql_observer_records_real_saved_metadata_reads() {
    let db = Database::open().await;
    db.seed(GUILD, CHANNEL, AT, None, "observer control").await;
    let statements = Statements::default();
    let subscriber = tracing_subscriber::registry().with(statements.clone());
    let record = db
        .store
        .get_lockdown(&CHANNEL.to_string())
        .with_subscriber(subscriber)
        .await
        .expect("real store read")
        .expect("fixture row");
    assert_eq!(record.reason, "observer control");
    assert!(
        !statements.moderation().is_empty(),
        "a silent observer cannot prove deny-before-read or SQL LIMIT"
    );
    statements.assert_read_only();
    db.close().await;
}

#[tokio::test]
async fn authorization_and_invalid_pages_deny_before_sensitive_reads_or_http() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_unused")
        .expect("lazy pool");
    pool.close().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(
        ChannelModerationStore::from_pool(pool),
        &mock,
        visibility(&[(CHANNEL, true)]),
    );
    for mode in [
        "disabled",
        "permission",
        "wrong_guild",
        "dm",
        "missing_member",
        "missing_permissions",
        "zero",
        "negative",
        "overflow",
        "extra_option",
        "wrong_type",
    ] {
        let mut request = interaction(COMMAND, None, MANAGE);
        match mode {
            "permission" => request = interaction(COMMAND, None, 8192),
            "wrong_guild" => request.guild_id = Some(twilight_model::id::Id::new(FOREIGN)),
            "dm" => request.guild_id = None,
            "missing_member" => request.member = None,
            "missing_permissions" => request.member.as_mut().expect("member").permissions = None,
            "zero" => request = interaction(COMMAND, Some(0), MANAGE),
            "negative" => request = interaction(COMMAND, Some(-1), MANAGE),
            "overflow" => request = interaction(COMMAND, Some(i64::MAX), MANAGE),
            "extra_option" | "wrong_type" => {
                let mut value = serde_json::to_value(request).expect("request");
                value["data"]["options"] = if mode == "extra_option" {
                    json!([{"name":"reason", "type":3, "value":"not a mutation"}])
                } else {
                    json!([{"name":"page", "type":3, "value":"1"}])
                };
                request = serde_json::from_value(value).expect("typed adversarial request");
            }
            _ => {}
        }
        let (reply, statements) = execute(&rt, &router(mode != "disabled"), &request).await;
        assert_eq!(reply.outcome, "refused", "{mode}: {}", reply.text);
        assert!(
            statements.moderation().is_empty(),
            "{mode}: deny before sensitive SELECT"
        );
        assert!(
            mock.requests().is_empty(),
            "{mode}: deny before Discord access"
        );
    }
}

#[tokio::test]
async fn mixed_visible_hidden_cold_and_foreign_rows_never_disclose_hidden_counts() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(
        db.store.clone(),
        &mock,
        visibility(&[(CHANNEL, true), (CHANNEL + 1, false)]),
    );
    db.seed(
        GUILD,
        CHANNEL,
        AT,
        Some("333333333333333333"),
        "visible reason",
    )
    .await;
    let (before_hidden, receipts) =
        execute(&rt, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    success(&before_hidden);
    assert!(before_hidden.text.contains(&CHANNEL.to_string()));
    assert!(before_hidden.text.contains("visible reason"));
    assert!(before_hidden.text.contains(&ACTOR.to_string()));
    receipts.assert_bounded_inventory();
    // Hidden/cold records sort BEFORE the visible row too: a pre-authorization
    // LIMIT/offset must not hide a valid row or expose hidden-page counts.
    for n in 1..=40 {
        db.seed(GUILD, CHANNEL - n, AT, None, "leading-cold-sentinel")
            .await;
    }
    for (guild, channel, reason) in [
        (GUILD, CHANNEL + 1, "hidden-sentinel"),
        (GUILD, CHANNEL + 2, "cold-sentinel"),
        (FOREIGN, CHANNEL + 3, "foreign-sentinel"),
    ] {
        db.seed(guild, channel, AT, Some("666666666666666666"), reason)
            .await;
    }
    let snapshot = db.snapshot().await;
    let (after_hidden, _) = execute(&rt, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    assert_eq!(
        after_hidden, before_hidden,
        "hidden rows must not change totals, continuation, or text"
    );
    for needle in [
        "hidden-sentinel",
        "cold-sentinel",
        "foreign-sentinel",
        "666666666666666666",
    ] {
        assert!(!after_hidden.text.contains(needle));
    }
    for id in CHANNEL + 1..=CHANNEL + 3 {
        assert!(!after_hidden.text.contains(&id.to_string()));
    }
    assert_eq!(
        db.snapshot().await,
        snapshot,
        "no cleanup, claims, lane release, audit-as-applied or recovery write"
    );
    assert!(
        mock.requests().is_empty(),
        "inventory must not fetch live overwrites"
    );
    db.close().await;
}

#[tokio::test]
async fn missing_member_or_role_cache_facts_are_not_permission_grants() {
    let db = Database::open().await;
    db.seed(GUILD, CHANNEL, AT, None, "cold-authority-sentinel")
        .await;
    let snapshot = db.snapshot().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    for missing in ["member", "everyone_role"] {
        let cache = visibility(&[(CHANNEL, true)]);
        let event =
            if missing == "member" {
                Event::MemberRemove(serde_json::from_value(json!({
                "guild_id":GUILD.to_string(),
                "user":{"id":ACTOR.to_string(),"username":"fixture-member","discriminator":"0"}
            })).expect("member eviction"))
            } else {
                Event::RoleDelete(
                    serde_json::from_value(json!({
                        "guild_id":GUILD.to_string(), "role_id":GUILD.to_string()
                    }))
                    .expect("role eviction"),
                )
            };
        cache.update(&event);
        let rt = runtime(db.store.clone(), &mock, cache);
        let (reply, receipts) =
            execute(&rt, &router(true), &interaction(COMMAND, None, MANAGE)).await;
        assert_eq!(reply.outcome, "refused", "missing {missing}");
        require(&reply.text, &["unavailable", "try again"]);
        assert!(!reply.text.contains("cold-authority-sentinel"));
        assert!(!reply.text.contains(&CHANNEL.to_string()));
        assert!(
            receipts.moderation().is_empty(),
            "resolve actor authority before sensitive reads"
        );
    }
    assert!(mock.requests().is_empty());
    assert_eq!(db.snapshot().await, snapshot);
    db.close().await;
}

#[tokio::test]
async fn pages_are_stateless_deterministic_complete_and_utf16_bounded() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let ids: Vec<_> = (0..31).map(|n| CHANNEL + n).collect();
    let rt = runtime(
        db.store.clone(),
        &mock,
        visibility(&ids.iter().map(|id| (*id, true)).collect::<Vec<_>>()),
    );
    // Reverse insertion, equal timestamps and astral text defeat insertion order,
    // time-only ordering, scalar-count bounds and tail-truncated channel IDs.
    let reason = format!("@everyone <@555555555555555555> <@&666666666666666666> <#777777777777777777> https://example.invalid/ ` ```\n{}", "🦀".repeat(900));
    for id in ids.iter().rev() {
        db.seed(GUILD, *id, AT, None, &reason).await;
    }
    let snapshot = db.snapshot().await;
    let mut seen = Vec::new();
    let mut first = None;
    let mut ended = false;
    for page in 1..=32 {
        let (reply, receipts) = execute(
            &rt,
            &router(true),
            &interaction(COMMAND, Some(page), MANAGE),
        )
        .await;
        let on_page: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| reply.text.contains(&id.to_string()))
            .collect();
        if on_page.is_empty() {
            require(&reply.text, &["page"]);
            assert!(
                reply.text.to_lowercase().contains("range")
                    || reply.text.to_lowercase().contains("no records on"),
                "explicit out-of-range, not empty guild: {}",
                reply.text
            );
            ended = true;
            break;
        }
        success(&reply);
        assert!(on_page.len() <= 10, "bounded rows even with a short reason");
        let positions: Vec<_> = on_page
            .iter()
            .map(|id| reply.text.find(&id.to_string()).expect("complete ID"))
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "stable ID order"
        );
        assert!(
            on_page.iter().all(|id| !seen.contains(id)),
            "stateless pages must not repeat rows"
        );
        seen.extend(on_page);
        receipts.assert_bounded_inventory();
        if page == 1 {
            first = Some(reply);
        }
    }
    assert!(ended, "finite pagination");
    assert_eq!(
        seen, ids,
        "all visible records reachable, none silently lost to response truncation"
    );
    // New runtime instance: page state must not live in a process-local cursor.
    let restart = runtime(
        db.store.clone(),
        &mock,
        visibility(&ids.iter().map(|id| (*id, true)).collect::<Vec<_>>()),
    );
    let (again, _) = execute(&restart, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    assert_eq!(Some(again), first);
    assert_eq!(db.snapshot().await, snapshot);
    assert!(mock.requests().is_empty());
    db.close().await;
}

#[tokio::test]
async fn full_saved_instants_agree_between_inventory_and_current_channel_inspect() {
    let db = Database::open().await;
    let mock = MockRest::with_responder(|request| {
        if request.method == "GET" && request.path == format!("/api/v10/channels/{CHANNEL}") {
            ScriptedResponse::json(200,json!({"id":CHANNEL.to_string(),"guild_id":GUILD.to_string(),"type":0,"rate_limit_per_user":0,"permission_overwrites":[]}))
        } else { ScriptedResponse::status(500) }
    }).await;
    let rt = runtime(
        db.store.clone(),
        &mock,
        visibility(&[(CHANNEL, true), (CHANNEL + 1, true)]),
    );
    db.seed(
        GUILD,
        CHANNEL,
        "2026-10-20T10:09:10.123456+02:00",
        None,
        "first instant",
    )
    .await;
    db.seed(
        GUILD,
        CHANNEL + 1,
        "2026-10-20T10:10:11.654321+02:00",
        None,
        "second instant",
    )
    .await;
    let (inventory, _) = execute(&rt, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    success(&inventory);
    let (inspect, _) = execute(
        &rt,
        &router(true),
        &interaction("channel-inspect", None, MANAGE),
    )
    .await;
    assert_eq!(inspect.outcome, "channel_inspect");
    require(&inspect.text, &["saved", "unknown"]);
    // Accept equivalent RFC3339 UTC/offset displays or full Discord timestamps;
    // date-only and minute-only formatting cannot satisfy these literal instants.
    for (candidates, other) in [
        (
            [
                "2026-10-20T10:09:10.123456+02:00",
                "2026-10-20T08:09:10.123456Z",
                "2026-10-20T08:09:10.123456+00:00",
                "<t:1792483750:",
            ],
            false,
        ),
        (
            [
                "2026-10-20T10:10:11.654321+02:00",
                "2026-10-20T08:10:11.654321Z",
                "2026-10-20T08:10:11.654321+00:00",
                "<t:1792483811:",
            ],
            true,
        ),
    ] {
        let token = candidates
            .iter()
            .find(|token| inventory.text.contains(**token))
            .expect("inventory retains full saved instant, not just date");
        if !other {
            assert!(
                inspect.text.contains(*token),
                "shared saved-time formatting: {}",
                inspect.text
            );
        }
    }
    assert!(mock
        .requests()
        .iter()
        .all(|request| request.method == "GET"
            && request.path == format!("/api/v10/channels/{CHANNEL}")));
    db.close().await;
}

#[tokio::test]
async fn malformed_legacy_provenance_is_unknown_and_never_a_fabricated_setter() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(
        db.store.clone(),
        &mock,
        visibility(&[(CHANNEL, true), (CHANNEL + 1, true)]),
    );
    db.seed(GUILD, CHANNEL, "legacy-not-a-time", None, "legacy reason")
        .await;
    db.seed(
        GUILD,
        CHANNEL + 1,
        AT,
        Some("000333333333333333333"),
        "bad setter reason",
    )
    .await;
    let snapshot = db.snapshot().await;
    let (reply, _) = execute(&rt, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    success(&reply);
    require(
        &reply.text,
        &["unknown", "legacy reason", "bad setter reason"],
    );
    assert!(!reply.text.contains("legacy-not-a-time"));
    assert!(!reply.text.contains("000333333333333333333"));
    assert!(!reply.text.contains("1970-01-01"));
    assert_eq!(db.snapshot().await, snapshot);
    assert!(mock.requests().is_empty());
    db.close().await;
}

#[tokio::test]
async fn empty_unavailable_cold_visibility_and_out_of_range_are_not_conflated() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(db.store.clone(), &mock, visibility(&[(CHANNEL, true)]));
    let (empty, _) = execute(&rt, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    success(&empty);
    assert!(
        empty.text.to_lowercase().contains("no saved")
            || empty.text.to_lowercase().contains("no recover")
    );
    db.seed(GUILD, CHANNEL, AT, None, "saved fixture").await;
    let (out, _) = execute(
        &rt,
        &router(true),
        &interaction(COMMAND, Some(max_page()), MANAGE),
    )
    .await;
    require(&out.text, &["page"]);
    assert_ne!(out.text, empty.text);
    let cold = runtime(db.store.clone(), &mock, Arc::new(build_cache()));
    let (unknown, receipts) =
        execute(&cold, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    assert!(
        receipts.moderation().is_empty(),
        "cold authority must fail before saved-reason reads"
    );
    assert_eq!(unknown.outcome, "refused");
    require(&unknown.text, &["unavailable", "try again"]);
    assert!(
        !unknown.text.contains("saved fixture") && !unknown.text.contains(&CHANNEL.to_string())
    );
    // Query failure, not empty. Close this fixture's pool, never another database.
    db.store.pool().close().await;
    let (unavailable, _) = execute(&rt, &router(true), &interaction(COMMAND, None, MANAGE)).await;
    assert_eq!(unavailable.outcome, "refused");
    require(&unavailable.text, &["try again"]);
    assert_ne!(unavailable.text, empty.text);
    assert!(mock.requests().is_empty());
    db.close().await;
}

#[tokio::test]
async fn private_lifecycle_disables_mentions_and_never_mutates_channels_or_ledgers() {
    let db = Database::open().await;
    let mock = MockRest::with_responder(|request| {
        if request.method == "POST" && request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else if request.method == "PATCH" && request.path.ends_with("/messages/@original") {
            ScriptedResponse::json(200, json!({}))
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await;
    let rt = runtime(db.store.clone(), &mock, visibility(&[(CHANNEL, true)]));
    db.seed(
        GUILD,
        CHANNEL,
        AT,
        None,
        &format!("@everyone @here <@{ACTOR}> <@&{ACTOR}> `ping` 🦀"),
    )
    .await;
    let snapshot = db.snapshot().await;
    assert!(rt
        .respond(&router(true), &interaction(COMMAND, None, MANAGE))
        .await
        .expect("private response"));
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "only one defer and one original-response edit; no live overwrite fetch"
    );
    let defer: Value = serde_json::from_slice(&requests[0].body).expect("defer JSON");
    assert_eq!(defer["type"], 5);
    assert_eq!(
        defer["data"]["flags"].as_u64().expect("ephemeral flags") & 64,
        64
    );
    let edit: Value = serde_json::from_slice(&requests[1].body).expect("edit JSON");
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(edit["allowed_mentions"][key]
            .as_array()
            .is_none_or(Vec::is_empty));
    }
    assert_ne!(edit["allowed_mentions"]["replied_user"], true);
    let text = edit["content"].as_str().expect("inventory content");
    require(text, &["saved", "recover", "channel-inspect"]);
    assert!(text.contains(&CHANNEL.to_string()));
    assert!(text.encode_utf16().count() <= 2000);
    assert_eq!(db.snapshot().await, snapshot);
    db.close().await;
}

#[tokio::test]
async fn rejected_defer_never_reads_saved_reasons_or_claims_a_lane() {
    let db = Database::open().await;
    db.seed(GUILD, CHANNEL, AT, None, "must remain unread")
        .await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let rt = runtime(db.store.clone(), &mock, visibility(&[(CHANNEL, true)]));
    let snapshot = db.snapshot().await;
    let statements = Statements::default();
    let subscriber = tracing_subscriber::registry().with(statements.clone());
    assert!(rt
        .respond(&router(true), &interaction(COMMAND, None, MANAGE))
        .with_subscriber(subscriber)
        .await
        .is_err());
    assert!(statements.moderation().is_empty());
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(db.snapshot().await, snapshot);
    db.close().await;
}
