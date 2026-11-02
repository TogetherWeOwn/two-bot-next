//! Protected acceptance tests: bound ticket component admission before
//! spawning and acknowledge overload.
//!
//! PROTECTED FILE: the paired Builder implements the production bound without
//! editing this file. The only wiring outside it is the
//! `#[cfg(test)] mod ticket_component_admission_acceptance_tests;` line in
//! `main.rs`. Return necessary corrections to the Test Author; never edit.
//!
//! Contract under test (all through the real `CommandRuntime::dispatch`
//! gateway path, `TicketRuntime` + `MockRest` doubles, no database):
//!
//! - Admission is finite, non-waiting and happens before a ticket component
//!   task is spawned: with every admitted defer held pending, a burst above
//!   the bound starts at most [`MAX_TICKET_COMPONENT_IN_FLIGHT`] ticket
//!   executions. Rejected interactions are refused synchronously (dispatch
//!   returns `false`), never queued: after the held defers settle, no
//!   rejected interaction starts, defers, edits or mutates.
//! - Per-member fairness holds before spawning: one member holds at most
//!   [`MAX_TICKET_COMPONENT_PER_MEMBER`] in-flight ticket executions, so a
//!   single-member burst cannot fill the runtime. Another member at available
//!   capacity is still admitted.
//! - An otherwise valid component refused for overload gets exactly one
//!   ephemeral, actionable busy/retry acknowledgement (type 4, ephemeral,
//!   names being busy and retrying) on bounded response capacity. No ticket
//!   row, Discord effect beyond that one callback, duplicate callback or
//!   recovery promise is produced for work never admitted.
//! - Admission is held until the handler actually settles and released on
//!   success, failure, cancellation and shutdown, so later work can run. A
//!   different member can still make progress while one member is held.
//! - Ticket authorization, guild fencing, durable close/recovery semantics and
//!   existing button deadlines are preserved. Human-facing normal
//!   confirmations keep their shape; only overload is explained honestly.
//! - Overload exposes bounded machine-identifiable evidence with no
//!   member/token labels, documented with code and next action where on-call
//!   agents look (`docs/tickets.md`). The evidence substring
//!   [`TICKET_COMPONENT_EVIDENCE_SUBSTRING`] appears in the global metrics
//!   render after refusals, and [`TICKET_COMPONENT_SATURATION_CODE`] plus a
//!   next action appear in the ticket runbook.
//!
//! The Builder may pick any global bound in `1..=MAX_TICKET_COMPONENT_IN_FLIGHT`,
//! any per-member bound in `1..=MAX_TICKET_COMPONENT_PER_MEMBER`, and any busy
//! capacity in `1..=MAX_TICKET_COMPONENT_BUSY_IN_FLIGHT`, and must document
//! the chosen bounds and overload behavior in production code and
//! `docs/tickets.md`. Total concurrent reply calls stay within
//! [`MAX_TICKET_COMPONENT_TOTAL_REPLY_IN_FLIGHT`].
//!
//! Bounds mirror the shared router: the interaction lane is 16, the busy lane
//! is 8, per-member fairness is 3. Ticket work is at least as heavy, so the
//! ceilings match rather than exceed.
//!
//! Offline only: lazy pools never connect, `MockRest` doubles Discord. No
//! staging/production database, no live Discord, no migration, no deploy.

use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::incoming::InteractionCreate;
use two_bot_core::tickets::{TicketAction, TICKET_CLAIM_ID, TICKET_CLOSE_ID, TICKET_OPEN_ID};

use crate::command_runtime::CommandRuntime;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::ticket_runtime::{TicketConfig, TicketRuntime};

/// Ceiling on concurrently admitted ticket component executions.
const MAX_TICKET_COMPONENT_IN_FLIGHT: usize = 16;
/// Ceiling on concurrently in-flight overload/busy acknowledgements.
const MAX_TICKET_COMPONENT_BUSY_IN_FLIGHT: usize = 8;
/// Ceiling on concurrent reply-trait calls of any kind.
const MAX_TICKET_COMPONENT_TOTAL_REPLY_IN_FLIGHT: usize = 32;
/// Ceiling on in-flight ticket executions one member may hold.
const MAX_TICKET_COMPONENT_PER_MEMBER: usize = 3;

