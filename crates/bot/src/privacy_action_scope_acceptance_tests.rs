//! Protected acceptance: complete /privacy action decoding and truthful scope.
//!
//! Exercise the real dispatcher, privacy service and REST completion, not a
//! replacement decoder. A malformed action must use the existing private usage
//! correction before acquiring any database connection. Valid calls retain the
//! current self/guild scope, cooldown, purge and deletion-intake semantics.
//! Scope assertions deliberately permit wording changes while requiring each
//! analytics reply to distinguish analytics from safety/delivery and erasure.
//! Builder: implement against this file unchanged; report a faulty oracle to
//! its author rather than weakening it. No staging or production bindings.
#![cfg(test)]

use super::*;
use two_bot_cutover::db::{CutoverDb, FunnelWrite};
use two_bot_testsupport::TestDatabase;

const SERVER: u64 = 100000000000000451;
const OTHER_SERVER: u64 = 100000000000000452;
const SELF: u64 = 100000000000000453;
const OTHER: u64 = 100000000000000454;
const REQUEST: &str = "del-0123456789abcdef";
const HOSTILE: &str = "hostile-receipt-@everyone-<@123456789012345678>";
const CORRECTION: &str = "Use `/privacy opt-out`, `/privacy opt-in`, `/privacy status`, `/privacy delete`, `/privacy deletion-status`, or `/privacy deletion-receipt`.";
const STORE_FAILURE: &str = "Privacy command failed; try again.";
const ACTIONS: [&str; 6] = [
    "opt-out",
    "opt-in",
    "status",
    "delete",
    "deletion-status",
    "deletion-receipt",
];

fn action(name: &str, children: Vec<CommandDataOption>) -> CommandDataOption {
    option(name, CommandOptionValue::SubCommand(children))
}

fn request_id(value: &str) -> CommandDataOption {
    option("request-id", CommandOptionValue::String(value.to_owned()))
}

fn valid_action(name: &str) -> CommandDataOption {
    action(
        name,
        if name.starts_with("deletion-") {
            vec![request_id(REQUEST)]
        } else {
            Vec::new()
        },
    )
}

fn invocation(options: Vec<CommandDataOption>) -> Interaction {
    let mut interaction = privacy_interaction_in(SERVER, SELF, Some("status"));
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_mut() else {
        panic!("application-command fixture");
    };
    data.options = options;
    // The top-level user deliberately differs from the authenticated guild
    // member. Valid commands must keep using the latter, without admin rights.
    interaction
        .user
        .as_mut()
        .expect("top-level fixture user")
        .id = Id::new(OTHER);
    interaction
}

