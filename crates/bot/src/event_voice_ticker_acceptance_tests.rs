#![cfg(test)]
//! Protected acceptance tests: T-15 catch-up ticker + reconcile + per-route
//! rate-limit queue (SPEC card 2 + R1/R2/R5/R13/R14/R22).
//!
//! PROTECTED FILE: the paired builder implements the production tick without
//! editing this file. The only wiring outside it is the `#[path] mod
//! event_voice_ticker_acceptance;` line in `website_jobs_tests.rs`.
//!
//! ## What is pinned
//!
//! One tick core plus job registration:
//!
//! - `NAMES` contains `"event_voice"` (the new supervised job name).
//! - ONE inherent method with EXACTLY this signature:
//!   ```ignore
//!   impl Kind {
//!       pub async fn run_event_voice_tick(
//!           rest: &ActionExecutor,
//!           guild: &str,
//!           events: &[serde_json::Value],
//!           category_id: Option<&str>,
//!           now_ms: i64,
//!           shutdown: &watch::Receiver<bool>,
//!           gate: &JobGate,
//!       ) -> Result<serde_json::Value, ErrorClass>;
//!   }
//!   ```
//!
//! ## RED NOW (baseline)
//!
//! The inherent does not exist yet, so every call below resolves to the
//! test-local `BaselineEventVoiceTick` adapter, which replays today's
//! behavior: no gate check, no REST, and a zeroed outcome
//! (`{counters: zeroed, events: []}`, no `tick_ms`). Every behavior test
//! fails on the missing Discord calls and the missing outcome records;
//! the responder controls and the adapter keep-alive stay green.
//!
//! ## GREEN (builder)
//!
//! Add the inherent method above. Inherent methods shadow trait methods, so
//! these calls flip from the adapter to the wired tick with no test edit.
//! Then point the scheduler's EventVoice closure at it and register the job
//! (`Kind::EventVoice`, `NAMES`, 60 s cadence, `TWO_TEST_EVENT_VOICE_`
//! `INTERVAL_MS` override); that wiring is review-verified.
//!
//! ## The tick core is a pure function of (snapshots, now, REST)
//!
//! - Inputs arrive as caller-built snapshots (schema below):
//!   the 60 s scheduler scans the mirror plus `event_voice_channels`,
//!   builds one snapshot per candidate, calls the core, then persists the
//!   returned bindings. The scan mapping and the persistence are
//!   review-verified (same split as the Events `run_events_tick`, whose
//!   scheduler swap was review-verified); the table card pins the store
//!   semantics and the category card pins the preflight call shape.
//! - The core takes NO pool: it issues no SQL. Cross-tick dedup is the
//!   idempotency insert (table card); the core dedups only within one call.
//!   Sequential identical calls each attempt (see
//!   `sequential_ticks_each_attempt`).
//! - ALL windows, backoffs and buckets derive from the passed `now_ms`
//!   (tick time, UTC millis). The scheduler passes wall-clock; tests pass
//!   scripted time. Wall-clock (`Instant`) buckets or sleeps WILL fail
//!   these tests: a tick at +95 s tick-time runs milliseconds after the
//!   previous tick in wall-time.
//! - 429/backoff state is IN-MEMORY, namespaced per `(guild, route)` for
//!   route buckets and per `(guild, instance)` for the retry floor. The
//!   core never consults any `rate_limited_until` column: cap triage is
//!   recomputed deterministically every tick, and 429 pacing is tick-time
//!   buckets. Every test uses a distinct guild so buckets never leak
//!   across tests; no test needs serial execution.
//! - Route identity distinguishes at least the channel-create route from
//!   the channel-delete route. A non-global 429 pauses ONLY its route in
//!   ONLY its guild; other routes and guilds proceed. Global-429 handling
//!   is builder-owned and unpinned.
//! - The gate is checked FIRST, before any REST: a held gate returns `Ok`
//!   at once with the skip shape and issues zero requests. The scheduler
//!   wrapper MUST NOT take the same gate or every tick self-skips (the
//!   Events-gate precedent documents this hazard).
//! - REST is single-attempt: the core MUST NOT sleep out `Retry-After`
//!   inside a REST call. Every tick call here runs under a 60 s timeout;
//!   a client-side 429 sleep fails as a stall, not a hang.
//! - `category_id` arrives PRE-VALIDATED: `None` means the category gap
//!   (outcome `no_category`, zero REST); `Some` proceeds. The config read
//!   and the per-tick preflight call are the category card's; a 403
//!   surfacing on the create path maps to `perm_denied` here.
//! - Binding health arrives in the snapshot (`binding_deleted`, set by the
//!   scheduler's reconcile GET): the core is NOT required to re-verify
//!   bindings, but the double answers verification GETs faithfully, so
//!   builders that re-check also pass.
//! - `shutdown` is observed between creates on a best-effort basis;
//!   partial outcomes are unpinned and no test fires it.
//! - Transport failures (mock down, broken body): builder's choice,
//!   `Err(ErrorClass::Rest)` recommended; unpinned, no test covers it.
//!
//! ## Input snapshot schema (all tests build these via `base_event`)
//!
//! - `event_id: String` (required)
//! - `name: String` (required; fixtures use already-valid lowercase names
//!   and the create POST MUST carry the name verbatim)
//! - `status: String` (required; `SCHEDULED|ACTIVE|COMPLETED|CANCELLED`)
//! - `entity_type: String` (required; `VOICE|STAGE|EXTERNAL`)
//! - `instance_starts_at_ms: i64` (required; UTC millis)
//! - `attached_channel_id: String?` (Discord's event channel; STAGE names it)
//! - `voice_channel_id: String?` (bot binding from `event_voice_channels`)
//! - `binding_deleted: bool` (default false; scheduler's 404 detection)
//! - `voice_opt_out: bool` (default false; mod-only per-event flag)
//! - `occupant_ids: [String]` (default []; scheduler's live voice view)
//!
//! ## Outcome schema (every shape below is pinned)
//!
//! Normal: `{tick_ms, counters, events}` where `tick_ms` echoes `now_ms`,
//! `counters` holds `{created, rate_limited, perm_denied, no_category,
//! deleted}` (extra keys allowed, these five required), and `events` holds
//! EXACTLY ONE record per input (same order encouraged, matched by id).
//! Recreates count under `created`; give-ups count under `rate_limited`.
//!
//! Record: `{event_id, outcome, reason?, channel_id?}` with outcome in
//! `created|recreated|deleted|rate_limited|perm_denied|no_category|`
//! `give_up|skipped|noop`. Reasons are pinned ONLY for
//! `skipped -> out_of_window|post_start|external|stage|opt_out|`
//! `duplicate_input` and `noop -> bound_healthy|occupied_kept`; every other
//! reason is present-but-builder-worded. `channel_id` rides created,
//! recreated, deleted, and the STAGE passthrough.
//!
//! Skip: `{skipped: true, reason: "gate_held", tick_ms, counters zeroed,
//! events: []}`.
//!
//! ## Decision table pinned here
//!
//! - Create ⟺ `SCHEDULED + VOICE + !opt_out + voice_channel_id none +
//!   starts_at in (now, now+15m]`. The R1 SQL's `-5m` lower bound is scan
//!   scope, not eligibility: post-start NEVER creates (card bullet).
//! - First attempts run to T-0; only RETRIES-after-failure give up at T-2
//!   (`now > starts_at - 2m`, outcome `give_up`).
//! - Far-future ⟺ `starts_at > now + 24h` (strict): bound + empty deletes
//!   once; bound + occupied never deletes (the occupied convert is the
//!   handoff card's; here only the no-DELETE is pinned).
//! - Cap: one tick creates at most 10 per guild, earliest `starts_at`
//!   first, attempts serialized in that order; losers stay `rate_limited`.
//! - Failed attempt (429/5xx/403-on-create) arms a 90 s per-instance
//!   tick-time floor; 429 ALSO pauses its route until
//!   `now + retry_after`. Effective retry: the later of the two.
//! - Pre-start reconcile: healthy binding is a noop; occupied is never
//!   deleted; mod-deleted + SCHEDULED + pre-start recreates exactly once
//!   per tick; mod-deleted post-start never recreates (no shape pinned:
//!   the handoff card owns post-start records).
//! - Non-SCHEDULED statuses never POST a create. Their record outcomes are
//!   UNPINNED (presence only): the handoff card owns end-path records and
//!   must not break this file.
//! - No PATCH/PUT anywhere in this suite: the core never renames (a mod
//!   rename is never fought) and never re-permissions pre-start.
//!
//! ## Seams this file depends on (builder: preserve them)
//!
//! `Kind`, `NAMES`; `crate::jobs::ErrorClass`;
//! `ActionExecutor::with_proxy_and_guard`;
//! `RateLimitGuard::new` + `GuardConfig::default`;
//! `MockRest::with_responder`, `requests`, `shutdown`, `origin`;
//! `ScriptedResponse::{json, status, rate_limited}`; `RestRequest`
//! `{method, path, body}`; `JobGate::try_acquire` (frozen);
//! `crate::gateway::ensure_crypto_provider`.