/// Machine-identifiable evidence substring the Builder exposes in the global
/// metrics render (for example `two_bot_ticket_component_drops_total`).
const TICKET_COMPONENT_EVIDENCE_SUBSTRING: &str = "ticket_component";
/// Saturation code the Builder logs and documents for on-call.
const TICKET_COMPONENT_SATURATION_CODE: &str = "ticket_component_admission_saturated";

const MODE_HOLD_DEFER: u8 = 0;
const MODE_PASS: u8 = 1;
const MODE_FAIL_CATEGORY: u8 = 2;

fn ticket_config() -> TicketConfig {
    TicketConfig {
        guild_id: "100".into(),
        category_id: "200".into(),
        panel_channel_id: "700".into(),
        staff_role_id: "300".into(),
        cooldown_seconds: two_bot_core::tickets::COOLDOWN_SECONDS,
    }
}

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool")
}

fn button(
    action: TicketAction,
    actor: u64,
    interaction_id: u64,
    token: &str,
    roles: Vec<&str>,
    permissions: u64,
) -> Interaction {
    let custom_id = match action {
        TicketAction::Open => TICKET_OPEN_ID,
        TicketAction::Claim => TICKET_CLAIM_ID,
        TicketAction::Close => TICKET_CLOSE_ID,
    };
    serde_json::from_value(json!({
        "application_id": "400",
        "authorizing_integration_owners": {"0": "100"},
        "id": interaction_id.to_string(),
        "token": token,
        "type": 3,
        "version": 1,
        "guild_id": "100",
        "channel": {"id": "600", "type": 0, "name": "ticket-member", "permissions": "0"},
        "data": {"custom_id": custom_id, "component_type": 2},
        "member": {
            "roles": roles,
            "permissions": permissions.to_string(),
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": actor.to_string(), "username": "Member", "discriminator": "0", "avatar": null}
        }
    }))
    .expect("wire-format ticket interaction")
}

fn open_for(member: u64, index: u64) -> Interaction {
    button(
        TicketAction::Open,
        member,
        100_000 + index,
        &format!("ticket-busy-{member}-{index}"),
        vec![],
        0,
    )
}

fn claim_by(member: u64, index: u64, roles: Vec<&str>, permissions: u64) -> Interaction {
    button(
        TicketAction::Claim,
        member,
        200_000 + index,
        &format!("ticket-claim-{member}-{index}"),
        roles,
        permissions,
    )
}

fn interaction_event(interaction: Interaction) -> Event {
    Event::InteractionCreate(Box::new(InteractionCreate(interaction)))
}

fn runtime_with_tickets(mock: &MockRest) -> Arc<CommandRuntime> {
    let pool = lazy_pool();
    let executor = two_bot_discord::ActionExecutor::with_proxy(
        "ticket-test-token".into(),
        Some(mock.origin()),
    )
    .expect("mock executor");
    let tickets = Arc::new(
        TicketRuntime::new(
            pool.clone(),
            executor.clone(),
            ticket_config(),
            std::collections::HashMap::new(),
        )
        .expect("ticket runtime"),
    );
    tickets.set_bot_id(400);
    CommandRuntime::with_tickets(pool, executor, tickets)
}

fn callback_type(request: &RestRequest) -> Option<u64> {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| body.get("type")?.as_u64())
}

fn is_callback(request: &RestRequest) -> bool {
    request.method == "POST" && request.path.ends_with("/callback")
}

fn is_edit(request: &RestRequest) -> bool {
    request.method == "PATCH" && request.path.ends_with("/messages/@original")
}

