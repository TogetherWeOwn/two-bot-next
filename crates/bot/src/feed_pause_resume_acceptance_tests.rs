//! Protected command/store/poller lifecycle acceptance. No feature emulation:
//! raw slash inputs go through CommandRuntime and the real disposable DB store.
#![cfg(test)]

use super::*;
use serde_json::Value;
use tokio::sync::Notify;
use twilight_model::application::interaction::Interaction;
use two_bot_core::RouterGates;

use crate::command_runtime::{router_with_commands, CommandRuntime};

fn slash(name: &str, id: Option<&str>, permission: u64, guild: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"1111", "authorizing_integration_owners":{"0":guild},
        "id":"7007", "token":"synthetic-feed-lifecycle-token", "type":2,
        "guild_id":guild, "channel_id":CHANNEL, "entitlements":[],
        "member":{"user":{"id":"88","username":"fixture-admin","discriminator":"0", "bot":false, "mfa_enabled":false},
                  "permissions":permission.to_string(), "roles":[], "deaf":false, "mute":false, "flags":0, "joined_at":null},
        "data":{"id":"1","name":name,"type":1,
                "options":id.map_or_else(|| json!([]), |id| json!([{"name":"id","type":3,"value":id}]))}
    })).expect("valid synthetic slash interaction")
}

async fn invoke(
    pool: &PgPool,
    name: &str,
    id: Option<&str>,
    permission: u64,
    guild: &str,
) -> String {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id":"9001"})),
    )
    .await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor(&mock),
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
    );
    runtime
        .on_interaction(&slash(name, id, permission, guild))
        .await;
    let requests = mock.requests();
    let mut replies = Vec::new();
    for request in &requests {
        assert!(
            !request.path.contains("/channels/"),
            "lifecycle commands must not post to a channel"
        );
        let body: Value = serde_json::from_slice(&request.body).expect("command response JSON");
        let data = if request.method == "PATCH" {
            &body
        } else {
            &body["data"]
        };
        if let Some(content) = data["content"].as_str() {
            assert_eq!(data["allowed_mentions"]["parse"], json!([]));
            assert!(content.encode_utf16().count() <= 2000);
            replies.push(content.to_owned());
        }
        if request.method == "POST" {
            assert_eq!(
                body["data"]["flags"].as_u64().unwrap_or(0) & 64,
                64,
                "member command replies/defer must be ephemeral"
            );
        }
    }
    mock.shutdown().await;
    assert_eq!(
        replies.len(),
        1,
        "one truthful lifecycle reply, got {replies:?}"
    );
    replies.remove(0)
}

/// Silence-aware invocation for the foreign-guild refusal case. The shared
/// router maps every generic builtin (including `feed-add`/`feed-remove`)
/// from a foreign guild to `SlashOutcome::Ignore`, and `on_interaction`
/// stays silent on `Ignore`, so zero REST requests is the correct
/// no-effects posture — not one reply.
async fn invoke_silent(pool: &PgPool, name: &str, id: Option<&str>, permission: u64, guild: &str) {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::json(200, json!({"id":"9001"})),
    )
    .await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor(&mock),
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
    );
    runtime
        .on_interaction(&slash(name, id, permission, guild))
        .await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert!(
        requests.is_empty(),
        "foreign-guild {name} stays silent, got {} request(s)",
        requests.len()
    );
}

async fn command(pool: &PgPool, name: &str, id: &str) -> String {
    invoke(pool, name, Some(id), 32, GUILD).await
}

async fn enabled(pool: &PgPool, id: &str) -> bool {
    sqlx::query_scalar("SELECT enabled FROM feed_relays WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("relay still exists")
}

async fn snapshot(pool: &PgPool, lifecycle_fields: bool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'relays', (SELECT coalesce(jsonb_agg(CASE WHEN $1 THEN to_jsonb(r)
                       ELSE to_jsonb(r) - 'enabled' - 'updated_at' END ORDER BY id), '[]'::jsonb)
                     FROM feed_relays r),
          'deliveries', (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY feed_id, item_key), '[]'::jsonb)
                         FROM feed_deliveries d))"
    ).bind(lifecycle_fields).fetch_one(pool).await.expect("snapshot every feed/ledger column")
}