fn malformed() -> Vec<(String, Vec<CommandDataOption>)> {
    let mut cases = vec![
        ("missing action".to_owned(), Vec::new()),
        (
            "unknown action".to_owned(),
            vec![action(HOSTILE, Vec::new())],
        ),
    ];
    for name in ACTIONS {
        for value in [
            CommandOptionValue::String(HOSTILE.to_owned()),
            CommandOptionValue::Integer(1),
            CommandOptionValue::Boolean(true),
            CommandOptionValue::SubCommandGroup(vec![valid_action(name)]),
        ] {
            cases.push((
                format!("{name}: wrong action type {value:?}"),
                vec![option(name, value)],
            ));
        }
        for other in ACTIONS {
            cases.push((
                format!("{name} then {other}"),
                vec![valid_action(name), valid_action(other)],
            ));
        }
        for extra in [
            action(HOSTILE, Vec::new()),
            option("request-id", CommandOptionValue::String(HOSTILE.to_owned())),
        ] {
            cases.push((
                format!("{name}: extra last {}", extra.name),
                vec![valid_action(name), extra.clone()],
            ));
            cases.push((
                format!("{name}: extra first {}", extra.name),
                vec![extra, valid_action(name)],
            ));
        }
    }
    for name in ["opt-out", "opt-in", "status", "delete"] {
        for child in [
            request_id(HOSTILE),
            option("member", CommandOptionValue::User(Id::new(OTHER))),
            option(HOSTILE, CommandOptionValue::Boolean(true)),
            action("status", Vec::new()),
        ] {
            cases.push((
                format!("{name}: unexpected child {}", child.name),
                vec![action(name, vec![child])],
            ));
        }
    }
    for name in ["deletion-status", "deletion-receipt"] {
        cases.push((
            format!("{name}: missing request-id"),
            vec![action(name, Vec::new())],
        ));
        for value in [
            CommandOptionValue::Integer(1),
            CommandOptionValue::Boolean(false),
            CommandOptionValue::User(Id::new(OTHER)),
            CommandOptionValue::SubCommand(Vec::new()),
            CommandOptionValue::SubCommandGroup(Vec::new()),
        ] {
            let wrong = option("request-id", value);
            cases.push((
                format!("{name}: wrong request-id type {:?}", wrong.value),
                vec![action(name, vec![wrong.clone()])],
            ));
            cases.push((
                format!("{name}: typed duplicate first {:?}", wrong.value),
                vec![action(name, vec![wrong.clone(), request_id(REQUEST)])],
            ));
            cases.push((
                format!("{name}: typed duplicate last {:?}", wrong.value),
                vec![action(name, vec![request_id(REQUEST), wrong])],
            ));
        }
        for (a, b) in [(REQUEST, HOSTILE), (HOSTILE, REQUEST), (REQUEST, REQUEST)] {
            cases.push((
                format!("{name}: duplicated request-id {a}/{b}"),
                vec![action(name, vec![request_id(a), request_id(b)])],
            ));
        }
        for extra in [
            option("member", CommandOptionValue::User(Id::new(OTHER))),
            option(HOSTILE, CommandOptionValue::String(HOSTILE.to_owned())),
            action("status", Vec::new()),
        ] {
            cases.push((
                format!("{name}: unknown child last {}", extra.name),
                vec![action(name, vec![request_id(REQUEST), extra.clone()])],
            ));
            cases.push((
                format!("{name}: unknown child first {}", extra.name),
                vec![action(name, vec![extra, request_id(REQUEST)])],
            ));
        }
        // Existing service constraints are byte-based, not a new receipt-id
        // format: nonblank, at most 128 bytes, no control characters.
        for value in [
            String::new(),
            " \t ".to_owned(),
            "x".repeat(129),
            "é".repeat(65),
            "request\nvalue".to_owned(),
            "request\0value".to_owned(),
        ] {
            cases.push((
                format!("{name}: invalid text {value:?}"),
                vec![action(name, vec![request_id(&value)])],
            ));
        }
    }
    cases
}

struct Harness {
    mock: MockRest,
    runtime: CommandRuntime,
}

impl Harness {
    async fn new(pool: PgPool) -> Self {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin))
            .expect("mock REST executor");
        let mut privacy_gates = gates(false, false);
        privacy_gates.configured_guild = Some(SERVER);
        let runtime = CommandRuntime::new(
            pool,
            executor,
            router_with_commands(privacy_gates),
            SERVER,
            false,
        );
        Self { mock, runtime }
    }

    async fn reply(&self, interaction: &Interaction) -> String {
        let start = self.mock.requests().len();
        self.runtime.on_interaction(interaction).await;
        let requests = self.mock.requests();
        let pair = &requests[start..];
        assert_eq!(
            pair.len(),
            2,
            "one private reply, no additional Discord effects"
        );
        assert_eq!(pair[0].method, "POST");
        assert!(pair[0].path.ends_with("/callback"));
        let ack: serde_json::Value = serde_json::from_slice(&pair[0].body).expect("ack JSON");
        assert_eq!(ack["type"], 5);
        assert_eq!(ack["data"]["flags"], 64, "private defer");
        assert_eq!(pair[1].method, "PATCH");
        assert!(pair[1].path.ends_with("/messages/@original"));
        let reply: serde_json::Value = serde_json::from_slice(&pair[1].body).expect("reply JSON");
        assert_eq!(reply["allowed_mentions"]["parse"], serde_json::json!([]));
        let text = reply["content"].as_str().expect("reply text").to_owned();
        assert!(text.encode_utf16().count() <= 2000);
        assert!(
            !text.contains(&interaction.token),
            "no interaction-token echo"
        );
        assert!(!text.contains(HOSTILE));
        assert!(!text.contains("@everyone"));
        assert!(!text.contains("<@"));
        text
    }

    async fn close(self) {
        self.mock.shutdown().await;
    }
}

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the privacy acceptance database"
            );
            eprintln!("SKIP privacy scope DB acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("owned disposable privacy database"),
    )
}