use super::super::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use two_bot_core::JobGate;
use two_bot_discord::ratelimit_guard::{GuardConfig, RateLimitGuard};
use two_bot_discord::ActionExecutor;

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::jobs::ErrorClass;

// ---------------------------------------------------------------------------
// Baseline adapter: today's unwired behavior (no gate check, no REST, zeroed
// outcome without `tick_ms`). The builder's inherent method shadows it.
// ---------------------------------------------------------------------------

trait BaselineEventVoiceTick {
    async fn run_event_voice_tick(
        rest: &ActionExecutor,
        guild: &str,
        events: &[Value],
        category_id: Option<&str>,
        now_ms: i64,
        shutdown: &watch::Receiver<bool>,
        gate: &JobGate,
    ) -> Result<Value, ErrorClass>;
}

impl BaselineEventVoiceTick for Kind {
    async fn run_event_voice_tick(
        _rest: &ActionExecutor,
        _guild: &str,
        _events: &[Value],
        _category_id: Option<&str>,
        _now_ms: i64,
        _shutdown: &watch::Receiver<bool>,
        _gate: &JobGate,
    ) -> Result<Value, ErrorClass> {
        Ok(json!({
            "counters": {
                "created": 0,
                "rate_limited": 0,
                "perm_denied": 0,
                "no_category": 0,
                "deleted": 0,
            },
            "events": [],
        }))
    }
}

// ---------------------------------------------------------------------------
// Harness: scripted fake Discord + tick driver.
// ---------------------------------------------------------------------------

/// 2026-10-22T00:00:00.000Z in UTC millis: the fixed tick-time base every
/// test derives event starts from. Wall-clock never enters a window.
const BASE_NOW_MS: i64 = 1_792_627_200_000;
const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

/// Every tick call runs under this budget: a core that sleeps out
/// `Retry-After` (or any other stall) fails here instead of hanging the
/// test binary, which has no default timeout.
const TICK_TIMEOUT: Duration = Duration::from_secs(60);

/// Distinct guild/category/event ids per test keep the core's in-memory
/// buckets isolated without serial execution.
static NEXT_ID: AtomicU64 = AtomicU64::new(7_100_000);

fn fresh_guild() -> String {
    NEXT_ID.fetch_add(1, Ordering::SeqCst).to_string()
}

fn fresh_category() -> String {
    format!("8{}", NEXT_ID.fetch_add(1, Ordering::SeqCst))
}

fn fresh_event_id() -> String {
    format!("9{}", NEXT_ID.fetch_add(1, Ordering::SeqCst))
}

/// One input snapshot with valid defaults (VOICE SCHEDULED at +10m).
/// Mutate the returned `Value` for the case under test.
fn base_event(now_ms: i64) -> Value {
    let id = fresh_event_id();
    json!({
        "event_id": id,
        "name": format!("event-{id}"),
        "status": "SCHEDULED",
        "entity_type": "VOICE",
        "instance_starts_at_ms": now_ms + 10 * MINUTE_MS,
        "attached_channel_id": Value::Null,
        "voice_channel_id": Value::Null,
        "binding_deleted": false,
        "voice_opt_out": false,
        "occupant_ids": [],
    })
}

fn executor_for(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy_and_guard(
        "event-voice-ticker-fixture-token".to_owned(),
        Some(mock.origin()),
        Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("fixture guard")),
    )
    .expect("executor builds against the mock")
}

/// Scripted create-POST outcomes, popped in order; an empty queue creates.
enum CreateScript {
    RateLimited { retry_after_secs: u64 },
    Forbidden,
    ServerError,
}

/// Scripted channel-DELETE outcomes, popped in order; empty deletes.
enum DeleteScript {
    RateLimited { retry_after_secs: u64 },
}

struct FakeDiscord {
    next_channel: u64,
    live: HashMap<String, Value>,
    mod_deleted: HashSet<String>,
    create_script: VecDeque<CreateScript>,
    delete_script: VecDeque<DeleteScript>,
}

impl FakeDiscord {
    fn new() -> Self {
        Self {
            next_channel: 81_000_000,
            live: HashMap::new(),
            mod_deleted: HashSet::new(),
            create_script: VecDeque::new(),
            delete_script: VecDeque::new(),
        }
    }

    /// A pre-existing bound channel the double serves 200 for.
    fn preset_live(&mut self, channel_id: &str, guild: &str, category: &str) {
        self.live.insert(
            channel_id.to_owned(),
            json!({
                "id": channel_id,
                "type": 2,
                "guild_id": guild,
                "name": format!("event-{channel_id}"),
                "parent_id": category,
            }),
        );
    }

    fn preset_mod_deleted(&mut self, channel_id: &str) {
        self.live.remove(channel_id);
        self.mod_deleted.insert(channel_id.to_owned());
    }
}

fn channel_id_from_path(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let (_, after) = path.split_once("/channels/")?;
    let id = after.split('/').next().unwrap_or("").trim();
    (!id.is_empty()).then(|| id.to_owned())
}