async fn accepted(pool: &PgPool, relay: &FeedRelay, index: usize) -> FeedPost {
    let post = seed_pending(pool, relay, index).await;
    assert!(store::mark_delivered(
        pool,
        GUILD,
        &post,
        &format!("pending-seed-{index}"),
        "9001",
        now_millis_for_test()
    )
    .await
    .unwrap());
    store::mark_checked(pool, GUILD, &relay.id, 123456)
        .await
        .unwrap();
    post
}

fn recovery_reply(reply: &str) {
    let lower = reply.to_ascii_lowercase();
    assert!(
        lower.contains("recover") || lower.contains("pending"),
        "explain the recovery refusal: {reply}"
    );
    assert!(
        lower.contains("retry") || lower.contains("try again"),
        "give an actionable retry after recovery: {reply}"
    );
    assert!(
        !lower.starts_with("paused"),
        "must not announce success while still enabled: {reply}"
    );
}

#[tokio::test]
async fn pause_is_idempotent_and_preserves_identity_high_water_and_complete_receipts() {
    let db = fixture().await;
    let relay = feed("pause-history");
    store::add_feed(db.pool(), &relay).await.unwrap();
    store::add_feed(db.pool(), &feed("other-relay"))
        .await
        .unwrap();
    accepted(db.pool(), &relay, 0).await;
    let before = snapshot(db.pool(), false).await;
    let reply = command(db.pool(), "feed-pause", &relay.id).await;
    let paused = !enabled(db.pool(), &relay.id).await;
    let again = command(db.pool(), "feed-pause", &relay.id).await;
    assert_eq!(
        snapshot(db.pool(), false).await,
        before,
        "do not delete/re-add or rewrite delivery evidence"
    );
    assert!(
        enabled(db.pool(), "other-relay").await,
        "pause only the selected relay"
    );
    assert!(
        paused && !enabled(db.pool(), &relay.id).await,
        "pause must commit disabled state: {reply}"
    );
    assert!(reply.to_ascii_lowercase().contains("paused"));
    assert!(
        again.to_ascii_lowercase().contains("already")
            && again.to_ascii_lowercase().contains("paused"),
        "truthful no-op: {again}"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn resume_is_idempotent_and_preserves_history_across_independent_pool_restart() {
    let db = fixture().await;
    let relay = feed("resume-history");
    store::add_feed(db.pool(), &relay).await.unwrap();
    accepted(db.pool(), &relay, 0).await;
    sqlx::query("UPDATE feed_relays SET enabled = false WHERE id = $1")
        .bind(&relay.id)
        .execute(db.pool())
        .await
        .unwrap();
    let before = snapshot(db.pool(), false).await;
    let reopened = db.independent_pool().await.unwrap();
    let reply = command(&reopened, "feed-resume", &relay.id).await;
    let resumed = enabled(&reopened, &relay.id).await;
    let again = command(&reopened, "feed-resume", &relay.id).await;
    assert_eq!(
        snapshot(&reopened, false).await,
        before,
        "preserve seen items, nonces, receipt and check timestamp"
    );
    assert!(
        resumed && enabled(db.pool(), &relay.id).await,
        "resume must commit enabled state: {reply}"
    );
    assert!(reply.to_ascii_lowercase().contains("resumed"));
    assert!(
        again.to_ascii_lowercase().contains("already"),
        "truthful resume no-op: {again}"
    );
    reopened.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn lifecycle_commands_refuse_insufficient_permissions_foreign_guilds_and_unknown_ids_without_writes(
) {
    let db = fixture().await;
    for name in ["feed-pause", "feed-resume"] {
        let mut relay = feed(name);
        relay.enabled = name == "feed-pause";
        store::add_feed(db.pool(), &relay).await.unwrap();
        for (permission, guild) in [(0, GUILD), (8192, GUILD), (32, "9999")] {
            let before = snapshot(db.pool(), true).await;
            if guild != GUILD {
                invoke_silent(db.pool(), name, Some(&relay.id), permission, guild).await;
                assert_eq!(
                    snapshot(db.pool(), true).await,
                    before,
                    "refused {name} must not alter either guild"
                );
                continue;
            }
            let reply = invoke(db.pool(), name, Some(&relay.id), permission, guild).await;
            assert_eq!(
                snapshot(db.pool(), true).await,
                before,
                "refused {name} must not alter either guild"
            );
            let lower = reply.to_ascii_lowercase();
            if permission != 32 {
                assert!(
                    lower.contains("manage server") || lower.contains("manage guild"),
                    "same authority as add/remove: {reply}"
                );
            } else {
                assert!(
                    lower.contains("guild") || lower.contains("server"),
                    "foreign-guild refusal: {reply}"
                );
            }
        }
        let before = snapshot(db.pool(), true).await;
        let reply = command(db.pool(), name, "not-a-relay").await;
        assert_eq!(snapshot(db.pool(), true).await, before);
        let lower = reply.to_ascii_lowercase();
        assert!(
            lower.contains("not found") || lower.contains("no feed") || lower.contains("no relay"),
            "unknown ID is not success: {reply}"
        );
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn runtime_list_includes_paused_ids_and_status_without_cross_guild_reads_or_writes() {
    let db = fixture().await;
    let active = feed("visible-alpha");
    let mut paused = feed("visible-beta");
    paused.enabled = false;
    let mut foreign = feed("foreign-private-relay");
    foreign.guild_id = "9999".into();
    for relay in [&active, &paused, &foreign] {
        store::add_feed(db.pool(), relay).await.unwrap();
    }
    let before = snapshot(db.pool(), true).await;
    let reply = invoke(db.pool(), "feed-list", None, 32, GUILD).await;
    assert_eq!(
        snapshot(db.pool(), true).await,
        before,
        "discovery is read-only"
    );
    assert!(!reply.contains(&foreign.id));
    for (relay, state) in [(&active, "enabled"), (&paused, "paused")] {
        let row = reply
            .lines()
            .find(|line| line.starts_with(&format!("`{}`", relay.id)))
            .expect("stable lifecycle ID remains discoverable in runtime output");
        assert!(
            row.to_ascii_lowercase().contains(state),
            "missing {state}: {row}"
        );
        assert!(row.contains(&relay.source));
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn repeated_pause_resume_cycles_preserve_receipts_and_state_across_reopened_pools() {
    let db = fixture().await;
    let relay = feed("lifecycle-cycles");
    store::add_feed(db.pool(), &relay).await.unwrap();
    accepted(db.pool(), &relay, 0).await;
    let before = snapshot(db.pool(), false).await;
    for _ in 0..3 {
        for (name, want_enabled) in [("feed-pause", false), ("feed-resume", true)] {
            let peer = db.independent_pool().await.unwrap();
            let reply = command(&peer, name, &relay.id).await;
            let actual = enabled(&peer, &relay.id).await;
            assert_eq!(
                snapshot(&peer, false).await,
                before,
                "cycles must not reset dedup state"
            );
            peer.close().await;
            assert_eq!(
                actual, want_enabled,
                "{name} must persist across restart: {reply}"
            );
        }
    }
    db.close().await.unwrap();
}

async fn pending_refusal(expired: bool, tokenless: bool) {
    let db = fixture().await;
    let relay = feed("pending-pause");
    store::add_feed(db.pool(), &relay).await.unwrap();
    seed_pending(db.pool(), &relay, 0).await;
    if expired {
        expire(db.pool()).await;
    }
    if tokenless {
        sqlx::query("UPDATE feed_deliveries SET claim_token = NULL, claimed_at = NULL")
            .execute(db.pool())
            .await
            .unwrap();
    }
    let before = snapshot(db.pool(), true).await;
    let reply = command(db.pool(), "feed-pause", &relay.id).await;
    assert!(
        enabled(db.pool(), &relay.id).await,
        "never strand pending recovery behind disabled discovery"
    );
    assert_eq!(
        snapshot(db.pool(), true).await,
        before,
        "including original lease/token/timestamps"
    );
    recovery_reply(&reply);
    db.close().await.unwrap();
}

#[tokio::test]
async fn pause_refuses_a_current_pending_lease() {
    pending_refusal(false, false).await;
}

#[tokio::test]
async fn pause_refuses_expired_uncertain_send_evidence() {
    pending_refusal(true, false).await;
}

#[tokio::test]
async fn pause_refuses_restored_tokenless_pending_evidence() {
    pending_refusal(false, true).await;
}

#[tokio::test]
async fn pause_retries_only_after_nonce_recovery_settles_without_a_repost() {
    let db = fixture().await;
    let relay = feed("pause-recovery");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let post = seed_pending(db.pool(), &relay, 0).await;
    expire(db.pool()).await;
    let refusal = command(db.pool(), "feed-pause", &relay.id).await;
    assert!(enabled(db.pool(), &relay.id).await);
    let mock = MockRest::start(vec![me(), history(&post)], ScriptedResponse::status(500)).await;
    let reopened = db.independent_pool().await.unwrap();
    run_once(&reopened, &executor(&mock), GUILD, &Items(Vec::new()))
        .await
        .unwrap();
    assert_eq!(states(&reopened).await, (1, 0));
    assert_eq!(
        post_count(&mock),
        0,
        "accepted nonce is reconciled, never blindly posted"
    );
    let before = snapshot(&reopened, false).await;
    let reply = command(&reopened, "feed-pause", &relay.id).await;
    let paused = !enabled(&reopened, &relay.id).await;
    assert_eq!(snapshot(&reopened, false).await, before);
    mock.shutdown().await;
    reopened.close().await;
    recovery_reply(&refusal);
    assert!(paused, "retry after settlement can pause: {reply}");
    db.close().await.unwrap();
}

#[tokio::test]
async fn uncertain_history_miss_survives_refused_pause_and_restart_without_reposting() {
    let db = fixture().await;
    let relay = feed("pause-unknown");
    store::add_feed(db.pool(), &relay).await.unwrap();
    seed_pending(db.pool(), &relay, 0).await;
    expire(db.pool()).await;
    let before = snapshot(db.pool(), true).await;
    let reply = command(db.pool(), "feed-pause", &relay.id).await;
    assert_eq!(snapshot(db.pool(), true).await, before);
    let mock = MockRest::start(vec![me()], ScriptedResponse::json(200, json!([]))).await;
    let reopened = db.independent_pool().await.unwrap();
    assert_eq!(
        run_once(&reopened, &executor(&mock), GUILD, &Items(Vec::new()))
            .await
            .err(),
        Some(ErrorClass::RecoveryRequired)
    );
    assert_eq!(
        states(&reopened).await,
        (0, 1),
        "history miss is not evidence of an unsent post"
    );
    assert!(enabled(&reopened, &relay.id).await);
    assert_eq!(post_count(&mock), 0);
    let after = snapshot(&reopened, true).await;
    let before_row = &before["deliveries"][0];
    let after_row = &after["deliveries"][0];
    for key in [
        "feed_id",
        "item_key",
        "nonce",
        "state",
        "message_id",
        "first_seen_at",
        "delivered_at",
    ] {
        assert_eq!(
            after_row[key], before_row[key],
            "recovery must preserve {key}"
        );
    }
    mock.shutdown().await;
    reopened.close().await;
    recovery_reply(&reply);
    db.close().await.unwrap();
}

#[tokio::test]
async fn completed_pause_fences_fresh_claims_and_stale_polled_snapshot_fetches() {
    let db = fixture().await;
    let relay = feed("pause-stale-snapshot");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let before = snapshot(db.pool(), false).await;
    let reply = command(db.pool(), "feed-pause", &relay.id).await;
    let paused = !enabled(db.pool(), &relay.id).await;
    let reopened = db.independent_pool().await.unwrap();
    let post = plan_post(&relay, &item(0)).unwrap();
    let claim = store::claim_fresh_delivery(
        &reopened,
        GUILD,
        &post,
        "after-completed-pause",
        now_millis_for_test(),
    )
    .await
    .unwrap();
    // Release a baseline's wrongly admitted claim so the stale-fetch observation
    // is not masked by a synthetic pending row. The final snapshot still proves
    // the correct path never left evidence behind.
    if claim.is_some() {
        assert!(
            store::release_unposted(&reopened, GUILD, &post, "after-completed-pause")
                .await
                .unwrap()
        );
    }
    let fetch = CountFetch::new(Vec::new());
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    poll_relay(&reopened, &executor(&mock), &relay, &fetch)
        .await
        .unwrap();
    let fetches = fetch.calls.load(Ordering::SeqCst);
    let wire = mock.requests();
    mock.shutdown().await;
    reopened.close().await;
    assert!(
        paused,
        "pause must complete before admission assertions: {reply}"
    );
    assert_eq!(claim, None, "no new post claim after completed pause");
    assert_eq!(
        fetches, 0,
        "a stale enabled snapshot cannot admit a new fetch after pause"
    );
    assert!(wire.is_empty());
    assert_eq!(
        snapshot(db.pool(), false).await,
        before,
        "no post-pause high-water or ledger writes"
    );
    db.close().await.unwrap();
}

#[derive(Clone)]
struct CountFetch {
    calls: Arc<AtomicUsize>,
    items: Vec<FeedItem>,
}
impl CountFetch {
    fn new(items: Vec<FeedItem>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            items,
        }
    }
}
impl FeedFetch for CountFetch {
    fn fetch(&self, _feed: FeedRelay) -> FetchFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let items = self.items.clone();
        Box::pin(async move { Ok(items) })
    }
}

#[derive(Clone)]
struct HeldFetch {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<AtomicUsize>,
}
impl FeedFetch for HeldFetch {
    fn fetch(&self, _feed: FeedRelay) -> FetchFuture {
        let this = self.clone();
        Box::pin(async move {
            this.entered.notify_one();
            this.release.notified().await;
            this.completed.fetch_add(1, Ordering::SeqCst);
            Ok(vec![item(0)])
        })
    }
}

#[tokio::test]
async fn pause_racing_an_admitted_fetch_never_cancels_it_or_admits_a_post_after_success() {
    let db = fixture().await;
    let relay = feed("pause-held-fetch");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let fetch = HeldFetch {
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        completed: Arc::new(AtomicUsize::new(0)),
    };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"9001"}))).await;
    let rest = executor(&mock);
    let poll_pool = db.independent_pool().await.unwrap();
    let task_pool = poll_pool.clone();
    let task_relay = relay.clone();
    let task_fetch = fetch.clone();
    let poll =
        tokio::spawn(async move { poll_relay(&task_pool, &rest, &task_relay, &task_fetch).await });
    tokio::time::timeout(Duration::from_secs(3), fetch.entered.notified())
        .await
        .expect("fetch admitted");
    let command_pool = db.independent_pool().await.unwrap();
    let task_pool = command_pool.clone();
    let id = relay.id.clone();
    let mut pause = tokio::spawn(async move { command(&task_pool, "feed-pause", &id).await });
    let early = tokio::time::timeout(Duration::from_millis(500), &mut pause).await;
    let completed_before_release = early.is_ok();
    fetch.release.notify_one();
    poll.await.unwrap().unwrap();
    let reply = match early {
        Ok(result) => result.unwrap(),
        Err(_) => pause.await.unwrap(),
    };
    let paused = !enabled(db.pool(), &relay.id).await;
    let fresh = store::claim_fresh_delivery(
        db.pool(),
        GUILD,
        &plan_post(&relay, &item(1)).unwrap(),
        "late-claim",
        now_millis_for_test(),
    )
    .await
    .unwrap();
    let posts = post_count(&mock);
    mock.shutdown().await;
    poll_pool.close().await;
    command_pool.close().await;
    assert_eq!(
        fetch.completed.load(Ordering::SeqCst),
        1,
        "already-admitted fetch must not be cancelled"
    );
    assert!(
        paused,
        "pause eventually succeeds once admitted work can settle: {reply}"
    );
    assert_eq!(fresh, None, "post-success claims are fenced");
    if completed_before_release {
        assert_eq!(
            posts, 0,
            "fetch result cannot claim a fresh post after successful pause"
        );
        assert_eq!(states(db.pool()).await, (0, 0));
    } else {
        assert!(
            posts <= 1,
            "waiting for admitted work is allowed, never duplicate it"
        );
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn pause_during_an_admitted_send_refuses_and_preserves_settlement() {
    let db = fixture().await;
    let relay = feed("pause-held-send");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id":"9001"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let poll_pool = db.independent_pool().await.unwrap();
    let task_pool = poll_pool.clone();
    let poll =
        tokio::spawn(
            async move { run_once(&task_pool, &rest, GUILD, &Items(vec![item(0)])).await },
        );
    tokio::time::timeout(Duration::from_secs(3), gate.wait_for_request())
        .await
        .expect("send admitted");
    let before = snapshot(db.pool(), true).await;
    assert_eq!(states(db.pool()).await, (0, 1));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        command(db.pool(), "feed-pause", &relay.id),
    )
    .await;
    let after = snapshot(db.pool(), true).await;
    gate.release();
    poll.await.unwrap().unwrap();
    assert_eq!(
        states(db.pool()).await,
        (1, 0),
        "admitted send is not cancelled or unsettled by pause"
    );
    assert_eq!(post_count(&mock), 1);
    mock.shutdown().await;
    poll_pool.close().await;
    assert_eq!(
        after, before,
        "no disabled relay or lost pending evidence while send is held"
    );
    recovery_reply(&result.expect("pause must refuse pending recovery, not wait across send HTTP"));
    assert!(enabled(db.pool(), &relay.id).await);
    db.close().await.unwrap();
}

#[tokio::test]
async fn pause_uses_the_existing_contended_relay_delivery_fence() {
    let db = fixture().await;
    let relay = feed("pause-contended");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let holder = hold_feed_delivery_advisory_lock(db.pool(), GUILD, &relay.id).await;
    let peer = db.independent_pool().await.unwrap();
    let task_pool = peer.clone();
    let id = relay.id.clone();
    let pause = tokio::spawn(async move { command(&task_pool, "feed-pause", &id).await });
    let queued = wait_for_advisory_waiters(db.pool(), GUILD, &relay.id, 1)
        .await
        .unwrap();
    let enabled_while_held = enabled(db.pool(), &relay.id).await;
    holder.commit().await.unwrap();
    let reply = pause.await.unwrap();
    let paused = !enabled(db.pool(), &relay.id).await;
    peer.close().await;
    assert!(
        queued,
        "pause must acquire the same actual guild/relay advisory lock as claim admission"
    );
    assert!(
        enabled_while_held,
        "cannot confirm pause before its serialized update"
    );
    assert!(paused, "pause commits after fence release: {reply}");
    db.close().await.unwrap();
}

#[tokio::test]
async fn claim_queued_before_pause_wins_and_pause_keeps_recovery_discoverable() {
    let db = fixture().await;
    let relay = feed("pause-behind-claim");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let holder = hold_feed_delivery_advisory_lock(db.pool(), GUILD, &relay.id).await;
    let peer = db.independent_pool().await.unwrap();
    let claim_pool = peer.clone();
    let post = plan_post(&relay, &item(0)).unwrap();
    let claim = tokio::spawn(async move {
        store::claim_fresh_delivery(
            &claim_pool,
            GUILD,
            &post,
            "first-claim",
            now_millis_for_test(),
        )
        .await
    });
    let claim_queued = wait_for_advisory_waiters(db.pool(), GUILD, &relay.id, 1)
        .await
        .unwrap();
    let pause_pool = db.independent_pool().await.unwrap();
    let task_pool = pause_pool.clone();
    let id = relay.id.clone();
    let pause = tokio::spawn(async move { command(&task_pool, "feed-pause", &id).await });
    let both_queued = wait_for_advisory_waiters(db.pool(), GUILD, &relay.id, 2)
        .await
        .unwrap();
    holder.commit().await.unwrap();
    let claimed = claim.await.unwrap().unwrap();
    let reply = pause.await.unwrap();
    let still_enabled = enabled(db.pool(), &relay.id).await;
    assert_eq!(states(db.pool()).await, (0, 1));
    peer.close().await;
    pause_pool.close().await;
    assert!(
        claim_queued && both_queued,
        "observe actual lock queue ordering, not task start order"
    );
    assert_eq!(claimed, Some(DeliveryClaim::Fresh));
    assert!(still_enabled, "pause cannot strand a claim admitted first");
    recovery_reply(&reply);
    db.close().await.unwrap();
}

#[tokio::test]
async fn restart_while_paused_fetches_nothing_and_paused_removal_is_still_available() {
    let db = fixture().await;
    let mut relay = feed("paused-restart-remove");
    relay.enabled = false;
    store::add_feed(db.pool(), &relay).await.unwrap();
    let before = snapshot(db.pool(), true).await;
    let peer = db.independent_pool().await.unwrap();
    let fetch = CountFetch::new(vec![item(0)]);
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    run_once(&peer, &executor(&mock), GUILD, &fetch)
        .await
        .unwrap();
    assert_eq!(fetch.calls.load(Ordering::SeqCst), 0);
    assert!(mock.requests().is_empty());
    assert_eq!(snapshot(&peer, true).await, before);
    assert_eq!(
        store::list_feeds(&peer, GUILD, false).await.unwrap().len(),
        1
    );
    let reply = command(&peer, "feed-remove", &relay.id).await;
    assert!(reply.to_ascii_lowercase().contains("removed"));
    assert_eq!(relay_count(&peer, &relay.id).await, 0);
    mock.shutdown().await;
    peer.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn resume_catch_up_is_bounded_and_never_reposts_accepted_nonces_after_restart() {
    let db = fixture().await;
    let relay = feed("resume-backlog");
    store::add_feed(db.pool(), &relay).await.unwrap();
    let old = accepted(db.pool(), &relay, 0).await;
    let old_row = snapshot(db.pool(), true).await["deliveries"][0].clone();
    sqlx::query("UPDATE feed_relays SET enabled = false WHERE id = $1")
        .bind(&relay.id)
        .execute(db.pool())
        .await
        .unwrap();
    let reply = command(db.pool(), "feed-resume", &relay.id).await;
    let resumed = enabled(db.pool(), &relay.id).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"9001"}))).await;
    let rest = executor(&mock);
    let fetch = Items((0..70).map(item).collect());
    run_once(db.pool(), &rest, GUILD, &fetch).await.unwrap();
    let first = post_count(&mock);
    let peer = db.independent_pool().await.unwrap();
    run_once(&peer, &rest, GUILD, &fetch).await.unwrap();
    let all = mock.requests();
    let posts: Vec<_> = all.iter().filter(|r| r.method == "POST").collect();
    let mut nonces = std::collections::HashSet::new();
    for request in &posts {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let nonce = body["nonce"].as_str().expect("stable string nonce");
        assert_ne!(nonce, old.nonce);
        assert!(
            nonces.insert(nonce.to_owned()),
            "no accepted nonce may be reposted across passes/restart"
        );
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }
    let after = snapshot(&peer, true).await;
    let retained = after["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["item_key"] == old.item_key)
        .unwrap();
    assert_eq!(
        retained, &old_row,
        "accepted history must survive resume and subsequent polling verbatim"
    );
    mock.shutdown().await;
    peer.close().await;
    assert!(resumed, "resume actually enables before polling: {reply}");
    assert!(
        first <= 20 && posts.len() - first <= 20,
        "retain the existing 20-attempt per-poll budget, not an unbounded resume drain"
    );
    db.close().await.unwrap();
}