async fn seed(pool: &PgPool, guild: u64, member: u64) {
    let db = CutoverDb::from_pool(pool.clone());
    let (inserted, _) = two_bot_cutover::db::record_event(
        &db,
        &FunnelWrite {
            member_id: Some(member.to_string()),
            guild_id: guild.to_string(),
            event_type: "first_message".to_owned(),
            occurred_at: "2026-10-12T00:00:00Z".to_owned(),
            source: "privacy-shape-fixture".to_owned(),
            metadata: Some("{}".to_owned()),
        },
    )
    .await
    .expect("seed event");
    assert!(inserted);
}

async fn events(pool: &PgPool, guild: u64, member: u64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
        .bind(guild.to_string())
        .bind(member.to_string())
        .fetch_one(pool)
        .await
        .expect("event count")
}

async fn snapshot(pool: &PgPool) -> Vec<String> {
    let mut rows = Vec::new();
    // Every column (including timestamps), including retained audit/requests.
    // The acquisition observer below additionally catches reads and rolled-back
    // writes, which a before/after snapshot alone cannot establish.
    for table in [
        "events",
        "members",
        "community_facts",
        "invite_snapshots",
        "privacy_opt_outs",
        "privacy_opt_out_audit",
        "guild_analytics_settings",
        "retention_deletion_requests",
    ] {
        let query = format!("SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb)::text FROM {table} t");
        rows.push(
            sqlx::query_scalar(&query)
                .fetch_one(pool)
                .await
                .expect("complete relation snapshot"),
        );
    }
    rows
}

#[tokio::test]
async fn malformed_actions_share_one_private_correction_even_with_unavailable_storage() {
    // A CLOSED lazy pool cannot contact any database, including on a bad test
    // fixture. A mistakenly dispatched service call gets the failure reply,
    // not the usage correction. The real DB observer below also proves zero
    // acquisitions, so catching a store error and disguising it cannot pass.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_ci")
        .expect("lazy test-only pool");
    pool.close().await;
    let harness = Harness::new(pool).await;
    let mut failures = Vec::new();
    for (name, options) in malformed() {
        let text = harness.reply(&invocation(options)).await;
        if text != CORRECTION {
            failures.push(format!("{name}: {text}"));
        }
    }
    harness.close().await;
    assert!(
        failures.is_empty(),
        "malformed action dispatched instead of correcting:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn all_six_valid_actions_reach_existing_store_failure_semantics() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_ci")
        .expect("lazy test-only pool");
    pool.close().await;
    let harness = Harness::new(pool).await;
    for name in ACTIONS {
        assert_eq!(
            harness.reply(&invocation(vec![valid_action(name)])).await,
            STORE_FAILURE,
            "{name}: valid input must not become usage or false success"
        );
    }
    for name in ["deletion-status", "deletion-receipt"] {
        // Preserve opaque text accepted by the existing service; do not impose
        // a new hash/prefix grammar, trim, or switch from bytes to characters.
        for value in [
            "a".to_owned(),
            "x".repeat(128),
            "é".repeat(64),
            " legacy opaque id ".to_owned(),
        ] {
            assert_eq!(
                harness
                    .reply(&invocation(vec![action(name, vec![request_id(&value)])]))
                    .await,
                STORE_FAILURE
            );
        }
    }
    harness.close().await;
}