fn guild_from_path(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    segments
        .iter()
        .position(|s| *s == "guilds")
        .and_then(|i| segments.get(i + 1))
        .map(|s| s.split('?').next().unwrap_or(s).to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0".to_owned())
}

fn is_create_post(request: &RestRequest) -> bool {
    request.method == "POST"
        && request.path.contains("/guilds/")
        && request.path.contains("/channels")
        && channel_id_from_path(&request.path).is_none()
}

fn is_single_get(request: &RestRequest) -> bool {
    request.method == "GET" && channel_id_from_path(&request.path).is_some()
}

fn is_delete(request: &RestRequest) -> bool {
    request.method == "DELETE" && channel_id_from_path(&request.path).is_some()
}

fn respond(state: &Arc<Mutex<FakeDiscord>>, request: &RestRequest) -> ScriptedResponse {
    let mut fake = state.lock().expect("fake discord");
    if is_create_post(request) {
        match fake.create_script.pop_front() {
            Some(CreateScript::RateLimited { retry_after_secs }) => {
                return ScriptedResponse::rate_limited(
                    retry_after_secs as f64,
                    &retry_after_secs.to_string(),
                );
            }
            Some(CreateScript::Forbidden) => {
                return ScriptedResponse::json(
                    403,
                    json!({"message": "Missing Permissions", "code": 50001}),
                );
            }
            Some(CreateScript::ServerError) => {
                return ScriptedResponse::json(500, json!({}));
            }
            None => {}
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let id = fake.next_channel.to_string();
        fake.next_channel += 1;
        let channel = json!({
            "id": id,
            "type": 2,
            "guild_id": guild_from_path(&request.path),
            "name": body.get("name").cloned().unwrap_or(Value::Null),
            "parent_id": body.get("parent_id").cloned().unwrap_or(Value::Null),
        });
        fake.live.insert(id, channel.clone());
        return ScriptedResponse::json(201, channel);
    }
    if is_delete(request) {
        let id = channel_id_from_path(&request.path).expect("delete id");
        match fake.delete_script.pop_front() {
            Some(DeleteScript::RateLimited { retry_after_secs }) => {
                return ScriptedResponse::rate_limited(
                    retry_after_secs as f64,
                    &retry_after_secs.to_string(),
                );
            }
            None => {}
        }
        if fake.live.remove(&id).is_some() {
            return ScriptedResponse::status(204);
        }
        return ScriptedResponse::json(404, json!({"message": "Unknown Channel", "code": 10003}));
    }
    if is_single_get(request) {
        let id = channel_id_from_path(&request.path).expect("get id");
        if let Some(channel) = fake.live.get(&id) {
            return ScriptedResponse::json(200, channel.clone());
        }
        return ScriptedResponse::json(404, json!({"message": "Unknown Channel", "code": 10003}));
    }
    if request.method == "GET" && request.path.contains("/channels") {
        // Lenient channel-list read for builders that reconcile in bulk.
        let channels: Vec<Value> = fake.live.values().cloned().collect();
        return ScriptedResponse::json(200, Value::Array(channels));
    }
    // Fail closed on anything else: an unexpected route or verb is a loud
    // 404, never a silent pass.
    ScriptedResponse::json(404, json!({"message": "Unknown Channel", "code": 10003}))
}

/// Start the double; returns the mock plus the state handle for presets.
async fn start_double() -> (MockRest, Arc<Mutex<FakeDiscord>>) {
    let state = Arc::new(Mutex::new(FakeDiscord::new()));
    let responder_state = Arc::clone(&state);
    let mock = MockRest::with_responder(move |request| respond(&responder_state, request)).await;
    (mock, state)
}

/// One tick call with a fresh executor AND fresh transport guard: cross-tick
/// pacing is the core's tick-time buckets, never transport-learned
/// wall-clock deadlines.
async fn tick_with_gate(
    mock: &MockRest,
    guild: &str,
    events: &[Value],
    category_id: Option<&str>,
    now_ms: i64,
    gate: &JobGate,
) -> Result<Value, ErrorClass> {
    let rest = executor_for(mock);
    let (_stop, shutdown) = watch::channel(false);
    tokio::time::timeout(
        TICK_TIMEOUT,
        Kind::run_event_voice_tick(&rest, guild, events, category_id, now_ms, &shutdown, gate),
    )
    .await
    .expect("tick must not stall: REST is single-attempt, never a Retry-After sleep")
}

async fn tick(
    mock: &MockRest,
    guild: &str,
    events: &[Value],
    category_id: Option<&str>,
    now_ms: i64,
) -> Result<Value, ErrorClass> {
    let gate = JobGate::default();
    tick_with_gate(mock, guild, events, category_id, now_ms, &gate).await
}

// ---------------------------------------------------------------------------
// Assertion helpers (panic messages carry the full outcome for RED triage).
// ---------------------------------------------------------------------------

fn create_posts(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests.iter().filter(|r| is_create_post(r)).collect()
}

fn deletes(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests.iter().filter(|r| is_delete(r)).collect()
}

fn patches_and_puts(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| r.method == "PATCH" || r.method == "PUT")
        .collect()
}

fn post_bodies(posts: &[&RestRequest]) -> Vec<Value> {
    posts
        .iter()
        .map(|r| serde_json::from_slice(&r.body).expect("create body is JSON"))
        .collect()
}

fn counter(outcome: &Value, name: &str) -> u64 {
    outcome
        .get("counters")
        .and_then(|c| c.get(name))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("outcome lacks counters.{name}: {outcome}"))
}

fn record_for<'a>(outcome: &'a Value, event_id: &str) -> &'a Value {
    outcome
        .get("events")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("outcome lacks events array: {outcome}"))
        .iter()
        .find(|r| r.get("event_id").and_then(Value::as_str) == Some(event_id))
        .unwrap_or_else(|| panic!("outcome lacks record for {event_id}: {outcome}"))
}

fn record_outcome(record: &Value) -> &str {
    record
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("record lacks outcome: {record}"))
}

fn record_reason(record: &Value) -> &str {
    record
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("record lacks reason: {record}"))
}

fn assert_tick_clock(outcome: &Value, now_ms: i64) {
    assert_eq!(
        outcome.get("tick_ms").and_then(Value::as_i64),
        Some(now_ms),
        "outcome must echo the passed tick clock: {outcome}"
    );
}

// ---------------------------------------------------------------------------
// Responder controls (prove the double; green in RED and GREEN).
// ---------------------------------------------------------------------------