fn callback_content(request: &RestRequest) -> String {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| {
            body.get("data")?
                .get("content")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn assert_no_recovery_promise(reply: &str, context: &str) {
    let lower = reply.to_lowercase();
    assert!(
        !lower.contains("recover"),
        "{context}: busy refusal must not name recovery for work never admitted: {reply:?}"
    );
    assert!(
        !lower.contains("unfinished"),
        "{context}: busy refusal must not promise to finish work that does not exist: {reply:?}"
    );
    assert!(
        !lower.contains("will retry") && !lower.contains("will be retried"),
        "{context}: busy refusal must not promise an automatic retry: {reply:?}"
    );
}

fn assert_busy_shape(request: &RestRequest, context: &str) {
    assert_eq!(
        callback_type(request),
        Some(4),
        "{context}: overload acknowledgement is an immediate type-4 reply, not a defer"
    );
    let body: Value = serde_json::from_slice(&request.body).expect("busy callback is JSON");
    assert_eq!(
        body["data"]["flags"], 64,
        "{context}: busy acknowledgement is ephemeral"
    );
    let content = callback_content(request);
    let lower = content.to_lowercase();
    assert!(
        lower.contains("busy"),
        "{context}: busy acknowledgement names overload honestly: {content:?}"
    );
    assert!(
        lower.contains("try again") || lower.contains("retry"),
        "{context}: busy acknowledgement is actionable: {content:?}"
    );
    assert_no_recovery_promise(&content, context);
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deadline waiting for {what}"));
}

async fn settle_requests(mock: &MockRest) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut stable = 0;
    let mut last = mock.requests().len();
    while stable < 20 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "mock requests never settled: {}",
            mock.requests().len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
        let now = mock.requests().len();
        if now == last {
            stable += 1;
        } else {
            stable = 0;
            last = now;
        }
    }
}

/// Route-aware double: defers (type 5) are held, busy replies (type 4) pass
/// immediately, everything else fails fast so admitted work cannot wander
/// past the defer into channel or store effects while held.
async fn held_mock(hold: Duration) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        if is_callback(request) {
            match callback_type(request) {
                Some(5) => ScriptedResponse::status(204).delayed(hold),
                Some(4) => ScriptedResponse::status(204),
                _ => ScriptedResponse::status(500),
            }
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await
}

/// Mode-aware double for release tests: hold, pass, or fail the category
/// fetch so admitted work settles (error) and must release its permit.
struct ReleaseMock {
    mock: MockRest,
    mode: Arc<AtomicU8>,
}

impl ReleaseMock {
    async fn start(hold: Duration) -> Self {
        let mode = Arc::new(AtomicU8::new(MODE_HOLD_DEFER));
        let responder_mode = Arc::clone(&mode);
        let mock = MockRest::with_responder(move |request: &RestRequest| {
            if is_callback(request) {
                return match callback_type(request) {
                    Some(5) => match responder_mode.load(Ordering::SeqCst) {
                        MODE_HOLD_DEFER => ScriptedResponse::status(204).delayed(hold),
                        _ => ScriptedResponse::status(204),
                    },
                    Some(4) => ScriptedResponse::status(204),
                    _ => ScriptedResponse::status(500),
                };
            }
            if is_edit(request) {
                return ScriptedResponse::json(200, json!({}));
            }
            // Category and any other REST fail fast (before any store work).
            // Admitted handlers settle as errors and must release permits.
            ScriptedResponse::status(500)
        })
        .await;
        Self { mock, mode }
    }