#[tokio::test]
async fn guild_and_unknown_member_refusals_remain_private_without_store_access() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_ci")
        .expect("lazy test-only pool");
    pool.close().await;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin))
        .expect("mock REST executor");
    let mut dm = invocation(vec![valid_action("opt-out")]);
    set_guild(&mut dm, None);
    let mut unknown = invocation(vec![valid_action("delete")]);
    unknown.member = None;
    unknown.user = None;
    for (interaction, expected) in [
        (dm, "This command only works in a server."),
        (
            unknown,
            "Could not identify you for this server; try again.",
        ),
    ] {
        let start = mock.requests().len();
        // The dispatcher itself fences guild-less traffic. Direct handler
        // invocation covers its defensive guard after the caller's defer.
        crate::privacy_command::privacy_command(&pool, &executor, &interaction).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), start + 1);
        let completion = &requests[start];
        assert_eq!(completion.method, "PATCH");
        assert!(completion.path.ends_with("/messages/@original"));
        let body: serde_json::Value =
            serde_json::from_slice(&completion.body).expect("guard completion");
        assert_eq!(body["content"], expected);
        assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn malformed_actions_acquire_no_connection_and_change_no_rows() {
    let Some(db) = database().await else { return };
    seed(db.pool(), SERVER, SELF).await;
    seed(db.pool(), SERVER, OTHER).await;
    seed(db.pool(), OTHER_SERVER, SELF).await;
    let before = snapshot(db.pool()).await;
    let acquisitions = Arc::new(AtomicU64::new(0));
    let observed = acquisitions.clone();
    let connections = acquisitions.clone();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .test_before_acquire(false)
        .after_connect(move |_, _| {
            connections.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        })
        .before_acquire(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(true) })
        })
        .connect_with(db.pool().connect_options().as_ref().clone())
        .await
        .expect("instrumented owned fixture pool");
    let harness = Harness::new(pool.clone()).await;
    // Healthy observer: the actual status path must acquire a connection.
    let control = harness
        .reply(&invocation(vec![valid_action("status")]))
        .await;
    assert!(control.contains("Server switches:"));
    assert!(
        acquisitions.load(Ordering::SeqCst) > 0,
        "observer detects valid service reads"
    );
    acquisitions.store(0, Ordering::SeqCst);
    let mut failures = Vec::new();
    for (name, options) in malformed() {
        let prior = acquisitions.load(Ordering::SeqCst);
        let text = harness.reply(&invocation(options)).await;
        if text != CORRECTION || acquisitions.load(Ordering::SeqCst) != prior {
            failures.push(name);
        }
    }
    let after = snapshot(db.pool()).await;
    harness.close().await;
    pool.close().await;
    db.close()
        .await
        .expect("drop disposable database before RED assertion");
    assert_eq!(
        before, after,
        "malformed actions must not purge, suppress, queue or audit"
    );
    assert!(
        failures.is_empty(),
        "malformed actions reached storage or failed correction: {failures:?}"
    );
}

fn scope_defects(text: &str) -> Vec<&'static str> {
    let text = text.to_lowercase();
    let mut defects = Vec::new();
    for (meaning, present) in [
        ("analytics", text.contains("analytics")),
        ("server-local", text.contains("this server")),
        (
            "server switches still apply",
            text.contains("switch") && text.contains("server"),
        ),
        ("moderation", text.contains("moderation")),
        ("safety", text.contains("safety")),
        ("delivery", text.contains("delivery")),
        (
            "unchanged operational records",
            text.contains("unchanged") || text.contains("not affected"),
        ),
    ] {
        if !present {
            defects.push(meaning);
        }
    }
    // These are actual misleading promises, not bans on accurate negative
    // wording such as "Discord messages are not deleted".
    for false_claim in [
        "all your data was deleted",
        "all your records were deleted",
        "discord messages were deleted",
        "reminders are disabled",
        "restored your deleted",
        "deleted history is restored",
    ] {
        if text.contains(false_claim) {
            defects.push("false erasure/delivery/restoration promise");
        }
    }
    defects
}