#[test]
fn responder_scripts_create_then_429_then_forbidden() {
    let state = Arc::new(Mutex::new(FakeDiscord::new()));
    {
        let mut fake = state.lock().expect("fake");
        fake.create_script.push_back(CreateScript::RateLimited {
            retry_after_secs: 120,
        });
        fake.create_script.push_back(CreateScript::Forbidden);
    }
    let post = |body: Value| RestRequest {
        method: "POST".to_owned(),
        path: "/api/v10/guilds/7100001/channels".to_owned(),
        headers: Vec::new(),
        body: body.to_string().into_bytes(),
        received_at: tokio::time::Instant::now(),
    };
    // Queue order: 429, 403, then the default create.
    let first = respond(
        &state,
        &post(json!({"name": "a", "type": 2, "parent_id": "1"})),
    );
    assert_eq!(first.status, 429);
    assert!(first
        .headers
        .iter()
        .any(|(k, v)| k == "retry-after" && v == "120"));
    let second = respond(
        &state,
        &post(json!({"name": "b", "type": 2, "parent_id": "1"})),
    );
    assert_eq!(second.status, 403);
    let third = respond(
        &state,
        &post(json!({"name": "c", "type": 2, "parent_id": "7"})),
    );
    assert_eq!(third.status, 201);
    let channel: Value = serde_json::from_slice(&third.body).expect("channel JSON");
    assert_eq!(channel.get("name").and_then(Value::as_str), Some("c"));
    assert_eq!(channel.get("parent_id").and_then(Value::as_str), Some("7"));
}

#[test]
fn responder_remembers_created_channels_for_reverify_gets() {
    let state = Arc::new(Mutex::new(FakeDiscord::new()));
    {
        state.lock().expect("fake").preset_mod_deleted("8800001");
    }
    let get = |id: &str| RestRequest {
        method: "GET".to_owned(),
        path: format!("/api/v10/channels/{id}"),
        headers: Vec::new(),
        body: Vec::new(),
        received_at: tokio::time::Instant::now(),
    };
    let post = RestRequest {
        method: "POST".to_owned(),
        path: "/api/v10/guilds/7100001/channels".to_owned(),
        headers: Vec::new(),
        body: json!({"name": "n", "type": 2, "parent_id": "1"})
            .to_string()
            .into_bytes(),
        received_at: tokio::time::Instant::now(),
    };
    let created = respond(&state, &post);
    assert_eq!(created.status, 201);
    let channel: Value = serde_json::from_slice(&created.body).expect("channel JSON");
    let id = channel
        .get("id")
        .and_then(Value::as_str)
        .expect("id")
        .to_owned();
    // A builder that re-verifies after create sees its channel alive ...
    assert_eq!(respond(&state, &get(&id)).status, 200);
    // ... while a mod-deleted binding 404s.
    assert_eq!(respond(&state, &get("8800001")).status, 404);
}

// ---------------------------------------------------------------------------
// A. Registration.
// ---------------------------------------------------------------------------

#[test]
fn job_registers_event_voice_name() {
    assert!(
        NAMES.contains(&"event_voice"),
        "the new supervised job must register its name next to {NAMES:?}"
    );
}

// ---------------------------------------------------------------------------
// B. Single-flight.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn held_gate_skips_tick_without_rest() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let gate = JobGate::default();
    let _held = gate.try_acquire().expect("test holds the gate");

    let outcome = tick_with_gate(
        &mock,
        &guild,
        std::slice::from_ref(&base_event(now_ms)),
        Some(category.as_str()),
        now_ms,
        &gate,
    )
    .await
    .expect("a skipped tick still returns Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        requests.is_empty(),
        "held gate must issue zero REST: {requests:?}"
    );
    assert_eq!(
        outcome.get("skipped"),
        Some(&json!(true)),
        "skip shape: {outcome}"
    );
    assert_eq!(
        outcome.get("reason").and_then(Value::as_str),
        Some("gate_held"),
        "skip shape: {outcome}"
    );
    assert_tick_clock(&outcome, now_ms);
    for name in [
        "created",
        "rate_limited",
        "perm_denied",
        "no_category",
        "deleted",
    ] {
        assert_eq!(counter(&outcome, name), 0, "skip zeroes {name}: {outcome}");
    }
    assert_eq!(
        outcome
            .get("events")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0),
        "skip records no events: {outcome}"
    );
}

#[tokio::test]
async fn sequential_ticks_each_attempt() {
    // The core is stateless across calls: identical inputs attempt again.
    // Cross-tick dedup is the table layer's idempotency insert, not this core.
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let event = base_event(now_ms);

    let first = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await;
    let second = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await;
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(first.is_ok(), "first tick Ok: {first:?}");
    assert!(second.is_ok(), "second tick Ok: {second:?}");
    assert_eq!(
        create_posts(&requests).len(),
        2,
        "each call attempts: {requests:?}"
    );
}

// ---------------------------------------------------------------------------
// C. Catch-up window.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creates_inside_catch_up_window() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let mut event = base_event(now_ms);
    event["instance_starts_at_ms"] = json!(now_ms + 10 * MINUTE_MS);
    let event_id = event["event_id"].as_str().expect("id").to_owned();
    let name = event["name"].as_str().expect("name").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    let posts = create_posts(&requests);
    assert_eq!(posts.len(), 1, "one create inside the window: {requests:?}");
    assert!(
        patches_and_puts(&requests).is_empty(),
        "never renames: {requests:?}"
    );
    let bodies = post_bodies(&posts);
    assert_eq!(
        bodies[0].get("name").and_then(Value::as_str),
        Some(name.as_str())
    );
    assert_eq!(bodies[0].get("type").and_then(Value::as_u64), Some(2));
    assert_eq!(
        bodies[0].get("parent_id").and_then(Value::as_str),
        Some(category.as_str())
    );
    assert_tick_clock(&outcome, now_ms);
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "created", "{outcome}");
    assert!(
        record.get("channel_id").and_then(Value::as_str).is_some(),
        "{outcome}"
    );
    assert_eq!(counter(&outcome, "created"), 1, "{outcome}");
}

#[tokio::test]
async fn ninety_second_tick_gap_still_creates() {
    // R1: a late tick (job restart, single-flight skip) must catch up.
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let starts_at = BASE_NOW_MS + 16 * MINUTE_MS;
    let mut event = base_event(BASE_NOW_MS);
    event["instance_starts_at_ms"] = json!(starts_at);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let before = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        BASE_NOW_MS,
    )
    .await
    .expect("tick1 Ok");
    assert_eq!(
        create_posts(&mock.requests()).len(),
        0,
        "T-16 is outside the window"
    );
    let after = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        BASE_NOW_MS + 90_000,
    )
    .await
    .expect("tick2 Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        1,
        "90 s gap still creates: {requests:?}"
    );
    assert_eq!(record_outcome(record_for(&before, &event_id)), "skipped");
    assert_eq!(
        record_reason(record_for(&before, &event_id)),
        "out_of_window"
    );
    assert_eq!(record_outcome(record_for(&after, &event_id)), "created");
}

#[tokio::test]
async fn late_create_from_t14_to_t0() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let offsets = [
        14 * MINUTE_MS,
        10 * MINUTE_MS,
        5 * MINUTE_MS,
        MINUTE_MS,
        1_000,
    ];
    let mut events = Vec::new();
    for offset in offsets {
        let mut event = base_event(now_ms);
        event["instance_starts_at_ms"] = json!(now_ms + offset);
        events.push(event);
    }

    let outcome = tick(&mock, &guild, &events, Some(category.as_str()), now_ms)
        .await
        .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        5,
        "T-14..T-0 all create: {requests:?}"
    );
    assert_eq!(counter(&outcome, "created"), 5, "{outcome}");
    for event in &events {
        let id = event["event_id"].as_str().expect("id");
        assert_eq!(
            record_outcome(record_for(&outcome, id)),
            "created",
            "{outcome}"
        );
    }
}