    fn set_mode(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ticket_component_burst_is_bounded_before_spawned_tasks_can_run() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let runtime = runtime_with_tickets(&mock);
    let _guard = runtime.dispatch_guard();
    let metrics = tokio::runtime::Handle::current().metrics();
    let tasks_before = metrics.num_alive_tasks();

    // No await: a current-thread executor cannot poll any spawned task during
    // this burst. Admission inside a task leaves the entire burst queued, even
    // if reply concurrency looks bounded after a yield.
    let mut admitted = 0;
    let mut refused = 0;
    for index in 0..64 {
        if runtime.dispatch(&interaction_event(open_for(10_000 + index, index))) {
            admitted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(
        (1..=MAX_TICKET_COMPONENT_IN_FLIGHT).contains(&admitted),
        "synchronous burst admitted {admitted} ticket executions, bound is {MAX_TICKET_COMPONENT_IN_FLIGHT}"
    );
    assert_eq!(
        admitted + refused,
        64,
        "every interaction is admitted or refused synchronously, never queued"
    );
    assert!(
        refused >= 64 - MAX_TICKET_COMPONENT_IN_FLIGHT,
        "excess interactions were queued instead of synchronously refused: admitted {admitted}, refused {refused}"
    );
    let spawned = metrics.num_alive_tasks().saturating_sub(tasks_before);
    assert!(
        (1..=MAX_TICKET_COMPONENT_TOTAL_REPLY_IN_FLIGHT).contains(&spawned),
        "synchronous burst queued {spawned} tasks before admission; even with bounded busy replies the ceiling is {MAX_TICKET_COMPONENT_TOTAL_REPLY_IN_FLIGHT}"
    );
    assert!(
        mock.requests().is_empty(),
        "no spawned task could run before the first await"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ticket_component_burst_never_exceeds_global_bound_while_defers_held() {
    let mock = held_mock(Duration::from_secs(30)).await;
    let runtime = runtime_with_tickets(&mock);
    let _guard = runtime.dispatch_guard();

    // Distinct members, so per-member fairness cannot masquerade as the global
    // bound. Sequential dispatch with immediate busy replies keeps the busy
    // lane free; the held defers alone must enforce the bound.
    for index in 0..48 {
        let _ = runtime.dispatch(&interaction_event(open_for(20_000 + index, index)));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle_requests(&mock).await;

    let requests = mock.requests();
    let defers: Vec<_> = requests
        .iter()
        .filter(|r| is_callback(r) && callback_type(r) == Some(5))
        .collect();
    let busy: Vec<_> = requests
        .iter()
        .filter(|r| is_callback(r) && callback_type(r) == Some(4))
        .collect();
    assert!(
        (1..=MAX_TICKET_COMPONENT_IN_FLIGHT).contains(&defers.len()),
        "held burst admitted {} ticket executions, bound is {MAX_TICKET_COMPONENT_IN_FLIGHT}",
        defers.len()
    );
    // Sequential dispatch with immediate busy replies: every refused
    // interaction gets its one busy ack (no drops), so defers + busy == burst
    // for any Builder bound in 1..=16.
    assert_eq!(
        defers.len() + busy.len(),
        48,
        "each interaction gets exactly one callback: an admitted defer or a busy refusal"
    );
    // Every observed callback is either an admitted defer or a busy refusal:
    // no second callback, no edit, no channel effect while held.
    for request in &requests {
        assert!(
            is_callback(request),
            "held burst must not reach edits or channel effects: {} {}",
            request.method,
            request.path
        );
    }
    assert!(
        requests.iter().all(|r| !is_edit(r)),
        "held work cannot complete, refused work never starts: no edits"
    );
    for reply in &busy {
        assert_busy_shape(reply, "global bound");
    }
    // Tokens are distinct per interaction: at most one callback per token, so
    // no duplicate ack for any interaction.
    let mut seen = HashSet::new();
    for request in &requests {
        assert!(
            seen.insert(request.path.clone()),
            "duplicate callback path: {}",
            request.path
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn ticket_component_per_member_fairness_holds_and_other_member_progresses() {
    let mock = held_mock(Duration::from_secs(30)).await;
    let runtime = runtime_with_tickets(&mock);
    let _guard = runtime.dispatch_guard();

    // One member bursts above the per-member cap while the runtime still has
    // global room. Sequential dispatch keeps busy replies sequential.
    for index in 0..8 {
        let _ = runtime.dispatch(&interaction_event(open_for(42, index)));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Another member at available capacity must still make progress.
    let _ = runtime.dispatch(&interaction_event(open_for(43, 100)));
    settle_requests(&mock).await;

    let requests = mock.requests();
    let member_defers = requests
        .iter()
        .filter(|r| r.path.contains("/ticket-busy-42-") && callback_type(r) == Some(5))
        .count();
    assert!(
        (1..=MAX_TICKET_COMPONENT_PER_MEMBER).contains(&member_defers),
        "single-member burst admitted {member_defers} executions, per-member bound is {MAX_TICKET_COMPONENT_PER_MEMBER}"
    );
    let member_busy: Vec<_> = requests
        .iter()
        .filter(|r| r.path.contains("/ticket-busy-42-") && callback_type(r) == Some(4))
        .collect();
    assert!(
        !member_busy.is_empty(),
        "per-member excess gets a busy acknowledgement, not silence"
    );
    assert!(
        member_busy.len() <= MAX_TICKET_COMPONENT_BUSY_IN_FLIGHT,
        "busy acknowledgements stay within bounded response capacity: {}",
        member_busy.len()
    );
    for reply in &member_busy {
        assert_busy_shape(reply, "per-member fairness");
    }
    let other: Vec<_> = requests
        .iter()
        .filter(|r| r.path.contains("/ticket-busy-43-"))
        .collect();
    assert_eq!(
        other.len(),
        1,
        "another member at available capacity is admitted exactly once"
    );
    assert_eq!(
        callback_type(other[0]),
        Some(5),
        "the other member is admitted (defer), not refused"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ticket_component_refused_gets_one_busy_acknowledgement_without_side_effects() {
    let mock = held_mock(Duration::from_secs(30)).await;
    let runtime = runtime_with_tickets(&mock);
    let _guard = runtime.dispatch_guard();

    // Fill the bound with held defers from distinct members. Any Builder
    // bound in 1..=16 fills within the first 16; sequential busy keeps the
    // lane free for the final refusal.
    for index in 0..16 {
        let _ = runtime.dispatch(&interaction_event(open_for(30_000 + index, index)));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    wait_for("bound to fill", || {
        mock.requests()
            .iter()
            .filter(|r| callback_type(r) == Some(5))
            .count()
            >= 1
    })
    .await;
    settle_requests(&mock).await;
    let filled = mock
        .requests()
        .iter()
        .filter(|r| callback_type(r) == Some(5))
        .count();
    assert!(
        (1..=MAX_TICKET_COMPONENT_IN_FLIGHT).contains(&filled),
        "setup must fill the bound with held defers, got {filled}"
    );

    // One more otherwise-valid component from a fresh member.
    let refused = runtime.dispatch(&interaction_event(open_for(99_999, 999)));
    assert!(
        !refused,
        "overload refusal reports false, like the shared interaction lane"
    );
    wait_for("busy callback", || {
        mock.requests()
            .iter()
            .any(|r| r.path.contains("/ticket-busy-99999-999"))
    })
    .await;
    settle_requests(&mock).await;

    let requests = mock.requests();
    let busy: Vec<_> = requests
        .iter()
        .filter(|r| r.path.contains("/ticket-busy-99999-999"))
        .collect();
    assert_eq!(
        busy.len(),
        1,
        "one busy acknowledgement, no duplicate callback"
    );
    assert!(
        busy.len() <= MAX_TICKET_COMPONENT_BUSY_IN_FLIGHT,
        "single refusal fits bounded busy capacity"
    );
    assert_busy_shape(busy[0], "single refusal");
    assert!(
        requests
            .iter()
            .all(|r| !is_edit(r) || !r.path.contains("ticket-busy-99999-999")),
        "refused work never reaches the completion edit"
    );
    // No channel, message or store-driven REST for the refused token: the one
    // callback is the only Discord effect.
    for request in &requests {
        if request.path.contains("ticket-busy-99999-999") {
            assert!(
                is_callback(request),
                "refused interaction has only its busy callback: {} {}",
                request.method,
                request.path
            );
        }
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn ticket_component_permits_return_after_error_so_later_work_runs() {
    let held = ReleaseMock::start(Duration::from_secs(2)).await;
    let runtime = runtime_with_tickets(&held.mock);
    let _guard = runtime.dispatch_guard();

    // Fill the bound with held defers; the bound assertion is the RED gate
    // before any release behavior is reached. Burst 24 exceeds the 16 ceiling
    // so the unbounded path fails here.
    for index in 0..24 {
        let _ = runtime.dispatch(&interaction_event(open_for(40_000 + index, index)));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle_requests(&held.mock).await;
    let admitted = held
        .mock
        .requests()
        .iter()
        .filter(|r| callback_type(r) == Some(5))
        .count();
    assert!(
        (1..=MAX_TICKET_COMPONENT_IN_FLIGHT).contains(&admitted),
        "setup must fill the bound with held defers, got {admitted}"
    );

    // Held defers complete (2 s), admitted handlers then fail fast at the
    // category fetch (500, before any store work) and must release. Later
    // sequential errors each attempt their defer, proving permits returned:
    // far more than one bound's worth of interactions complete.
    held.set_mode(MODE_PASS);
    wait_for("held batch to settle", || {
        held.mock.requests().iter().filter(|r| is_edit(r)).count() >= admitted
    })
    .await;
    let before = held.mock.requests().len();
    held.set_mode(MODE_FAIL_CATEGORY);
    for index in 0..24 {
        let _ = runtime.dispatch(&interaction_event(open_for(50_000 + index, 1_000 + index)));
        // Defer (callback) + category fetch (GET) + completion edit (PATCH).
        wait_for("error interaction to finish", || {
            held.mock.requests().len() >= before + (index as usize + 1) * 3
        })
        .await;
    }
    settle_requests(&held.mock).await;
    let defers = held
        .mock
        .requests()
        .iter()
        .filter(|r| callback_type(r) == Some(5))
        .count();
    assert!(
        defers >= admitted + 24,
        "permits did not return after errors: {defers} defers for {} admitted + 24 sequential",
        admitted
    );
    held.mock.shutdown().await;
}

#[tokio::test]
async fn ticket_component_shutdown_drains_promptly_without_stranding_admission() {
    let mock = held_mock(Duration::from_secs(30)).await;
    let pool = lazy_pool();
    let executor = two_bot_discord::ActionExecutor::with_proxy(
        "ticket-test-token".into(),
        Some(mock.origin()),
    )
    .expect("mock executor");
    let tickets = Arc::new(
        TicketRuntime::new(
            pool.clone(),
            executor.clone(),
            ticket_config(),
            std::collections::HashMap::new(),
        )
        .expect("ticket runtime"),
    );
    tickets.set_bot_id(400);
    let supervisor = tickets.start().expect("ticket supervisor");
    let runtime = CommandRuntime::with_tickets(pool, executor, Arc::clone(&tickets));
    let _guard = runtime.dispatch_guard();

    // Burst 24 exceeds the 16 ceiling so the unbounded path fails the setup
    // bound check (RED) before shutdown is reached.
    for index in 0..24 {
        let _ = runtime.dispatch(&interaction_event(open_for(60_000 + index, index)));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle_requests(&mock).await;
    let admitted = mock
        .requests()
        .iter()
        .filter(|r| callback_type(r) == Some(5))
        .count();
    assert!(
        (1..=MAX_TICKET_COMPONENT_IN_FLIGHT).contains(&admitted),
        "setup must fill the bound with held defers, got {admitted}"
    );

    // Shutdown cancels in-flight ticket work and joins it; it must not wait
    // out the 30 s held defer or the 120 s button deadline.
    tokio::time::timeout(Duration::from_secs(10), supervisor.shutdown())
        .await
        .expect("shutdown drains promptly");
    mock.shutdown().await;
}

#[tokio::test]
async fn ticket_component_preserves_authorization_and_routing() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let runtime = runtime_with_tickets(&mock);
    let _guard = runtime.dispatch_guard();

    // Unauthorized claim: staff-only text, immediate type-4, no defer, no
    // edit, no other effect. Green before and after the bound lands.
    runtime.on_interaction(&claim_by(501, 1, vec![], 0)).await;
    // Foreign guild stays silent through the router fence.
    let mut foreign = open_for(502, 2);
    foreign.guild_id = Some(twilight_model::id::Id::new(999));
    runtime.on_interaction(&foreign).await;
    // Single valid open defers ephemerally exactly once. The lazy pool fails
    // the later store step, so the completion edit carries the existing error
    // text; the defer shape is what this control pins.
    runtime.on_interaction(&open_for(500, 3)).await;

    settle_requests(&mock).await;
    let requests = mock.requests();
    let callbacks: Vec<_> = requests.iter().filter(|r| is_callback(r)).collect();
    assert_eq!(
        callbacks.len(),
        2,
        "unauthorized claim (immediate) + valid open (defer); foreign guild silent"
    );
    let unauthorized = requests
        .iter()
        .find(|r| r.path.contains("/ticket-claim-501-1"))
        .expect("unauthorized callback");
    assert_eq!(callback_type(unauthorized), Some(4));
    let content = callback_content(unauthorized);
    assert_eq!(
        content,
        two_bot_core::tickets::TicketError::StaffOnly.to_string(),
        "domain refusals keep their existing member-visible text"
    );
    let defer = requests
        .iter()
        .find(|r| r.path.contains("/ticket-busy-500-3"))
        .expect("valid open defer");
    assert_eq!(callback_type(defer), Some(5));
    let body: Value = serde_json::from_slice(&defer.body).expect("defer JSON");
    assert_eq!(body["data"]["flags"], 64, "defer stays ephemeral");
    assert!(
        requests
            .iter()
            .all(|r| !r.path.contains("ticket-busy-502-2")),
        "foreign-guild router fence stays silent"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ticket_component_saturated_evidence_is_bounded_and_documented() {
    let mock = held_mock(Duration::from_secs(30)).await;
    let runtime = runtime_with_tickets(&mock);
    let _guard = runtime.dispatch_guard();

    for index in 0..24 {
        let _ = runtime.dispatch(&interaction_event(open_for(70_000 + index, index)));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle_requests(&mock).await;
    let defers = mock
        .requests()
        .iter()
        .filter(|r| callback_type(r) == Some(5))
        .count();
    assert!(
        (1..=MAX_TICKET_COMPONENT_IN_FLIGHT).contains(&defers),
        "setup must saturate the bound, got {defers} defers"
    );

    // Machine-identifiable evidence in the global metrics render. Existence,
    // not an exact count: parallel suites share the global registry.
    let render = two_bot_core::metrics::global().render(None);
    assert!(
        render.contains(TICKET_COMPONENT_EVIDENCE_SUBSTRING),
        "saturation exposes {TICKET_COMPONENT_EVIDENCE_SUBSTRING} evidence in the metrics render"
    );
    for line in render
        .lines()
        .filter(|l| l.contains(TICKET_COMPONENT_EVIDENCE_SUBSTRING))
    {
        assert!(
            !line.contains("70000") && !line.contains("ticket-busy-"),
            "admission evidence carries no member/token labels: {line}"
        );
    }

    // The runbook documents the saturation code and the next action where
    // on-call agents look. `include_str!` keeps this offline and exact.
    let tickets_doc = include_str!("../../../docs/tickets.md");
    assert!(
        tickets_doc.contains(TICKET_COMPONENT_SATURATION_CODE),
        "docs/tickets.md documents {TICKET_COMPONENT_SATURATION_CODE}"
    );
    let lower = tickets_doc.to_lowercase();
    assert!(
        lower.contains("next action") || lower.contains("on-call") || lower.contains("oncall"),
        "docs/tickets.md names the next action for {TICKET_COMPONENT_SATURATION_CODE}"
    );
    mock.shutdown().await;
}