#[tokio::test]
async fn every_analytics_outcome_discloses_scope_without_changing_service_effects() {
    let Some(db) = database().await else { return };
    for (guild, member) in [(SERVER, SELF), (SERVER, OTHER), (OTHER_SERVER, SELF)] {
        seed(db.pool(), guild, member).await;
    }
    let service = CutoverDb::from_pool(db.pool().clone());
    let harness = Harness::new(db.pool().clone()).await;
    let mut replies = Vec::new();
    // Exercise every server-switch combination in both member states. The
    // opt-in preference is not a promise that a disabled family is collecting.
    for suppressed in [false, true] {
        if suppressed {
            replies.push((
                "fresh opt-out".to_owned(),
                harness
                    .reply(&invocation(vec![valid_action("opt-out")]))
                    .await,
            ));
            assert_eq!(events(db.pool(), SERVER, SELF).await, 0);
            replies.push((
                "repeat opt-out".to_owned(),
                harness
                    .reply(&invocation(vec![valid_action("opt-out")]))
                    .await,
            ));
        }
        for presence in [false, true] {
            for activity in [false, true] {
                for voice in [false, true] {
                    two_bot_cutover::privacy::set_analytics_settings(
                        &service,
                        &SERVER.to_string(),
                        presence,
                        activity,
                        voice,
                    )
                    .await
                    .expect("fixture switches");
                    let text = harness
                        .reply(&invocation(vec![valid_action("status")]))
                        .await;
                    for (family, enabled) in [
                        ("presence", presence),
                        ("activity", activity),
                        ("voice", voice),
                    ] {
                        assert!(
                            text.contains(&format!(
                                "{family} {}",
                                if enabled { "on" } else { "off" }
                            )),
                            "actual server switch {family}: {text}"
                        );
                    }
                    assert!(text.contains(if suppressed { "opted out" } else { "on" }));
                    replies.push((
                        format!("status {suppressed}/{presence}/{activity}/{voice}"),
                        text,
                    ));
                }
            }
        }
    }
    let opt_in = harness
        .reply(&invocation(vec![valid_action("opt-in")]))
        .await;
    assert!(
        opt_in.contains("not restored"),
        "opt-in cannot restore purged analytics"
    );
    replies.push(("opt-in after purge".to_owned(), opt_in));
    replies.push((
        "opt-in no-op".to_owned(),
        harness
            .reply(&invocation(vec![valid_action("opt-in")]))
            .await,
    ));
    let cooldown = harness
        .reply(&invocation(vec![valid_action("opt-out")]))
        .await;
    assert!(
        cooldown.contains("few minutes"),
        "keep cooldown, never claim opted out"
    );
    replies.push(("cooldown".to_owned(), cooldown));
    assert!(!two_bot_cutover::privacy::is_opted_out(
        &service,
        &SERVER.to_string(),
        &SELF.to_string()
    )
    .await
    .expect("suppression"));
    assert_eq!(
        events(db.pool(), SERVER, SELF).await,
        0,
        "no history restored"
    );
    assert_eq!(
        events(db.pool(), SERVER, OTHER).await,
        1,
        "other member survives"
    );
    assert_eq!(
        events(db.pool(), OTHER_SERVER, SELF).await,
        1,
        "other server survives"
    );
    let receipts: Vec<(String, String, String)> =
        sqlx::query_as("SELECT guild_id, member_id, actor FROM privacy_opt_out_audit")
            .fetch_all(db.pool())
            .await
            .expect("audit receipts");
    assert_eq!(
        receipts,
        vec![(
            SERVER.to_string(),
            SELF.to_string(),
            "member:self-serve".to_owned()
        )],
        "one real purge; repeat, status, opt-in and cooldown never add receipts"
    );
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_deletion_requests")
        .fetch_one(db.pool())
        .await
        .expect("deletion queue");
    assert_eq!(queued, 0, "analytics preference never submits full erasure");
    let defects: Vec<_> = replies
        .iter()
        .filter_map(|(name, text)| {
            let defects = scope_defects(text);
            (!defects.is_empty()).then(|| format!("{name}: missing {defects:?}; reply={text}"))
        })
        .collect();
    harness.close().await;
    db.close()
        .await
        .expect("drop disposable database before RED assertion");
    assert!(
        defects.is_empty(),
        "inconsistent analytics scope:\n{}",
        defects.join("\n")
    );
}