#[tokio::test]
async fn window_edges_create_up_to_t15_exclusive_of_now() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let mut at_edge = base_event(now_ms);
    at_edge["instance_starts_at_ms"] = json!(now_ms + 15 * MINUTE_MS);
    let mut past_edge = base_event(now_ms);
    past_edge["instance_starts_at_ms"] = json!(now_ms + 15 * MINUTE_MS + 1);
    let mut at_now = base_event(now_ms);
    at_now["instance_starts_at_ms"] = json!(now_ms);
    let mut past_start = base_event(now_ms);
    past_start["instance_starts_at_ms"] = json!(now_ms - 1_000);
    let events = [at_edge, past_edge, at_now, past_start];
    let ids: Vec<String> = events
        .iter()
        .map(|e| e["event_id"].as_str().expect("id").to_owned())
        .collect();

    let outcome = tick(&mock, &guild, &events, Some(category.as_str()), now_ms)
        .await
        .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        1,
        "only T-15 exactly: {requests:?}"
    );
    assert_eq!(record_outcome(record_for(&outcome, &ids[0])), "created");
    assert_eq!(record_outcome(record_for(&outcome, &ids[1])), "skipped");
    assert_eq!(
        record_reason(record_for(&outcome, &ids[1])),
        "out_of_window"
    );
    assert_eq!(record_outcome(record_for(&outcome, &ids[2])), "skipped");
    assert_eq!(record_reason(record_for(&outcome, &ids[2])), "post_start");
    assert_eq!(record_outcome(record_for(&outcome, &ids[3])), "skipped");
    assert_eq!(record_reason(record_for(&outcome, &ids[3])), "post_start");
}

#[tokio::test]
async fn post_start_stays_tbd() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let mut event = base_event(now_ms);
    event["instance_starts_at_ms"] = json!(now_ms - 30_000);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        create_posts(&requests).is_empty(),
        "post-start never creates: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "skipped", "{outcome}");
    assert_eq!(record_reason(record), "post_start", "{outcome}");
    assert_eq!(counter(&outcome, "created"), 0, "{outcome}");
}

// ---------------------------------------------------------------------------
// D. Eligibility.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn external_never_creates() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let mut event = base_event(now_ms);
    event["entity_type"] = json!("EXTERNAL");
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        create_posts(&requests).is_empty(),
        "EXTERNAL never creates: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "skipped", "{outcome}");
    assert_eq!(record_reason(record), "external", "{outcome}");
}

#[tokio::test]
async fn stage_never_creates_voice_channel() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let mut event = base_event(now_ms);
    event["entity_type"] = json!("STAGE");
    event["attached_channel_id"] = json!("7700001");
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        create_posts(&requests).is_empty(),
        "STAGE never creates voice: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "skipped", "{outcome}");
    assert_eq!(record_reason(record), "stage", "{outcome}");
    assert_eq!(
        record.get("channel_id").and_then(Value::as_str),
        Some("7700001"),
        "STAGE passes its named channel through: {outcome}"
    );
}

#[tokio::test]
async fn opt_out_suppresses_create() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let mut event = base_event(now_ms);
    event["voice_opt_out"] = json!(true);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        create_posts(&requests).is_empty(),
        "opt-out suppresses: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "skipped", "{outcome}");
    assert_eq!(record_reason(record), "opt_out", "{outcome}");
}

#[tokio::test]
async fn non_scheduled_statuses_never_create() {
    // Record outcomes deliberately unpinned: the handoff card owns end-path
    // records and must not break this file. Only the no-create is pinned.
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let mut events = Vec::new();
    for status in ["ACTIVE", "COMPLETED", "CANCELLED"] {
        let mut event = base_event(now_ms);
        event["status"] = json!(status);
        events.push(event);
    }

    let outcome = tick(&mock, &guild, &events, Some(category.as_str()), now_ms)
        .await
        .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        create_posts(&requests).is_empty(),
        "non-SCHEDULED never creates: {requests:?}"
    );
    for event in &events {
        let id = event["event_id"].as_str().expect("id");
        record_for(&outcome, id);
    }
}

// ---------------------------------------------------------------------------
// E. Per-route 429 buckets + retry pacing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_429_pauses_only_the_create_route() {
    // A 429 on creates must not block the same tick's deletes.
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800001";
    {
        let mut fake = state.lock().expect("fake");
        fake.preset_live(binding, &guild, &category);
        fake.create_script.push_back(CreateScript::RateLimited {
            retry_after_secs: 120,
        });
    }
    let create_event = base_event(now_ms);
    let create_id = create_event["event_id"].as_str().expect("id").to_owned();
    let mut move_event = base_event(now_ms);
    move_event["instance_starts_at_ms"] = json!(now_ms + 25 * HOUR_MS);
    move_event["voice_channel_id"] = json!(binding);
    let move_id = move_event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        &[create_event, move_event],
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        1,
        "one create attempt: {requests:?}"
    );
    assert_eq!(
        deletes(&requests).len(),
        1,
        "the delete route proceeds: {requests:?}"
    );
    assert_eq!(
        record_outcome(record_for(&outcome, &create_id)),
        "rate_limited"
    );
    assert_eq!(record_outcome(record_for(&outcome, &move_id)), "deleted");
}

#[tokio::test]
async fn delete_429_pauses_only_the_delete_route() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800002";
    {
        let mut fake = state.lock().expect("fake");
        fake.preset_live(binding, &guild, &category);
        fake.delete_script.push_back(DeleteScript::RateLimited {
            retry_after_secs: 120,
        });
    }
    let mut move_event = base_event(now_ms);
    move_event["instance_starts_at_ms"] = json!(now_ms + 25 * HOUR_MS);
    move_event["voice_channel_id"] = json!(binding);
    let create_event = base_event(now_ms);
    let create_id = create_event["event_id"].as_str().expect("id").to_owned();

    // Tick 1: the delete 429s; the create proceeds on its own route.
    let first = tick(
        &mock,
        &guild,
        &[move_event.clone(), create_event],
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick1 Ok");
    assert_eq!(create_posts(&mock.requests()).len(), 1, "create proceeds");
    assert_eq!(deletes(&mock.requests()).len(), 1, "delete attempted once");
    assert_eq!(record_outcome(record_for(&first, &create_id)), "created");
    // Tick 2 (+95 s tick-time): the delete route is still paused.
    let second = tick(
        &mock,
        &guild,
        std::slice::from_ref(&move_event),
        Some(category.as_str()),
        now_ms + 95_000,
    )
    .await
    .expect("tick2 Ok");
    assert_eq!(
        deletes(&mock.requests()).len(),
        1,
        "delete route still paused"
    );
    assert_eq!(
        record_outcome(record_for(
            &second,
            move_event["event_id"].as_str().expect("id")
        )),
        "rate_limited"
    );
    // Tick 3 (+125 s): the route resumes and the delete lands.
    let third = tick(
        &mock,
        &guild,
        std::slice::from_ref(&move_event),
        Some(category.as_str()),
        now_ms + 125_000,
    )
    .await
    .expect("tick3 Ok");
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        deletes(&requests).len(),
        2,
        "delete retries after pause: {requests:?}"
    );
    assert_eq!(
        record_outcome(record_for(
            &third,
            move_event["event_id"].as_str().expect("id")
        )),
        "deleted"
    );
}

#[tokio::test]
async fn create_retry_paces_one_per_90s_per_instance() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    {
        state
            .lock()
            .expect("fake")
            .create_script
            .push_back(CreateScript::RateLimited {
                retry_after_secs: 60,
            });
    }
    let event = base_event(now_ms);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let first = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick1 Ok");
    assert_eq!(create_posts(&mock.requests()).len(), 1);
    assert_eq!(
        record_outcome(record_for(&first, &event_id)),
        "rate_limited"
    );
    assert_eq!(counter(&first, "rate_limited"), 1, "{first}");

    let second = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms + 30_000,
    )
    .await
    .expect("tick2 Ok");
    assert_eq!(
        create_posts(&mock.requests()).len(),
        1,
        "no retry within 90 s"
    );
    assert_eq!(
        record_outcome(record_for(&second, &event_id)),
        "rate_limited"
    );

    let third = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms + 95_000,
    )
    .await
    .expect("tick3 Ok");
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        create_posts(&requests).len(),
        2,
        "retry after 90 s: {requests:?}"
    );
    assert_eq!(record_outcome(record_for(&third, &event_id)), "created");
}

#[tokio::test]
async fn route_pause_holds_other_instances_on_that_route() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    {
        state
            .lock()
            .expect("fake")
            .create_script
            .push_back(CreateScript::RateLimited {
                retry_after_secs: 120,
            });
    }
    let first_event = base_event(now_ms);
    let first_id = first_event["event_id"].as_str().expect("id").to_owned();
    let second_event = base_event(now_ms);
    let second_id = second_event["event_id"].as_str().expect("id").to_owned();

    tick(
        &mock,
        &guild,
        std::slice::from_ref(&first_event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick1 Ok");
    assert_eq!(create_posts(&mock.requests()).len(), 1);

    // +95 s: the 120 s route pause holds even a never-seen instance.
    let held = tick(
        &mock,
        &guild,
        &[first_event, second_event.clone()],
        Some(category.as_str()),
        now_ms + 95_000,
    )
    .await
    .expect("tick2 Ok");
    assert_eq!(
        create_posts(&mock.requests()).len(),
        1,
        "route pause holds all instances"
    );
    assert_eq!(record_outcome(record_for(&held, &first_id)), "rate_limited");
    assert_eq!(
        record_outcome(record_for(&held, &second_id)),
        "rate_limited"
    );

    // +125 s: the route resumes for both.
    let resumed = tick(
        &mock,
        &guild,
        std::slice::from_ref(&second_event),
        Some(category.as_str()),
        now_ms + 125_000,
    )
    .await
    .expect("tick3 Ok");
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        create_posts(&requests).len(),
        2,
        "route resumes: {requests:?}"
    );
    assert_eq!(record_outcome(record_for(&resumed, &second_id)), "created");
}

#[tokio::test]
async fn other_guild_unaffected_by_route_pause() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let paused_guild = fresh_guild();
    let fresh = fresh_guild();
    let category = fresh_category();
    {
        state
            .lock()
            .expect("fake")
            .create_script
            .push_back(CreateScript::RateLimited {
                retry_after_secs: 120,
            });
    }
    // Guild A 429s its create route ...
    tick(
        &mock,
        &paused_guild,
        std::slice::from_ref(&base_event(now_ms)),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick1 Ok");
    assert_eq!(create_posts(&mock.requests()).len(), 1);
    // ... while guild B creates on the same route without waiting.
    let other = base_event(now_ms);
    let other_id = other["event_id"].as_str().expect("id").to_owned();
    let outcome = tick(
        &mock,
        &fresh,
        std::slice::from_ref(&other),
        Some(category.as_str()),
        now_ms + 1_000,
    )
    .await
    .expect("tick2 Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        2,
        "buckets are per-guild: {requests:?}"
    );
    assert_eq!(record_outcome(record_for(&outcome, &other_id)), "created");
}

#[tokio::test]
async fn give_up_at_t2_after_failure() {
    // A failed instance stops retrying past T-2 even with buckets clear,
    // while a first attempt that late still creates (late-create to T-0).
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    {
        state
            .lock()
            .expect("fake")
            .create_script
            .push_back(CreateScript::RateLimited {
                retry_after_secs: 60,
            });
    }
    let starts_at = BASE_NOW_MS + 10 * MINUTE_MS;
    let mut failed = base_event(BASE_NOW_MS);
    failed["instance_starts_at_ms"] = json!(starts_at);
    let failed_id = failed["event_id"].as_str().expect("id").to_owned();

    tick(
        &mock,
        &guild,
        std::slice::from_ref(&failed),
        Some(category.as_str()),
        BASE_NOW_MS,
    )
    .await
    .expect("tick1 Ok");
    assert_eq!(create_posts(&mock.requests()).len(), 1);

    // +9m tick-time: buckets long expired, but T-1 is past T-2.
    let gave_up = tick(
        &mock,
        &guild,
        std::slice::from_ref(&failed),
        Some(category.as_str()),
        starts_at - MINUTE_MS,
    )
    .await
    .expect("tick2 Ok");
    assert_eq!(create_posts(&mock.requests()).len(), 1, "no retry past T-2");
    assert_eq!(record_outcome(record_for(&gave_up, &failed_id)), "give_up");
    assert_eq!(counter(&gave_up, "rate_limited"), 1, "{gave_up}");

    // A first attempt at T-1 still creates.
    let mut fresh = base_event(starts_at - MINUTE_MS);
    fresh["instance_starts_at_ms"] = json!(starts_at);
    let fresh_id = fresh["event_id"].as_str().expect("id").to_owned();
    let created = tick(
        &mock,
        &guild,
        std::slice::from_ref(&fresh),
        Some(category.as_str()),
        starts_at - MINUTE_MS,
    )
    .await
    .expect("tick3 Ok");
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        create_posts(&requests).len(),
        2,
        "first attempts run to T-0: {requests:?}"
    );
    assert_eq!(record_outcome(record_for(&created, &fresh_id)), "created");
}

#[tokio::test]
async fn server_error_arms_instance_floor() {
    // A 500 is retried with the same 90 s per-instance pacing; whether it
    // also pauses the route is builder-owned and unpinned.
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    {
        state
            .lock()
            .expect("fake")
            .create_script
            .push_back(CreateScript::ServerError);
    }
    let event = base_event(now_ms);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let first = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick1 Ok: a 500 never crashes the tick");
    assert_eq!(
        record_outcome(record_for(&first, &event_id)),
        "rate_limited"
    );

    tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms + 30_000,
    )
    .await
    .expect("tick2 Ok");
    assert_eq!(
        create_posts(&mock.requests()).len(),
        1,
        "floor holds at +30 s"
    );

    let third = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms + 95_000,
    )
    .await
    .expect("tick3 Ok");
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        create_posts(&requests).len(),
        2,
        "recovered after 90 s: {requests:?}"
    );
    assert_eq!(record_outcome(record_for(&third, &event_id)), "created");
}