#[tokio::test]
async fn valid_deletion_intake_status_receipt_and_pair_isolation_are_unchanged() {
    let Some(db) = database().await else { return };
    seed(db.pool(), SERVER, SELF).await;
    seed(db.pool(), SERVER, OTHER).await;
    let harness = Harness::new(db.pool().clone()).await;
    let delete = invocation(vec![valid_action("delete")]);
    let ack = harness.reply(&delete).await;
    assert!(ack.contains("queued as pending") && ack.contains("not a completion receipt"));
    let (id, status): (String, String) = sqlx::query_as("SELECT request_id, status FROM retention_deletion_requests WHERE guild_id = $1 AND user_id = $2")
        .bind(SERVER.to_string()).bind(SELF.to_string()).fetch_one(db.pool()).await.expect("self-only pending request");
    assert_eq!(status, "pending");
    assert!(ack.contains(&id));
    let pending = harness
        .reply(&invocation(vec![action(
            "deletion-status",
            vec![request_id(&id)],
        )]))
        .await;
    assert!(pending.contains("is pending"));
    assert_eq!(
        harness
            .reply(&invocation(vec![action(
                "deletion-receipt",
                vec![request_id(&id)]
            )]))
            .await,
        STORE_FAILURE,
        "pending is not a receipt"
    );
    for name in ["deletion-status", "deletion-receipt"] {
        let mut other = invocation(vec![action(name, vec![request_id(&id)])]);
        other
            .member
            .as_mut()
            .expect("guild member")
            .user
            .as_mut()
            .expect("member user")
            .id = Id::new(OTHER);
        assert_eq!(
            harness.reply(&other).await,
            STORE_FAILURE,
            "other-member access fails privately"
        );
        let result = if name == "deletion-status" {
            two_bot_cutover::deletion_intake::deletion_status_for_requester(
                db.pool(),
                &OTHER_SERVER.to_string(),
                &SELF.to_string(),
                &id,
            )
            .await
            .map(|_| ())
        } else {
            two_bot_cutover::deletion_intake::deletion_receipt_for_requester(
                db.pool(),
                &OTHER_SERVER.to_string(),
                &SELF.to_string(),
                &id,
            )
            .await
            .map(|_| ())
        };
        assert!(result.is_err(), "other server cannot read the request");
    }
    // Fixture-only settled ledger, not retention execution or an erasure.
    sqlx::query("UPDATE retention_deletion_requests SET status = 'completed', completed_at = clock_timestamp(), receipt_id = 'scope-test-receipt' WHERE request_id = $1")
        .bind(&id).execute(db.pool()).await.expect("fixture completion");
    let retry = harness.reply(&delete).await;
    assert!(retry.contains("already accepted") && retry.contains("not a completion receipt"));
    assert!(!retry.contains("queued"));
    let receipt = harness
        .reply(&invocation(vec![action(
            "deletion-receipt",
            vec![request_id(&id)],
        )]))
        .await;
    assert!(receipt.contains("scope-test-receipt"));
    assert!(receipt.contains("erasable rows in the member erasure plan"));
    assert!(
        receipt.contains("retention_deletion_requests")
            && receipt.contains("privacy_opt_out_audit"),
        "receipt keeps retained-ledger limits"
    );
    assert_eq!(
        events(db.pool(), SERVER, SELF).await,
        1,
        "intake/reads never perform deletion"
    );
    assert_eq!(events(db.pool(), SERVER, OTHER).await, 1);
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_deletion_requests")
        .fetch_one(db.pool())
        .await
        .expect("request count");
    assert_eq!(
        requests, 1,
        "same interaction reuses request after completion"
    );
    harness.close().await;
    db.close().await.expect("drop disposable database");
}