// ---------------------------------------------------------------------------
// F. Per-guild cap + deterministic triage.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn twenty_events_same_t15_earliest_ten_win() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    // 20 distinct starts inside the window, deliberately shuffled at build.
    let mut events = Vec::new();
    for i in 0..20u32 {
        let mut event = base_event(now_ms);
        event["instance_starts_at_ms"] = json!(now_ms + MINUTE_MS + i64::from(i) * 40_000);
        events.push(event);
    }
    events.swap(0, 19);
    events.swap(4, 11);
    let mut expected: Vec<(i64, String, String)> = events
        .iter()
        .map(|e| {
            (
                e["instance_starts_at_ms"].as_i64().expect("ms"),
                e["event_id"].as_str().expect("id").to_owned(),
                e["name"].as_str().expect("name").to_owned(),
            )
        })
        .collect();
    expected.sort();
    let expected_ids: Vec<String> = expected
        .iter()
        .take(10)
        .map(|(_, id, _)| id.clone())
        .collect();
    let expected_names: Vec<String> = expected
        .iter()
        .take(10)
        .map(|(_, _, name)| name.clone())
        .collect();

    let outcome = tick(&mock, &guild, &events, Some(category.as_str()), now_ms)
        .await
        .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    let posts = create_posts(&requests);
    assert_eq!(posts.len(), 10, "cap is 10 per guild: {requests:?}");
    let bodies = post_bodies(&posts);
    let posted_names: Vec<String> = bodies
        .iter()
        .map(|b| {
            b.get("name")
                .and_then(Value::as_str)
                .expect("name")
                .to_owned()
        })
        .collect();
    assert_eq!(
        posted_names, expected_names,
        "earliest starts_at win, in order"
    );
    for body in &bodies {
        assert_eq!(body.get("type").and_then(Value::as_u64), Some(2));
        assert_eq!(
            body.get("parent_id").and_then(Value::as_str),
            Some(category.as_str())
        );
    }
    assert_eq!(counter(&outcome, "created"), 10, "{outcome}");
    assert_eq!(counter(&outcome, "rate_limited"), 10, "{outcome}");
    for event in &events {
        let id = event["event_id"].as_str().expect("id");
        let want = if expected_ids.contains(&id.to_owned()) {
            "created"
        } else {
            "rate_limited"
        };
        assert_eq!(record_outcome(record_for(&outcome, id)), want, "{outcome}");
    }
}

#[tokio::test]
async fn in_tick_duplicate_candidates_create_once() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let event = base_event(now_ms);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        &[event.clone(), event],
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        1,
        "one input list, one create: {requests:?}"
    );
    assert_eq!(counter(&outcome, "created"), 1, "{outcome}");
    let records = outcome
        .get("events")
        .and_then(Value::as_array)
        .expect("events");
    assert_eq!(records.len(), 2, "one record per input: {outcome}");
    assert_eq!(record_outcome(&records[0]), "created");
    assert_eq!(record_outcome(&records[1]), "skipped");
    assert_eq!(record_reason(&records[1]), "duplicate_input");
    assert_eq!(
        records[1].get("event_id").and_then(Value::as_str),
        Some(event_id.as_str())
    );
}

// ---------------------------------------------------------------------------
// G. Reconcile.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_healthy_binding_is_noop() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800011";
    state
        .lock()
        .expect("fake")
        .preset_live(binding, &guild, &category);
    let mut event = base_event(now_ms);
    event["voice_channel_id"] = json!(binding);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        create_posts(&requests).is_empty(),
        "no recreate: {requests:?}"
    );
    assert!(deletes(&requests).is_empty(), "no delete: {requests:?}");
    assert!(
        patches_and_puts(&requests).is_empty(),
        "never renames: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "noop", "{outcome}");
    assert_eq!(record_reason(record), "bound_healthy", "{outcome}");
}

#[tokio::test]
async fn reconcile_occupied_room_never_deleted() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800012";
    state
        .lock()
        .expect("fake")
        .preset_live(binding, &guild, &category);
    let mut event = base_event(now_ms);
    event["voice_channel_id"] = json!(binding);
    event["occupant_ids"] = json!(["1100001", "1100002"]);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        deletes(&requests).is_empty(),
        "occupied rooms are never deleted: {requests:?}"
    );
    assert!(
        create_posts(&requests).is_empty(),
        "and never recreated: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "noop", "{outcome}");
    assert_eq!(record_reason(record), "occupied_kept", "{outcome}");
}

#[tokio::test]
async fn reconcile_mod_deleted_recreates_exactly_once() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800013";
    state.lock().expect("fake").preset_mod_deleted(binding);
    let mut event = base_event(now_ms);
    event["voice_channel_id"] = json!(binding);
    event["binding_deleted"] = json!(true);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        1,
        "recreates exactly once: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "recreated", "{outcome}");
    let new_id = record
        .get("channel_id")
        .and_then(Value::as_str)
        .expect("new channel");
    assert_ne!(
        new_id, binding,
        "the recreate mints a new channel: {outcome}"
    );
    assert_eq!(
        counter(&outcome, "created"),
        1,
        "recreates count as created: {outcome}"
    );
}

#[tokio::test]
async fn reconcile_mod_deleted_post_start_does_not_recreate() {
    // Outcome unpinned (handoff card owns post-start records); no POST pinned.
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800014";
    state.lock().expect("fake").preset_mod_deleted(binding);
    let mut event = base_event(now_ms);
    event["instance_starts_at_ms"] = json!(now_ms - MINUTE_MS);
    event["voice_channel_id"] = json!(binding);
    event["binding_deleted"] = json!(true);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        create_posts(&requests).is_empty(),
        "post-start never recreates: {requests:?}"
    );
    record_for(&outcome, &event_id);
}

#[tokio::test]
async fn far_future_move_empty_deletes_once() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800015";
    state
        .lock()
        .expect("fake")
        .preset_live(binding, &guild, &category);
    let mut event = base_event(now_ms);
    event["instance_starts_at_ms"] = json!(now_ms + 24 * HOUR_MS + 1);
    event["voice_channel_id"] = json!(binding);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    let gone = deletes(&requests);
    assert_eq!(gone.len(), 1, "far-future empty deletes once: {requests:?}");
    assert!(
        gone[0].path.contains(binding),
        "deletes the bound channel: {requests:?}"
    );
    assert!(
        create_posts(&requests).is_empty(),
        "recreate waits for T-15: {requests:?}"
    );
    let record = record_for(&outcome, &event_id);
    assert_eq!(record_outcome(record), "deleted", "{outcome}");
    assert_eq!(
        record.get("channel_id").and_then(Value::as_str),
        Some(binding),
        "{outcome}"
    );
    assert_eq!(counter(&outcome, "deleted"), 1, "{outcome}");
}

#[tokio::test]
async fn far_future_move_occupied_never_deleted() {
    // Outcome unpinned: the handoff card converts the occupied room; here
    // only the no-DELETE is pinned.
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800016";
    state
        .lock()
        .expect("fake")
        .preset_live(binding, &guild, &category);
    let mut event = base_event(now_ms);
    event["instance_starts_at_ms"] = json!(now_ms + 25 * HOUR_MS);
    event["voice_channel_id"] = json!(binding);
    event["occupant_ids"] = json!(["1100003"]);
    let event_id = event["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        std::slice::from_ref(&event),
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        deletes(&requests).is_empty(),
        "occupied never deleted: {requests:?}"
    );
    record_for(&outcome, &event_id);
}

#[tokio::test]
async fn small_move_keeps_bound_channel() {
    // At most now+24h (inclusive) the bound channel stays put.
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let binding = "8800017";
    state
        .lock()
        .expect("fake")
        .preset_live(binding, &guild, &category);
    let mut moved = base_event(now_ms);
    moved["instance_starts_at_ms"] = json!(now_ms + 2 * HOUR_MS);
    moved["voice_channel_id"] = json!(binding);
    let mut at_edge = base_event(now_ms);
    at_edge["instance_starts_at_ms"] = json!(now_ms + 24 * HOUR_MS);
    at_edge["voice_channel_id"] = json!(binding);
    let events = [moved, at_edge];
    let ids: Vec<String> = events
        .iter()
        .map(|e| e["event_id"].as_str().expect("id").to_owned())
        .collect();

    let outcome = tick(&mock, &guild, &events, Some(category.as_str()), now_ms)
        .await
        .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        deletes(&requests).is_empty(),
        "small moves keep the channel: {requests:?}"
    );
    assert!(
        create_posts(&requests).is_empty(),
        "and never recreate: {requests:?}"
    );
    for id in &ids {
        let record = record_for(&outcome, id);
        assert_eq!(record_outcome(record), "noop", "{outcome}");
        assert_eq!(record_reason(record), "bound_healthy", "{outcome}");
    }
}

// ---------------------------------------------------------------------------
// H. Category gap + per-tick counters.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_category_disables_all_creates() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let events = [base_event(now_ms), base_event(now_ms)];
    let ids: Vec<String> = events
        .iter()
        .map(|e| e["event_id"].as_str().expect("id").to_owned())
        .collect();

    let outcome = tick(&mock, &guild, &events, None, now_ms)
        .await
        .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert!(
        requests.is_empty(),
        "no category means zero REST: {requests:?}"
    );
    assert_eq!(counter(&outcome, "no_category"), 2, "{outcome}");
    assert_eq!(counter(&outcome, "created"), 0, "{outcome}");
    for id in &ids {
        assert_eq!(
            record_outcome(record_for(&outcome, id)),
            "no_category",
            "{outcome}"
        );
    }
}

#[tokio::test]
async fn create_403_records_perm_denied_without_crashing_tick() {
    let now_ms = BASE_NOW_MS;
    let (mock, state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    {
        // First POST 403s, the second (default) creates: order follows the
        // input order for equal starts... so pin distinct starts instead.
        state
            .lock()
            .expect("fake")
            .create_script
            .push_back(CreateScript::Forbidden);
    }
    let mut denied = base_event(now_ms);
    denied["instance_starts_at_ms"] = json!(now_ms + 5 * MINUTE_MS);
    let denied_id = denied["event_id"].as_str().expect("id").to_owned();
    let mut allowed = base_event(now_ms);
    allowed["instance_starts_at_ms"] = json!(now_ms + 6 * MINUTE_MS);
    let allowed_id = allowed["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        &[denied, allowed],
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("a 403 never crashes the tick");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(
        create_posts(&requests).len(),
        2,
        "both attempted: {requests:?}"
    );
    assert_eq!(
        record_outcome(record_for(&outcome, &denied_id)),
        "perm_denied",
        "{outcome}"
    );
    assert_eq!(
        record_outcome(record_for(&outcome, &allowed_id)),
        "created",
        "{outcome}"
    );
    assert_eq!(counter(&outcome, "perm_denied"), 1, "{outcome}");
    assert_eq!(counter(&outcome, "created"), 1, "{outcome}");
}

#[tokio::test]
async fn mixed_tick_counters_add_up() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let guild = fresh_guild();
    let category = fresh_category();
    let ok = base_event(now_ms);
    let ok_id = ok["event_id"].as_str().expect("id").to_owned();
    let mut external = base_event(now_ms);
    external["entity_type"] = json!("EXTERNAL");
    let external_id = external["event_id"].as_str().expect("id").to_owned();
    let mut started = base_event(now_ms);
    started["instance_starts_at_ms"] = json!(now_ms - MINUTE_MS);
    let started_id = started["event_id"].as_str().expect("id").to_owned();
    let mut opted = base_event(now_ms);
    opted["voice_opt_out"] = json!(true);
    let opted_id = opted["event_id"].as_str().expect("id").to_owned();

    let outcome = tick(
        &mock,
        &guild,
        &[ok, external, started, opted],
        Some(category.as_str()),
        now_ms,
    )
    .await
    .expect("tick Ok");
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(create_posts(&requests).len(), 1, "{requests:?}");
    assert_tick_clock(&outcome, now_ms);
    assert_eq!(counter(&outcome, "created"), 1, "{outcome}");
    assert_eq!(counter(&outcome, "rate_limited"), 0, "{outcome}");
    assert_eq!(counter(&outcome, "perm_denied"), 0, "{outcome}");
    assert_eq!(counter(&outcome, "no_category"), 0, "{outcome}");
    assert_eq!(counter(&outcome, "deleted"), 0, "{outcome}");
    assert_eq!(record_outcome(record_for(&outcome, &ok_id)), "created");
    assert_eq!(
        record_outcome(record_for(&outcome, &external_id)),
        "skipped"
    );
    assert_eq!(record_outcome(record_for(&outcome, &started_id)), "skipped");
    assert_eq!(record_outcome(record_for(&outcome, &opted_id)), "skipped");
    assert_eq!(
        outcome
            .get("events")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(4),
        "one record per input: {outcome}"
    );
}

// ---------------------------------------------------------------------------
// Keep-alive: explicit UFCS call keeps the adapter live after GREEN, else
// clippy `-D warnings` fails on a never-used trait the builder may not fix.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn baseline_adapter_keeps_the_unwired_path_callable() {
    let now_ms = BASE_NOW_MS;
    let (mock, _state) = start_double().await;
    let rest = executor_for(&mock);
    let (_stop, shutdown) = watch::channel(false);
    let gate = JobGate::default();
    let outcome = <Kind as BaselineEventVoiceTick>::run_event_voice_tick(
        &rest,
        &fresh_guild(),
        &[],
        None,
        now_ms,
        &shutdown,
        &gate,
    )
    .await
    .expect("adapter is infallible");
    mock.shutdown().await;
    assert_eq!(counter(&outcome, "created"), 0);
    assert_eq!(
        outcome
            .get("events")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
}
