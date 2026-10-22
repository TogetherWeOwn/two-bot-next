#![cfg(test)]
//! Protected acceptance: end-of-event handoff + ownership (SPEC card 3 +
//! R3/R4/R6/R7/R8/R9/R10/R11/R15).
//!
//! PROTECTED FILE: the paired Builder implements the handoff cores, the
//! worker re-verify, the command gating and the migrations without editing
//! this file. The only wiring outside it is the `#[path] mod
//! event_voice_handoff_acceptance;` line in `voice_rooms_tests.rs`.
//!
//! ## What is pinned
//!
//! Two pure cores plus job registration (which is the ticker card's;
//! this card pins no job):
//!
//! - ONE async inherent method with EXACTLY this signature:
//!   ```ignore
//!   impl LiveGuild {
//!       pub async fn run_event_voice_handoff(
//!           rest: &ActionExecutor,
//!           guild: &str,
//!           handoff: &serde_json::Value,
//!           now_ms: i64,
//!       ) -> Result<serde_json::Value, String>;
//!   }
//!   ```
//! - ONE sync inherent method with EXACTLY this signature:
//!   ```ignore
//!   impl LiveGuild {
//!       pub fn check_event_voice_power(
//!           request: &serde_json::Value,
//!       ) -> serde_json::Value;
//!   }
//!   ```
//!
//! ## RED NOW (baseline)
//!
//! The inherents do not exist yet, so every call below resolves to the
//! test-local baseline adapters, which replay today's behavior: the
//! handoff adapter returns an unwired `noop`, and the power adapter
//! allows everything with no audit. Every behavior test fails on the
//! missing decision, the missing audit record, or the missing Discord
//! calls; the worker/responder/adapter controls stay green.
//!
//! ## GREEN (builder)
//!
//! Add the two inherent methods above. Inherent methods shadow trait
//! methods, so these calls flip from the adapters to the wired cores
//! with no test edit. Then point the EventVoice scheduler at them:
//! ended candidates go to the handoff core, every after-party
//! kick/private goes through the power check. That wiring, the snapshot
//! mapping (mirror row + `event_voice_channels` row + live voice view ->
//! snapshot), and the persistence of the returned outcome (worker
//! re-bind, owner grant, audit log emission) are review-verified, the
//! same split as the ticker core.
//!
//! ## The handoff core is a pure function of (snapshot, now, REST)
//!
//! - Inputs arrive as ONE caller-built snapshot (schema below): the
//!   scheduler scans the mirror plus `event_voice_channels`, builds the
//!   snapshot for the ended candidate, calls the core once, then
//!   persists the returned outcome. The scan mapping and the persistence
//!   are review-verified; the table card pins the store semantics.
//! - The core takes NO pool: it issues no SQL. All presence spans,
//!   occupant flags and clocks arrive in the snapshot.
//! - ALL windows derive from the passed `now_ms` (tick time, UTC
//!   millis). The scheduler passes wall-clock; tests pass scripted
//!   time. There is no cross-tick core state, so no test needs serial
//!   execution and wall-clock buckets would fail these tests.
//! - REST is single-attempt: the core MUST NOT sleep out `Retry-After`
//!   or retry inside a REST call. Every call runs under a 60 s timeout;
//!   a stall fails as a timeout, not a hang. Retry pacing is the
//!   ticker's; the next tick re-runs undecided candidates.
//! - Transport failures (mock down, broken body, unexpected status):
//!   builder's choice, `Err(String)` recommended; unpinned, no test
//!   covers them.
//! - Occupancy is HUMAN occupancy, matching `LiveGuild::humans`: a room
//!   with only bots is EMPTY for lifecycle purposes and takes the
//!   wait/delete path, never convert.
//!
//! ## Input snapshot schema (all tests build these via `base_handoff`)
//!
//! - `event_id: String` (required)
//! - `instance_starts_at_ms: i64` (required; UTC millis)
//! - `scheduled_end_ms: i64 | null` (required key; null = no end time,
//!   the start+4h backstop applies)
//! - `discord_status: String` (required; `SCHEDULED|ACTIVE|COMPLETED|`
//!   `CANCELED`)
//! - `event_name: String` (required; slug source for the rename)
//! - `event_timezone: String` (required; IANA name, e.g.
//!   `America/New_York`; the rename suffix renders the instance start
//!   in this zone)
//! - `voice_channel_id: String | null` (required key; null = no
//!   channel, noop)
//! - `channel_created_ms: i64` (required; young/old math)
//! - `total_joins: u64` (required; lifetime joins for this instance)
//! - `empty_since_ms: i64 | null` (required key; null = occupied, or
//!   newly empty when no occupant is present)
//! - `occupants: [{user_id: String, is_bot: bool, is_member: bool,
//!   is_host: bool, first_join_ms: i64, severed_before_ms: i64 |
//!   null}]` (required; the live voice view, enriched by the
//!   scheduler; empty = empty room)
//! - `presence_spans: [{user_id: String, joined_ms: i64, left_ms: i64 |
//!   null}]` (required; instance spans from PG; null `left_ms` = open,
//!   counted to `now_ms`)
//! - `memory_joins: [{user_id: String, joined_ms: i64}]` (default [];
//!   in-memory presence for the zero-rows fallback)
//! - `converted_at_ms: i64 | null` (default null; set = already
//!   converted, noop, never rename back)
//! - `existing_names: [String]` (default []; live sibling names for
//!   rename-collision detection)
//!
//! ## Outcome schema (every shape below is pinned)
//!
//! `{tick_ms, decision, reason, owner_id, owner_basis, rename_to,
//! audit}` where `tick_ms` echoes `now_ms`, `decision` is
//! `convert|delete|wait|noop`, and `reason` is pinned per the decision
//! table. `owner_id` is set only on convert with an elected owner;
//! `owner_basis` is `spans|fallback_host|fallback_memory|`
//! `fallback_claimer_pending` on convert, else null. `rename_to` is the
//! exact new channel name on convert, else null. `audit` is
//! `{event_id, instance_starts_at_ms, channel_id, tick_ms, decision,
//! reason, owner_id, owner_basis}` on convert/delete, else null.
//!
//! ## Decision table pinned here
//!
//! - Ended = Discord COMPLETED, or `now > scheduled_end + 30m`, or (no
//!   end time) `now > starts_at + 4h` (host D5, binding). The `+30m`
//!   and `+4h` bounds are STRICT: equality is not ended.
//! - Ended + human-occupied -> `convert` (`ended_occupied`). The
//!   overrun rule fires over a still-ACTIVE event past end+30m.
//! - Ended + empty + empty for <10m -> `wait` (`empty_in_grace`), zero
//!   REST. Empty with unknown `empty_since` waits (newly empty).
//! - Ended + empty + empty for >=10m -> `delete`
//!   (`empty_grace_elapsed`).
//! - CANCELED after create with occupants -> `convert`
//!   (`canceled_occupied`), never orphaned, never a COMPLETED patch.
//! - CANCELED + empty + young (created <5m ago, strict) + zero joins
//!   -> `delete` (`canceled_empty_young_no_joins`).
//! - CANCELED + empty + (old or joins > 0) -> `convert`
//!   (`canceled_empty_convert`).
//! - Not ended -> `noop` (`not_ended`), zero REST.
//! - Already converted (`converted_at_ms` set) -> `noop`
//!   (`already_converted`), zero REST: a mod rename is never fought.
//! - No channel (`voice_channel_id` null) -> `noop` (`no_channel`).
//!
//! ## Convert/delete REST contract (R7 metadata-only)
//!
//! - Convert issues at most: PATCH event `{status: COMPLETED}` with
//!   EXACTLY that body (never `scheduled_end_time`, never
//!   `entity_type`, never `channel_id`), plus PATCH channel `{name:
//!   rename_to}` with EXACTLY that body (no move, no type change, no
//!   overwrites). The event PATCH is skipped when Discord already
//!   reports COMPLETED, and never sent for CANCELED events.
//! - Delete issues at most: the same event PATCH under the same skip
//!   rules, plus DELETE channel.
//! - No member kick/disconnect/move, no permission overwrite PUT, no
//!   channel POST, no message POST, on either path. Plain GETs are
//!   allowed (builders may re-verify reads).
//!
//! ## Ownership election pinned here (R8/R9)
//!
//! - Eligible = present AND non-bot AND current member. Bots and
//!   leavers never own, however long their spans.
//! - Score = summed instance spans per user; open spans count to
//!   `now_ms`; spans with `joined_ms < severed_before_ms` are
//!   excluded (kick severs continuity: kicked-then-rejoined counts
//!   only current spans). Plain rejoins sum.
//! - Ties: host-if-present, then earliest `first_join_ms`, then
//!   lowest NUMERIC user id.
//! - Zero PG spans with humans present: host-if-present, then
//!   earliest in-memory join among PRESENT humans (absent memory
//!   entries are skipped), else convert with a pending owner
//!   (`owner_id` null, basis `fallback_claimer_pending`): the first
//!   claim wins post-handoff. The fired fallback is pinned in
//!   `owner_basis` and the audit record; log emission is
//!   review-verified.
//!
//! ## Power-check table pinned here (R11)
//!
//! Request: `{power, event_id, actor_id, channel_id, after_party,
//! converted_at_ms | null, mod_power_grant, is_admin, now_ms}`.
//! Response: `{allowed, reason, audit | null}` with audit
//! `{event_id, channel_id, actor_id, power, decision, reason,
//! tick_ms}`.
//!
//! - Non-after-party -> allowed (`not_after_party`), no audit.
//! - After-party + power outside kick/private -> allowed
//!   (`uncapped_power`), no audit.
//! - After-party + kick/private + admin -> allowed (`admin`), audited.
//! - After-party + kick/private + mod grant -> allowed
//!   (`mod_grant`), audited.
//! - After-party + kick/private + elapsed >= 600_000 ms since
//!   conversion -> allowed (`window_elapsed`), audited.
//! - Otherwise -> denied (`capped_window`), audited. A null
//!   `converted_at_ms` or a negative elapsed denies fail-closed.
//!
//! Precedence: party-scope, then power-scope, then admin, then grant,
//! then window. Denied attempts audit too: R11 says every kick/private
//! in an after-party writes the line, and a denied attempt inside the
//! cap window is the abuse signal.
//!
//! ## Discovery, not dictation, where the cards are silent
//!
//! - `severed_before_ms`: the scheduler marks the kick instant per
//!   occupant; the RULE (pre-kick spans excluded) is pinned, the
//!   storage is the scheduler's.
//! - Lowest id compares NUMERICALLY (snowflake order).
//! - `fallback_claimer_pending`: with zero rows, no host and no
//!   memory, the room converts owner-pending and the first claim
//!   wins; the claim path itself is builder-owned.
//! - Slug rule: lowercase, non-alphanumeric runs collapse to one
//!   `-`, trimmed, first 40 chars, trailing `-` re-trimmed, empty ->
//!   `event`. Unicode alphanumerics are kept.
//! - Collision short id = last 6 chars of the event id (snowflake
//!   prefixes collide for near-simultaneous events; suffixes do not).
//! - Bots-only rooms take the empty path (worker `humans`
//!   semantics).
//! - Unknown power strings are allowed fail-open (the cap list is
//!   kick/private, not an allow-list).
//! - Scheduler/wiring items review-verified, not pinned here: tick
//!   routing, snapshot mapping, outcome persistence (worker re-bind,
//!   owner grant), audit emission, owner-claim path, standard
//!   reclaim/auto-transfer (existing chain, pinned against
//!   regression by the controls below).
//!
//! ## Seams this file depends on (builder: preserve them)
//!
//! `LiveGuild`; `ActionExecutor::with_proxy`;
//! `MockRest::with_responder`, `requests`, `shutdown`, `origin`;
//! `ScriptedResponse::{json, status}`; `RestRequest`
//! `{method, path, body}`; `fixture`, `owned_room`, `room`,
//! `channel`, `dispatch`, `EMPTY_ROOM_GRACE`, `OwnershipCommand`,
//! `GuildRoomWorker::load`, `reconcile`, `tracked`,
//! `apply_ownership`, `LiveGuild::{upsert_channel, voice_update,
//! voice_update_at}`.

use super::*;

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod mock_rest;

use mock_rest::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use two_bot_discord::ActionExecutor;

// ---------------------------------------------------------------------------
// Baseline adapters: today's unwired behavior. The builder's inherent
// methods on `LiveGuild` shadow these with no test edit.
// ---------------------------------------------------------------------------

trait BaselineEventVoiceHandoff {
    async fn run_event_voice_handoff(
        rest: &ActionExecutor,
        guild: &str,
        handoff: &Value,
        now_ms: i64,
    ) -> Result<Value, String>;
}

impl BaselineEventVoiceHandoff for LiveGuild {
    async fn run_event_voice_handoff(
        _rest: &ActionExecutor,
        _guild: &str,
        _handoff: &Value,
        _now_ms: i64,
    ) -> Result<Value, String> {
        Ok(json!({
            "decision": "noop",
            "reason": "unwired_baseline",
        }))
    }
}

trait BaselineEventVoicePowerCheck {
    fn check_event_voice_power(request: &Value) -> Value;
}

impl BaselineEventVoicePowerCheck for LiveGuild {
    fn check_event_voice_power(_request: &Value) -> Value {
        json!({
            "allowed": true,
            "reason": "unwired_baseline",
            "audit": Value::Null,
        })
    }
}

// ---------------------------------------------------------------------------
// Constants: absolute scripted clocks (wall-clock never enters a window).
// ---------------------------------------------------------------------------

/// 2026-07-10T20:00:00Z: July instance, America/New_York is EDT (UTC-4).
const SUMMER_INSTANCE_MS: i64 = 1783713600000;
/// 2026-01-10T20:00:00Z: January instance, America/New_York is EST (UTC-5),
/// Europe/London is GMT (UTC+0).
const WINTER_INSTANCE_MS: i64 = 1768075200000;
/// Instance + 2h scheduled length + 31m overrun: ended via end+30m.
const SUMMER_NOW_MS: i64 = 1783722660000;
const WINTER_NOW_MS: i64 = 1768084260000;
/// Summer instance + 4h + 1m: ended via the null-end backstop.
const NULL_END_NOW_MS: i64 = 1783728060000;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const END_GRACE_MS: i64 = 30 * MINUTE_MS;
const BACKSTOP_MS: i64 = 4 * HOUR_MS;
const EMPTY_GRACE_MS: i64 = 10 * MINUTE_MS;
const YOUNG_MS: i64 = 5 * MINUTE_MS;
const CAP_WINDOW_MS: i64 = 10 * MINUTE_MS;

/// Every handoff call runs under this budget: a core that sleeps inside
/// REST fails here instead of hanging the test binary.
const TICK_TIMEOUT: Duration = Duration::from_secs(60);

const GUILD_S: &str = "7100001";
const EVENT_ID: &str = "123456789";
const CHANNEL_ID: &str = "9501";
const HUMAN_HOST: &str = "3001";
const HUMAN_MEMBER: &str = "3002";
const HUMAN_LEAVER: &str = "3003";
const BOT_MEMBER: &str = "4001";

const RENAME_SUMMER: &str = "after-party · friday-8s · 0710-1600";
const RENAME_WINTER: &str = "after-party · friday-8s · 0110-1500";
const RENAME_UTC: &str = "after-party · friday-8s · 0110-2000";
const RENAME_TRUNC: &str = "after-party · a-very-long-event-name-that-definitely-e · 0710-1600";
const RENAME_COLLISION: &str = "after-party · friday-8s · 0710-1600 · 456789";

// ---------------------------------------------------------------------------
// Snapshot builders.
// ---------------------------------------------------------------------------

fn occ(user_id: &str, host: bool, first_join_ms: i64) -> Value {
    json!({
        "user_id": user_id,
        "is_bot": false,
        "is_member": true,
        "is_host": host,
        "first_join_ms": first_join_ms,
        "severed_before_ms": Value::Null,
    })
}

fn span(user_id: &str, joined_ms: i64, left_ms: Option<i64>) -> Value {
    json!({
        "user_id": user_id,
        "joined_ms": joined_ms,
        "left_ms": left_ms,
    })
}

fn mem(user_id: &str, joined_ms: i64) -> Value {
    json!({
        "user_id": user_id,
        "joined_ms": joined_ms,
    })
}

/// Default: ACTIVE summer event, ended via end+30m, two humans present,
/// host longest. Mutate the returned `Value` for the case under test.
fn base_handoff() -> (Value, i64) {
    let now_ms = SUMMER_NOW_MS;
    let snap = json!({
        "event_id": EVENT_ID,
        "instance_starts_at_ms": SUMMER_INSTANCE_MS,
        "scheduled_end_ms": SUMMER_INSTANCE_MS + 2 * HOUR_MS,
        "discord_status": "ACTIVE",
        "event_name": "Friday 8s",
        "event_timezone": "America/New_York",
        "voice_channel_id": CHANNEL_ID,
        "channel_created_ms": SUMMER_INSTANCE_MS,
        "total_joins": 5,
        "empty_since_ms": Value::Null,
        "occupants": [
            occ(HUMAN_HOST, true, SUMMER_INSTANCE_MS),
            occ(HUMAN_MEMBER, false, SUMMER_INSTANCE_MS + HOUR_MS),
        ],
        "presence_spans": [
            span(HUMAN_HOST, SUMMER_INSTANCE_MS, None),
            span(HUMAN_MEMBER, SUMMER_INSTANCE_MS + HOUR_MS, None),
        ],
        "memory_joins": [],
        "converted_at_ms": Value::Null,
        "existing_names": [],
    });
    (snap, now_ms)
}

fn base_power(power: &str) -> (Value, i64) {
    let now_ms = SUMMER_NOW_MS;
    let request = json!({
        "power": power,
        "event_id": EVENT_ID,
        "actor_id": HUMAN_MEMBER,
        "channel_id": CHANNEL_ID,
        "after_party": true,
        "converted_at_ms": now_ms - 5 * MINUTE_MS,
        "mod_power_grant": false,
        "is_admin": false,
        "now_ms": now_ms,
    });
    (request, now_ms)
}

// ---------------------------------------------------------------------------
// Harness: scripted fake Discord + handoff driver.
// ---------------------------------------------------------------------------

struct FakeHandoff {
    live: HashMap<String, Value>,
}

impl FakeHandoff {
    fn new() -> Self {
        Self {
            live: HashMap::new(),
        }
    }

    fn preset_live(&mut self, channel_id: &str, guild: &str, name: &str) {
        self.live.insert(
            channel_id.to_owned(),
            json!({
                "id": channel_id,
                "type": 2,
                "guild_id": guild,
                "name": name,
                "parent_id": "8414",
            }),
        );
    }
}

fn channel_id_from_path(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let (_, after) = path.split_once("/channels/")?;
    let id = after.split('/').next().unwrap_or("").trim();
    (!id.is_empty()).then(|| id.to_owned())
}

fn event_id_from_path(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let (_, after) = path.split_once("/scheduled-events/")?;
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

fn is_event_patch(request: &RestRequest) -> bool {
    request.method == "PATCH" && event_id_from_path(&request.path).is_some()
}

fn is_channel_patch(request: &RestRequest) -> bool {
    request.method == "PATCH" && channel_id_from_path(&request.path).is_some()
}

fn is_channel_delete(request: &RestRequest) -> bool {
    request.method == "DELETE" && channel_id_from_path(&request.path).is_some()
}

fn is_single_get(request: &RestRequest) -> bool {
    request.method == "GET" && channel_id_from_path(&request.path).is_some()
}

fn respond(state: &Arc<Mutex<FakeHandoff>>, request: &RestRequest) -> ScriptedResponse {
    let mut fake = state.lock().expect("fake handoff lock");
    if is_event_patch(request) {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let id = event_id_from_path(&request.path).unwrap_or_default();
        return ScriptedResponse::json(
            200,
            json!({
                "id": id,
                "guild_id": guild_from_path(&request.path),
                "channel_id": CHANNEL_ID,
                "name": "Friday 8s",
                "status": body.get("status").cloned().unwrap_or(Value::Null),
            }),
        );
    }
    if is_channel_patch(request) {
        let id = channel_id_from_path(&request.path).unwrap_or_default();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let name = body.get("name").cloned().unwrap_or(Value::Null);
        if let Some(channel) = fake.live.get_mut(&id) {
            channel["name"] = name.clone();
            return ScriptedResponse::json(200, channel.clone());
        }
        return ScriptedResponse::json(
            200,
            json!({
                "id": id,
                "type": 2,
                "guild_id": guild_from_path(&request.path),
                "name": name,
            }),
        );
    }
    if is_channel_delete(request) {
        let id = channel_id_from_path(&request.path).unwrap_or_default();
        if fake.live.remove(&id).is_some() {
            return ScriptedResponse::status(204);
        }
        return ScriptedResponse::json(404, json!({"message": "Unknown Channel", "code": 10003}));
    }
    if is_single_get(request) {
        let id = channel_id_from_path(&request.path).unwrap_or_default();
        if let Some(channel) = fake.live.get(&id) {
            return ScriptedResponse::json(200, channel.clone());
        }
        return ScriptedResponse::json(404, json!({"message": "Unknown Channel", "code": 10003}));
    }
    // Fail closed on anything else: an unexpected route or verb is a loud
    // 404, never a silent pass.
    ScriptedResponse::json(404, json!({"message": "Unknown Channel", "code": 10003}))
}

/// Start the double with the case channel preset live.
async fn start_double() -> (MockRest, Arc<Mutex<FakeHandoff>>) {
    let state = Arc::new(Mutex::new(FakeHandoff::new()));
    state
        .lock()
        .expect("fake handoff lock")
        .preset_live(CHANNEL_ID, GUILD_S, "Friday 8s");
    let responder_state = Arc::clone(&state);
    let mock = MockRest::with_responder(move |request| respond(&responder_state, request)).await;
    (mock, state)
}

async fn handoff_tick(
    mock: &MockRest,
    guild: &str,
    snap: &Value,
    now_ms: i64,
) -> Result<Value, String> {
    let rest = ActionExecutor::with_proxy(
        "event-voice-handoff-fixture-token".to_owned(),
        Some(mock.origin()),
    )
    .expect("executor builds against the mock");
    tokio::time::timeout(
        TICK_TIMEOUT,
        LiveGuild::run_event_voice_handoff(&rest, guild, snap, now_ms),
    )
    .await
    .expect("handoff must not stall: REST is single-attempt")
}

// ---------------------------------------------------------------------------
// Assertion helpers (panic messages carry the full outcome for RED triage).
// ---------------------------------------------------------------------------

fn decision_of(outcome: &Value) -> &str {
    outcome
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("outcome lacks decision: {outcome}"))
}

fn reason_of(outcome: &Value) -> &str {
    outcome
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("outcome lacks reason: {outcome}"))
}

fn owner_of(outcome: &Value) -> Option<&str> {
    outcome.get("owner_id").and_then(Value::as_str)
}

fn basis_of(outcome: &Value) -> Option<&str> {
    outcome.get("owner_basis").and_then(Value::as_str)
}

fn rename_of(outcome: &Value) -> Option<&str> {
    outcome.get("rename_to").and_then(Value::as_str)
}

fn audit_of(outcome: &Value) -> Option<&Value> {
    outcome.get("audit").filter(|&audit| !audit.is_null())
}

fn assert_tick_clock(outcome: &Value, now_ms: i64) {
    assert_eq!(
        outcome.get("tick_ms").and_then(Value::as_i64),
        Some(now_ms),
        "outcome must echo the passed tick clock: {outcome}"
    );
}

fn json_body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).unwrap_or_else(|_| {
        panic!(
            "request body is not JSON: {} {}",
            request.method, request.path
        )
    })
}

fn event_patches(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests.iter().filter(|r| is_event_patch(r)).collect()
}

fn channel_patches(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests.iter().filter(|r| is_channel_patch(r)).collect()
}

fn channel_deletes(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests.iter().filter(|r| is_channel_delete(r)).collect()
}

/// R7: a convert touches at most the COMPLETED patch (unless skipped)
/// and exactly one name-only rename. Anything else mutating fails.
fn assert_convert_rest_clean(requests: &[RestRequest], expect_event_patch: bool, rename: &str) {
    for request in requests {
        if is_event_patch(request) {
            assert!(
                request.path.contains(&format!("/guilds/{GUILD_S}/")),
                "event patch must target the tick guild: {}",
                request.path
            );
            assert_eq!(
                event_id_from_path(&request.path).as_deref(),
                Some(EVENT_ID),
                "event patch must target the case event: {}",
                request.path
            );
            assert_eq!(
                json_body(request),
                json!({"status": "COMPLETED"}),
                "event patch carries status only: no end time, no type, no channel",
            );
        } else if is_channel_patch(request) {
            assert_eq!(
                channel_id_from_path(&request.path).as_deref(),
                Some(CHANNEL_ID),
                "rename must target the event channel: {}",
                request.path
            );
            assert_eq!(
                json_body(request),
                json!({"name": rename}),
                "rename patch carries the name only: no move, no type, no overwrites",
            );
        } else if request.method == "GET" {
            // Lenient: builders may re-verify reads.
        } else {
            panic!(
                "forbidden mutating call on the convert path: {} {}",
                request.method, request.path
            );
        }
    }
    let patches = event_patches(requests);
    if expect_event_patch {
        assert_eq!(
            patches.len(),
            1,
            "convert sends exactly one event patch: {}",
            patches.len()
        );
    } else {
        assert!(
            patches.is_empty(),
            "this convert must not patch the event at all"
        );
    }
    assert_eq!(
        channel_patches(requests).len(),
        1,
        "convert sends exactly one rename"
    );
    assert!(
        channel_deletes(requests).is_empty(),
        "convert never deletes the channel"
    );
}

/// R7 delete side: at most the COMPLETED patch (unless skipped) plus
/// exactly one channel DELETE.
fn assert_delete_rest_clean(requests: &[RestRequest], expect_event_patch: bool) {
    for request in requests {
        if is_event_patch(request) {
            assert_eq!(
                json_body(request),
                json!({"status": "COMPLETED"}),
                "event patch carries status only",
            );
        } else if is_channel_delete(request) {
            assert_eq!(
                channel_id_from_path(&request.path).as_deref(),
                Some(CHANNEL_ID),
                "delete must target the event channel: {}",
                request.path
            );
        } else if request.method == "GET" {
            // Lenient: builders may re-verify reads.
        } else {
            panic!(
                "forbidden mutating call on the delete path: {} {}",
                request.method, request.path
            );
        }
    }
    let patches = event_patches(requests);
    if expect_event_patch {
        assert_eq!(patches.len(), 1, "delete sends exactly one event patch");
    } else {
        assert!(
            patches.is_empty(),
            "this delete must not patch the event at all"
        );
    }
    assert_eq!(
        channel_deletes(requests).len(),
        1,
        "delete sends exactly one channel DELETE"
    );
}

fn assert_no_mutating_rest(requests: &[RestRequest]) {
    let mutating: Vec<String> = requests
        .iter()
        .filter(|request| request.method != "GET")
        .map(|request| {
            let method = request.method.as_str();
            let path = request.path.as_str();
            format!("{method} {path}")
        })
        .collect();
    assert!(
        mutating.is_empty(),
        "this outcome must issue no mutating REST: {mutating:?}"
    );
}

fn assert_audit(
    outcome: &Value,
    instance_ms: i64,
    now_ms: i64,
    decision: &str,
    reason: &str,
    owner_id: Option<&str>,
    owner_basis: Option<&str>,
) {
    let audit = audit_of(outcome)
        .unwrap_or_else(|| panic!("{decision} must carry an audit record: {outcome}"));
    assert_eq!(
        audit.get("event_id").and_then(Value::as_str),
        Some(EVENT_ID),
        "audit carries the event id: {audit}"
    );
    assert_eq!(
        audit.get("instance_starts_at_ms").and_then(Value::as_i64),
        Some(instance_ms),
        "audit carries the instance start: {audit}"
    );
    assert_eq!(
        audit.get("channel_id").and_then(Value::as_str),
        Some(CHANNEL_ID),
        "audit carries the channel id: {audit}"
    );
    assert_eq!(
        audit.get("tick_ms").and_then(Value::as_i64),
        Some(now_ms),
        "audit carries the actor tick: {audit}"
    );
    assert_eq!(
        audit.get("decision").and_then(Value::as_str),
        Some(decision),
        "audit carries the decision: {audit}"
    );
    assert_eq!(
        audit.get("reason").and_then(Value::as_str),
        Some(reason),
        "audit carries the reason: {audit}"
    );
    assert_eq!(
        audit.get("owner_id").and_then(Value::as_str),
        owner_id,
        "audit carries the ownership decision: {audit}"
    );
    assert_eq!(
        audit.get("owner_basis").and_then(Value::as_str),
        owner_basis,
        "audit carries the fallback used: {audit}"
    );
}

// ---------------------------------------------------------------------------
// Ended detection (host D5 + R3).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn active_past_end_plus_30m_with_occupants_converts() {
    let (mock, _state) = start_double().await;
    let (snap, now_ms) = base_handoff();
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_tick_clock(&outcome, now_ms);
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(reason_of(&outcome), "ended_occupied", "{outcome}");
    assert_eq!(owner_of(&outcome), Some(HUMAN_HOST), "{outcome}");
    assert_eq!(basis_of(&outcome), Some("spans"), "{outcome}");
    assert_eq!(rename_of(&outcome), Some(RENAME_SUMMER), "{outcome}");
    assert_audit(
        &outcome,
        SUMMER_INSTANCE_MS,
        now_ms,
        "convert",
        "ended_occupied",
        Some(HUMAN_HOST),
        Some("spans"),
    );
    assert_convert_rest_clean(&mock.requests(), true, RENAME_SUMMER);
    mock.shutdown().await;
}

#[tokio::test]
async fn completed_status_converts_without_event_repatch() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["discord_status"] = json!("COMPLETED");
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(reason_of(&outcome), "ended_occupied", "{outcome}");
    assert_eq!(rename_of(&outcome), Some(RENAME_SUMMER), "{outcome}");
    assert_convert_rest_clean(&mock.requests(), false, RENAME_SUMMER);
    mock.shutdown().await;
}

#[tokio::test]
async fn completed_empty_past_grace_deletes_without_event_repatch() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["discord_status"] = json!("COMPLETED");
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([span(
        HUMAN_MEMBER,
        now_ms - 2 * HOUR_MS,
        Some(now_ms - EMPTY_GRACE_MS - MINUTE_MS)
    )]);
    snap["empty_since_ms"] = json!(now_ms - EMPTY_GRACE_MS - MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "delete", "{outcome}");
    assert_eq!(reason_of(&outcome), "empty_grace_elapsed", "{outcome}");
    assert_delete_rest_clean(&mock.requests(), false);
    mock.shutdown().await;
}

#[tokio::test]
async fn active_within_end_grace_is_not_ended() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["scheduled_end_ms"] = json!(now_ms - 10 * MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_tick_clock(&outcome, now_ms);
    assert_eq!(decision_of(&outcome), "noop", "{outcome}");
    assert_eq!(reason_of(&outcome), "not_ended", "{outcome}");
    assert!(
        audit_of(&outcome).is_none(),
        "noop carries no audit: {outcome}"
    );
    assert_no_mutating_rest(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn scheduled_before_start_is_not_ended() {
    let (mock, _state) = start_double().await;
    let (mut snap, _) = base_handoff();
    let now_ms = SUMMER_INSTANCE_MS - MINUTE_MS;
    snap["discord_status"] = json!("SCHEDULED");
    snap["scheduled_end_ms"] = json!(SUMMER_INSTANCE_MS + 2 * HOUR_MS);
    snap["occupants"] = json!([occ(HUMAN_MEMBER, false, now_ms - 30 * MINUTE_MS)]);
    snap["presence_spans"] = json!([span(HUMAN_MEMBER, now_ms - 30 * MINUTE_MS, None)]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "noop", "{outcome}");
    assert_eq!(reason_of(&outcome), "not_ended", "{outcome}");
    assert_no_mutating_rest(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn end_plus_30m_boundary_is_strict() {
    let (mock, _state) = start_double().await;
    let (mut snap, _) = base_handoff();
    let end_ms = SUMMER_INSTANCE_MS + 2 * HOUR_MS;
    snap["scheduled_end_ms"] = json!(end_ms);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, end_ms + END_GRACE_MS)
        .await
        .expect("handoff succeeds");
    assert_eq!(
        decision_of(&outcome),
        "noop",
        "equality is not ended: {outcome}"
    );
    let outcome = handoff_tick(&mock, GUILD_S, &snap, end_ms + END_GRACE_MS + 1)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    mock.shutdown().await;
}

#[tokio::test]
async fn null_end_backstop_fires_at_start_plus_4h() {
    let (mock, _state) = start_double().await;
    let (mut snap, _) = base_handoff();
    snap["scheduled_end_ms"] = Value::Null;
    let now_ms = NULL_END_NOW_MS;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(reason_of(&outcome), "ended_occupied", "{outcome}");
    // R3: the backstop drives the math only; the PATCH never carries it.
    let requests = mock.requests();
    let patches = event_patches(&requests);
    assert_eq!(patches.len(), 1, "one COMPLETED patch");
    assert_eq!(
        json_body(patches[0]),
        json!({"status": "COMPLETED"}),
        "the computed end time is never PATCHed back to Discord"
    );
    assert_convert_rest_clean(&requests, true, RENAME_SUMMER);
    mock.shutdown().await;
}

#[tokio::test]
async fn null_end_backstop_quiet_before_4h() {
    let (mock, _state) = start_double().await;
    let (mut snap, _) = base_handoff();
    snap["scheduled_end_ms"] = Value::Null;
    let now_ms = SUMMER_INSTANCE_MS + BACKSTOP_MS - MINUTE_MS;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "noop", "{outcome}");
    assert_eq!(reason_of(&outcome), "not_ended", "{outcome}");
    assert_no_mutating_rest(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn start_plus_4h_boundary_is_strict() {
    let (mock, _state) = start_double().await;
    let (mut snap, _) = base_handoff();
    snap["scheduled_end_ms"] = Value::Null;
    let backstop_ms = SUMMER_INSTANCE_MS + BACKSTOP_MS;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, backstop_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(
        decision_of(&outcome),
        "noop",
        "equality is not ended: {outcome}"
    );
    let outcome = handoff_tick(&mock, GUILD_S, &snap, backstop_ms + 1)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// CANCELED after create (R4).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn canceled_with_occupants_converts_never_orphaned() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["discord_status"] = json!("CANCELED");
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(reason_of(&outcome), "canceled_occupied", "{outcome}");
    assert_eq!(owner_of(&outcome), Some(HUMAN_HOST), "{outcome}");
    assert_eq!(rename_of(&outcome), Some(RENAME_SUMMER), "{outcome}");
    // A canceled event is never COMPLETED-ized; the rename still fires.
    assert_convert_rest_clean(&mock.requests(), false, RENAME_SUMMER);
    mock.shutdown().await;
}

#[tokio::test]
async fn canceled_empty_young_zero_joins_deletes() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["discord_status"] = json!("CANCELED");
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([]);
    snap["channel_created_ms"] = json!(now_ms - 4 * MINUTE_MS);
    snap["total_joins"] = json!(0);
    snap["empty_since_ms"] = json!(now_ms - 4 * MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "delete", "{outcome}");
    assert_eq!(
        reason_of(&outcome),
        "canceled_empty_young_no_joins",
        "{outcome}"
    );
    assert_delete_rest_clean(&mock.requests(), false);
    mock.shutdown().await;
}

#[tokio::test]
async fn canceled_empty_old_converts() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["discord_status"] = json!("CANCELED");
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([]);
    snap["channel_created_ms"] = json!(now_ms - 6 * MINUTE_MS);
    snap["total_joins"] = json!(0);
    snap["empty_since_ms"] = json!(now_ms - 6 * MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(reason_of(&outcome), "canceled_empty_convert", "{outcome}");
    assert_convert_rest_clean(&mock.requests(), false, RENAME_SUMMER);
    mock.shutdown().await;
}

#[tokio::test]
async fn canceled_empty_with_joins_converts() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["discord_status"] = json!("CANCELED");
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([span(
        HUMAN_MEMBER,
        now_ms - 3 * MINUTE_MS,
        Some(now_ms - MINUTE_MS)
    )]);
    snap["channel_created_ms"] = json!(now_ms - 4 * MINUTE_MS);
    snap["total_joins"] = json!(3);
    snap["empty_since_ms"] = json!(now_ms - MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(reason_of(&outcome), "canceled_empty_convert", "{outcome}");
    assert_convert_rest_clean(&mock.requests(), false, RENAME_SUMMER);
    mock.shutdown().await;
}

#[tokio::test]
async fn young_boundary_is_strict() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["discord_status"] = json!("CANCELED");
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([]);
    snap["total_joins"] = json!(0);
    snap["empty_since_ms"] = json!(now_ms - YOUNG_MS);
    snap["channel_created_ms"] = json!(now_ms - YOUNG_MS + MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "delete", "4m is young: {outcome}");
    snap["channel_created_ms"] = json!(now_ms - YOUNG_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(
        decision_of(&outcome),
        "convert",
        "exactly 5m is old: {outcome}"
    );
    snap["channel_created_ms"] = json!(now_ms - YOUNG_MS - MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "6m is old: {outcome}");
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Empty-at-end grace (host D6 + R6 decision side).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_in_grace_waits_without_rest() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([span(
        HUMAN_MEMBER,
        now_ms - 2 * HOUR_MS,
        Some(now_ms - 9 * MINUTE_MS)
    )]);
    snap["empty_since_ms"] = json!(now_ms - 9 * MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_tick_clock(&outcome, now_ms);
    assert_eq!(decision_of(&outcome), "wait", "{outcome}");
    assert_eq!(reason_of(&outcome), "empty_in_grace", "{outcome}");
    assert!(
        audit_of(&outcome).is_none(),
        "wait carries no audit: {outcome}"
    );
    assert_no_mutating_rest(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn empty_past_grace_deletes() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([span(
        HUMAN_MEMBER,
        now_ms - 2 * HOUR_MS,
        Some(now_ms - EMPTY_GRACE_MS - MINUTE_MS)
    )]);
    snap["empty_since_ms"] = json!(now_ms - EMPTY_GRACE_MS - 1);
    snap["total_joins"] = json!(2);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "delete", "{outcome}");
    assert_eq!(reason_of(&outcome), "empty_grace_elapsed", "{outcome}");
    assert!(owner_of(&outcome).is_none(), "{outcome}");
    assert_audit(
        &outcome,
        SUMMER_INSTANCE_MS,
        now_ms,
        "delete",
        "empty_grace_elapsed",
        None,
        None,
    );
    assert_delete_rest_clean(&mock.requests(), true);
    mock.shutdown().await;
}

#[tokio::test]
async fn empty_with_unknown_empty_since_waits() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([]);
    snap["empty_since_ms"] = Value::Null;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "wait", "{outcome}");
    assert_eq!(reason_of(&outcome), "empty_in_grace", "{outcome}");
    assert_no_mutating_rest(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn rejoin_during_grace_cancels_delete_and_forces_convert() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    // Tick 1: empty for 30s, inside grace -> wait, no DELETE.
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([span(HUMAN_MEMBER, now_ms - HOUR_MS, Some(now_ms - 30_000))]);
    snap["empty_since_ms"] = json!(now_ms - 30_000);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "wait", "{outcome}");
    // Tick 2: a rejoin lands during grace -> convert, still no DELETE.
    snap["occupants"] = json!([occ(HUMAN_MEMBER, false, now_ms - HOUR_MS)]);
    snap["presence_spans"] = json!([
        span(HUMAN_MEMBER, now_ms - HOUR_MS, Some(now_ms - 30_000)),
        span(HUMAN_MEMBER, now_ms - 10_000, None),
    ]);
    snap["empty_since_ms"] = Value::Null;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms + 20_000)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert!(
        channel_deletes(&mock.requests()).is_empty(),
        "no DELETE across the wait->rejoin sequence"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn already_converted_tick_never_renames_back() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["converted_at_ms"] = json!(now_ms - HOUR_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "noop", "{outcome}");
    assert_eq!(reason_of(&outcome), "already_converted", "{outcome}");
    assert_no_mutating_rest(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_channel_is_noop() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["voice_channel_id"] = Value::Null;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "noop", "{outcome}");
    assert_eq!(reason_of(&outcome), "no_channel", "{outcome}");
    assert_no_mutating_rest(&mock.requests());
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Ownership matrix (R8/R9).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ownership_bot_longest_loses() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    let mut bot = occ(BOT_MEMBER, false, now_ms - 2 * HOUR_MS);
    bot["is_bot"] = json!(true);
    snap["occupants"] = json!([bot, occ(HUMAN_MEMBER, false, now_ms - 10 * MINUTE_MS),]);
    snap["presence_spans"] = json!([
        span(BOT_MEMBER, now_ms - 2 * HOUR_MS, None),
        span(HUMAN_MEMBER, now_ms - 10 * MINUTE_MS, None),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(owner_of(&outcome), Some(HUMAN_MEMBER), "{outcome}");
    assert_ne!(
        owner_of(&outcome),
        Some(BOT_MEMBER),
        "a bot never owns: {outcome}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_leaver_longest_loses() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([occ(HUMAN_MEMBER, false, now_ms - 10 * MINUTE_MS)]);
    snap["presence_spans"] = json!([
        span(HUMAN_LEAVER, now_ms - 2 * HOUR_MS, Some(now_ms - HOUR_MS)),
        span(HUMAN_MEMBER, now_ms - 10 * MINUTE_MS, None),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(owner_of(&outcome), Some(HUMAN_MEMBER), "{outcome}");
    assert_ne!(
        owner_of(&outcome),
        Some(HUMAN_LEAVER),
        "anyone absent at the ended tick is ineligible: {outcome}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_kicked_then_rejoined_counts_only_current_spans() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    let mut kicked = occ(HUMAN_MEMBER, false, now_ms - 2 * HOUR_MS);
    kicked["severed_before_ms"] = json!(now_ms - 15 * MINUTE_MS);
    snap["occupants"] = json!([kicked, occ(HUMAN_HOST, true, now_ms - 30 * MINUTE_MS),]);
    snap["presence_spans"] = json!([
        span(
            HUMAN_MEMBER,
            now_ms - 2 * HOUR_MS,
            Some(now_ms - 15 * MINUTE_MS)
        ),
        span(HUMAN_MEMBER, now_ms - 10 * MINUTE_MS, None),
        span(HUMAN_HOST, now_ms - 30 * MINUTE_MS, None),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(
        owner_of(&outcome),
        Some(HUMAN_HOST),
        "the 110 pre-kick minutes must not count; 30 beats 10: {outcome}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_plain_rejoin_sums_spans() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([
        occ(HUMAN_MEMBER, false, now_ms - HOUR_MS),
        occ(HUMAN_HOST, false, now_ms - 40 * MINUTE_MS),
    ]);
    snap["presence_spans"] = json!([
        span(
            HUMAN_MEMBER,
            now_ms - HOUR_MS,
            Some(now_ms - 30 * MINUTE_MS)
        ),
        span(HUMAN_MEMBER, now_ms - 20 * MINUTE_MS, None),
        span(HUMAN_HOST, now_ms - 40 * MINUTE_MS, None),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(
        owner_of(&outcome),
        Some(HUMAN_MEMBER),
        "30 + 20 beats 40: rejoins sum: {outcome}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_tie_prefers_host() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([
        occ(HUMAN_MEMBER, false, now_ms - 30 * MINUTE_MS),
        occ(HUMAN_HOST, true, now_ms - 30 * MINUTE_MS),
    ]);
    snap["presence_spans"] = json!([
        span(HUMAN_MEMBER, now_ms - 30 * MINUTE_MS, None),
        span(HUMAN_HOST, now_ms - 30 * MINUTE_MS, None),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(owner_of(&outcome), Some(HUMAN_HOST), "{outcome}");
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_tie_earliest_join_then_lowest_numeric_id() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    // Equal 30m scores, no host: earliest first-join wins.
    snap["occupants"] = json!([
        occ("3002", false, now_ms - 30 * MINUTE_MS),
        occ("3004", false, now_ms - 40 * MINUTE_MS),
    ]);
    snap["presence_spans"] = json!([
        span("3002", now_ms - 30 * MINUTE_MS, None),
        span(
            "3004",
            now_ms - 40 * MINUTE_MS,
            Some(now_ms - 10 * MINUTE_MS)
        ),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(owner_of(&outcome), Some("3004"), "{outcome}");
    // Exact tie on spans and first-join: lowest NUMERIC id wins. 999 <
    // 1000000 numerically but sorts after it lexicographically.
    snap["occupants"] = json!([
        occ("1000000", false, now_ms - 30 * MINUTE_MS),
        occ("999", false, now_ms - 30 * MINUTE_MS),
    ]);
    snap["presence_spans"] = json!([
        span("1000000", now_ms - 30 * MINUTE_MS, None),
        span("999", now_ms - 30 * MINUTE_MS, None),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(owner_of(&outcome), Some("999"), "{outcome}");
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_zero_rows_falls_back_to_host_if_present() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["presence_spans"] = json!([]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(owner_of(&outcome), Some(HUMAN_HOST), "{outcome}");
    assert_eq!(basis_of(&outcome), Some("fallback_host"), "{outcome}");
    assert_audit(
        &outcome,
        SUMMER_INSTANCE_MS,
        now_ms,
        "convert",
        "ended_occupied",
        Some(HUMAN_HOST),
        Some("fallback_host"),
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_zero_rows_falls_back_to_in_memory() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["presence_spans"] = json!([]);
    snap["occupants"] = json!([occ(HUMAN_MEMBER, false, now_ms - 50 * MINUTE_MS)]);
    // The longest memory entry belongs to an absent user: skipped.
    snap["memory_joins"] = json!([
        mem(HUMAN_LEAVER, now_ms - 90 * MINUTE_MS),
        mem(HUMAN_MEMBER, now_ms - 50 * MINUTE_MS),
    ]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert_eq!(owner_of(&outcome), Some(HUMAN_MEMBER), "{outcome}");
    assert_eq!(basis_of(&outcome), Some("fallback_memory"), "{outcome}");
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_zero_rows_with_no_signal_leaves_pending_first_claimer() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["presence_spans"] = json!([]);
    snap["occupants"] = json!([occ(HUMAN_MEMBER, false, now_ms - 50 * MINUTE_MS)]);
    snap["memory_joins"] = json!([]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    assert!(
        owner_of(&outcome).is_none(),
        "no signal: the room converts owner-pending: {outcome}"
    );
    assert_eq!(
        basis_of(&outcome),
        Some("fallback_claimer_pending"),
        "{outcome}"
    );
    assert_eq!(rename_of(&outcome), Some(RENAME_SUMMER), "{outcome}");
    assert_audit(
        &outcome,
        SUMMER_INSTANCE_MS,
        now_ms,
        "convert",
        "ended_occupied",
        None,
        Some("fallback_claimer_pending"),
    );
    // Memory entries for absent users alone are also no signal.
    snap["memory_joins"] = json!([mem(HUMAN_LEAVER, now_ms - 90 * MINUTE_MS)]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(
        basis_of(&outcome),
        Some("fallback_claimer_pending"),
        "{outcome}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ownership_bots_only_room_takes_empty_path() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    let mut bot = occ(BOT_MEMBER, false, now_ms - 2 * HOUR_MS);
    bot["is_bot"] = json!(true);
    snap["occupants"] = json!([bot]);
    snap["presence_spans"] = json!([span(BOT_MEMBER, now_ms - 2 * HOUR_MS, None)]);
    snap["empty_since_ms"] = json!(now_ms - EMPTY_GRACE_MS - MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_ne!(
        decision_of(&outcome),
        "convert",
        "bots-only is empty: {outcome}"
    );
    assert_eq!(decision_of(&outcome), "delete", "{outcome}");
    assert_eq!(reason_of(&outcome), "empty_grace_elapsed", "{outcome}");
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Rename template (R15).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rename_uses_event_timezone_summer_edt() {
    let (mock, _state) = start_double().await;
    let (snap, now_ms) = base_handoff();
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(rename_of(&outcome), Some(RENAME_SUMMER), "{outcome}");
    let requests = mock.requests();
    let renames = channel_patches(&requests);
    assert_eq!(renames.len(), 1);
    assert_eq!(
        json_body(renames[0]).get("name").and_then(Value::as_str),
        Some(RENAME_SUMMER),
        "the wire rename equals the outcome rename"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn rename_uses_event_timezone_winter_est() {
    let (mock, _state) = start_double().await;
    let (mut snap, _) = base_handoff();
    snap["instance_starts_at_ms"] = json!(WINTER_INSTANCE_MS);
    snap["scheduled_end_ms"] = json!(WINTER_INSTANCE_MS + 2 * HOUR_MS);
    snap["channel_created_ms"] = json!(WINTER_INSTANCE_MS);
    snap["occupants"] = json!([
        occ(HUMAN_HOST, true, WINTER_INSTANCE_MS),
        occ(HUMAN_MEMBER, false, WINTER_INSTANCE_MS + HOUR_MS),
    ]);
    snap["presence_spans"] = json!([
        span(HUMAN_HOST, WINTER_INSTANCE_MS, None),
        span(HUMAN_MEMBER, WINTER_INSTANCE_MS + HOUR_MS, None),
    ]);
    let now_ms = WINTER_NOW_MS;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(rename_of(&outcome), Some(RENAME_WINTER), "{outcome}");
    mock.shutdown().await;
}

#[tokio::test]
async fn rename_uses_utc_for_london_winter() {
    let (mock, _state) = start_double().await;
    let (mut snap, _) = base_handoff();
    snap["instance_starts_at_ms"] = json!(WINTER_INSTANCE_MS);
    snap["scheduled_end_ms"] = json!(WINTER_INSTANCE_MS + 2 * HOUR_MS);
    snap["channel_created_ms"] = json!(WINTER_INSTANCE_MS);
    snap["event_timezone"] = json!("Europe/London");
    snap["occupants"] = json!([
        occ(HUMAN_HOST, true, WINTER_INSTANCE_MS),
        occ(HUMAN_MEMBER, false, WINTER_INSTANCE_MS + HOUR_MS),
    ]);
    snap["presence_spans"] = json!([
        span(HUMAN_HOST, WINTER_INSTANCE_MS, None),
        span(HUMAN_MEMBER, WINTER_INSTANCE_MS + HOUR_MS, None),
    ]);
    let now_ms = WINTER_NOW_MS;
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(rename_of(&outcome), Some(RENAME_UTC), "{outcome}");
    mock.shutdown().await;
}

#[tokio::test]
async fn rename_truncates_slug_to_40_chars() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["event_name"] =
        json!("A Very Long Event Name That Definitely Exceeds Forty Characters Total");
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(rename_of(&outcome), Some(RENAME_TRUNC), "{outcome}");
    mock.shutdown().await;
}

#[tokio::test]
async fn rename_collapses_runs_and_falls_back_on_empty() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["event_name"] = json!("A  B!!C");
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(
        rename_of(&outcome),
        Some("after-party · a-b-c · 0710-1600"),
        "{outcome}"
    );
    snap["event_name"] = json!("!!!");
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(
        rename_of(&outcome),
        Some("after-party · event · 0710-1600"),
        "{outcome}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn rename_appends_short_event_id_on_collision() {
    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["existing_names"] = json!([RENAME_SUMMER]);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(rename_of(&outcome), Some(RENAME_COLLISION), "{outcome}");
    let requests = mock.requests();
    let renames = channel_patches(&requests);
    assert_eq!(renames.len(), 1);
    assert_eq!(
        json_body(renames[0]).get("name").and_then(Value::as_str),
        Some(RENAME_COLLISION)
    );
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Metadata-only convert + audit records (R7/R23 data side).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn convert_rest_is_metadata_only() {
    let (mock, _state) = start_double().await;
    let (snap, now_ms) = base_handoff();
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "convert", "{outcome}");
    let requests = mock.requests();
    assert_convert_rest_clean(&requests, true, RENAME_SUMMER);
    // Belt and suspenders on the no-disconnect rule: no member routes,
    // no overwrites, no extra PATCH targets at all.
    for request in &requests {
        assert!(
            !request.path.contains("/members/"),
            "no member route on convert: {} {}",
            request.method,
            request.path
        );
        assert!(
            !(request.method == "PUT" && request.path.contains("/channels/")),
            "no overwrite PUT on convert: {}",
            request.path
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn convert_and_delete_carry_audit_records() {
    let (mock, _state) = start_double().await;
    let (snap, now_ms) = base_handoff();
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_audit(
        &outcome,
        SUMMER_INSTANCE_MS,
        now_ms,
        "convert",
        "ended_occupied",
        Some(HUMAN_HOST),
        Some("spans"),
    );
    mock.shutdown().await;

    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([]);
    snap["empty_since_ms"] = json!(now_ms - EMPTY_GRACE_MS - 1);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "delete", "{outcome}");
    assert_audit(
        &outcome,
        SUMMER_INSTANCE_MS,
        now_ms,
        "delete",
        "empty_grace_elapsed",
        None,
        None,
    );
    mock.shutdown().await;

    let (mock, _state) = start_double().await;
    let (mut snap, now_ms) = base_handoff();
    snap["occupants"] = json!([]);
    snap["presence_spans"] = json!([]);
    snap["empty_since_ms"] = json!(now_ms - MINUTE_MS);
    let outcome = handoff_tick(&mock, GUILD_S, &snap, now_ms)
        .await
        .expect("handoff succeeds");
    assert_eq!(decision_of(&outcome), "wait", "{outcome}");
    assert!(
        audit_of(&outcome).is_none(),
        "wait carries no audit: {outcome}"
    );
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Capped after-party powers (R11).
// ---------------------------------------------------------------------------

fn power_allowed(response: &Value) -> bool {
    response
        .get("allowed")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("power response lacks allowed: {response}"))
}

fn power_reason(response: &Value) -> &str {
    response
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("power response lacks reason: {response}"))
}

fn power_audit(response: &Value) -> Option<&Value> {
    response.get("audit").filter(|&audit| !audit.is_null())
}

fn assert_power_audit(response: &Value, power: &str, decision: &str, now_ms: i64) {
    let audit = power_audit(response)
        .unwrap_or_else(|| panic!("{power} {decision} must audit: {response}"));
    assert_eq!(
        audit.get("event_id").and_then(Value::as_str),
        Some(EVENT_ID),
        "power audit carries the event id: {audit}"
    );
    assert_eq!(
        audit.get("actor_id").and_then(Value::as_str),
        Some(HUMAN_MEMBER),
        "power audit carries the actor: {audit}"
    );
    assert_eq!(
        audit.get("channel_id").and_then(Value::as_str),
        Some(CHANNEL_ID),
        "power audit carries the channel: {audit}"
    );
    assert_eq!(
        audit.get("power").and_then(Value::as_str),
        Some(power),
        "power audit names the power: {audit}"
    );
    assert_eq!(
        audit.get("decision").and_then(Value::as_str),
        Some(decision),
        "power audit carries the decision: {audit}"
    );
    assert_eq!(
        audit.get("tick_ms").and_then(Value::as_i64),
        Some(now_ms),
        "power audit carries the tick: {audit}"
    );
}

#[test]
fn capped_powers_denied_within_10_min() {
    for power in ["kick", "private"] {
        let (request, now_ms) = base_power(power);
        let response = LiveGuild::check_event_voice_power(&request);
        assert!(!power_allowed(&response), "{power} is capped: {response}");
        assert_eq!(power_reason(&response), "capped_window", "{response}");
        assert_power_audit(&response, power, "denied", now_ms);
    }
}

#[test]
fn kick_allowed_after_10_min() {
    let (mut request, now_ms) = base_power("kick");
    request["converted_at_ms"] = json!(now_ms - CAP_WINDOW_MS - MINUTE_MS);
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(power_allowed(&response), "{response}");
    assert_eq!(power_reason(&response), "window_elapsed", "{response}");
    assert_power_audit(&response, "kick", "allowed", now_ms);
}

#[test]
fn powers_allowed_with_mod_grant() {
    for power in ["kick", "private"] {
        let (mut request, now_ms) = base_power(power);
        request["mod_power_grant"] = json!(true);
        let response = LiveGuild::check_event_voice_power(&request);
        assert!(power_allowed(&response), "{power} with grant: {response}");
        assert_eq!(power_reason(&response), "mod_grant", "{response}");
        assert_power_audit(&response, power, "allowed", now_ms);
    }
    // Power-scope precedes the grant: uncapped powers stay unaudited.
    let (mut request, _) = base_power("name");
    request["mod_power_grant"] = json!(true);
    let response = LiveGuild::check_event_voice_power(&request);
    assert_eq!(power_reason(&response), "uncapped_power", "{response}");
    assert!(power_audit(&response).is_none(), "{response}");
}

#[test]
fn uncapped_powers_allowed_immediately() {
    for power in ["name", "limit", "transfer", "reclaim", "bogus"] {
        let (request, _) = base_power(power);
        let response = LiveGuild::check_event_voice_power(&request);
        assert!(power_allowed(&response), "{power} is uncapped: {response}");
        assert_eq!(power_reason(&response), "uncapped_power", "{response}");
        assert!(
            power_audit(&response).is_none(),
            "uncapped powers audit nothing: {response}"
        );
    }
}

#[test]
fn cap_window_boundary_is_exact() {
    let (mut request, now_ms) = base_power("kick");
    request["converted_at_ms"] = json!(now_ms - CAP_WINDOW_MS + 1);
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(
        !power_allowed(&response),
        "599_999 ms is capped: {response}"
    );
    assert_eq!(power_reason(&response), "capped_window", "{response}");
    assert_power_audit(&response, "kick", "denied", now_ms);
    request["converted_at_ms"] = json!(now_ms - CAP_WINDOW_MS);
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(
        power_allowed(&response),
        "exactly 600_000 ms is allowed: {response}"
    );
    assert_eq!(power_reason(&response), "window_elapsed", "{response}");
    assert_power_audit(&response, "kick", "allowed", now_ms);
    // Clock skew (conversion in the future) denies fail-closed ...
    request["converted_at_ms"] = json!(now_ms + MINUTE_MS);
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(
        !power_allowed(&response),
        "negative elapsed denies: {response}"
    );
    assert_power_audit(&response, "kick", "denied", now_ms);
    // ... unless a mod grant short-circuits the clock.
    request["mod_power_grant"] = json!(true);
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(power_allowed(&response), "grant beats skew: {response}");
    assert_eq!(power_reason(&response), "mod_grant", "{response}");
}

#[test]
fn admin_allowed_with_audit() {
    let (mut request, now_ms) = base_power("private");
    request["is_admin"] = json!(true);
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(power_allowed(&response), "{response}");
    assert_eq!(power_reason(&response), "admin", "{response}");
    assert_power_audit(&response, "private", "allowed", now_ms);
    // Power-scope precedes the admin bypass: uncapped powers stay unaudited.
    request["power"] = json!("name");
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(power_allowed(&response), "{response}");
    assert_eq!(power_reason(&response), "uncapped_power", "{response}");
    assert!(power_audit(&response).is_none(), "{response}");
}

#[test]
fn missing_converted_at_denies_fail_closed() {
    let (mut request, now_ms) = base_power("kick");
    request["converted_at_ms"] = Value::Null;
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(!power_allowed(&response), "{response}");
    assert_eq!(power_reason(&response), "capped_window", "{response}");
    assert_power_audit(&response, "kick", "denied", now_ms);
}

#[test]
fn non_after_party_allowed_without_audit() {
    let (mut request, _) = base_power("kick");
    request["after_party"] = json!(false);
    let response = LiveGuild::check_event_voice_power(&request);
    assert!(power_allowed(&response), "{response}");
    assert_eq!(power_reason(&response), "not_after_party", "{response}");
    assert!(
        power_audit(&response).is_none(),
        "non-after-party audits nothing: {response}"
    );
}

// ---------------------------------------------------------------------------
// Worker controls (existing mechanics the handoff relies on; green now).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn control_rejoin_before_send_cancels_delete_then_deletes_after_releave() {
    let (live, store, mut http, trace) = fixture();
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    store.rooms.lock().unwrap().insert(500, room(500));
    // A join racing the send cancels the delete: the guard re-verifies
    // empty immediately before the Discord DELETE (R6 mechanism). The
    // hook fires once so the re-armed delete below proceeds.
    let shared = live.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_hook = Arc::clone(&fired);
    http.before_delete = Some(Arc::new(move || {
        if !fired_hook.swap(true, Ordering::SeqCst) {
            shared.voice_update(MEMBER, Some(500), Some(false));
        }
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 60_000).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.tracked().contains_key(&500));
    // The room is not wedged: leaving again re-arms the normal delete.
    worker.live.voice_update(MEMBER, None, Some(false));
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 180_000).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
}

#[tokio::test(start_paused = true)]
async fn control_steady_state_empty_room_still_deletes() {
    let (live, store, http, trace) = fixture();
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    store.rooms.lock().unwrap().insert(500, room(500));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 60_000).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
}

#[tokio::test]
async fn control_reclaim_chain_intact_for_converted_rooms() {
    // R10 rides the standard reclaim chain with no event exception: an
    // occupied room whose owner left is reclaimable by a present member.
    let (live, store, http, _) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    let _ = worker.apply_ownership(301, false, OwnershipCommand::Reclaim);
    assert_eq!(worker.tracked().get(&500).expect("room").owner_id, 301);
}

// ---------------------------------------------------------------------------
// Responder + adapter controls (prove the double; green in RED and GREEN).
// ---------------------------------------------------------------------------

#[test]
fn control_responder_scripts_event_patch_rename_delete() {
    let state = Arc::new(Mutex::new(FakeHandoff::new()));
    state
        .lock()
        .expect("fake handoff lock")
        .preset_live(CHANNEL_ID, GUILD_S, "Friday 8s");
    let request = |method: &str, path: &str, body: Value| RestRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers: Vec::new(),
        body: body.to_string().into_bytes(),
        received_at: tokio::time::Instant::now(),
    };
    let patched = respond(
        &state,
        &request(
            "PATCH",
            &format!("/api/v10/guilds/{GUILD_S}/scheduled-events/{EVENT_ID}"),
            json!({"status": "COMPLETED"}),
        ),
    );
    assert_eq!(patched.status, 200);
    let renamed = respond(
        &state,
        &request(
            "PATCH",
            &format!("/api/v10/channels/{CHANNEL_ID}"),
            json!({"name": RENAME_SUMMER}),
        ),
    );
    assert_eq!(renamed.status, 200);
    let deleted = respond(
        &state,
        &request(
            "DELETE",
            &format!("/api/v10/channels/{CHANNEL_ID}"),
            Value::Null,
        ),
    );
    assert_eq!(deleted.status, 204);
    let again = respond(
        &state,
        &request(
            "DELETE",
            &format!("/api/v10/channels/{CHANNEL_ID}"),
            Value::Null,
        ),
    );
    assert_eq!(again.status, 404);
    let unknown = respond(
        &state,
        &request(
            "PUT",
            &format!("/api/v10/channels/{CHANNEL_ID}/permissions/1"),
            json!({}),
        ),
    );
    assert_eq!(unknown.status, 404);
}

#[tokio::test]
async fn control_baseline_handoff_adapter_keep_alive() {
    // Fully-qualified: bypasses the builder's inherent once it lands, so
    // this stays green in GREEN too.
    let (mock, _state) = start_double().await;
    let rest = ActionExecutor::with_proxy(
        "event-voice-handoff-fixture-token".to_owned(),
        Some(mock.origin()),
    )
    .expect("executor builds against the mock");
    let (snap, now_ms) = base_handoff();
    let outcome = <LiveGuild as BaselineEventVoiceHandoff>::run_event_voice_handoff(
        &rest, GUILD_S, &snap, now_ms,
    )
    .await
    .expect("adapter succeeds");
    assert_eq!(decision_of(&outcome), "noop", "{outcome}");
    assert_eq!(reason_of(&outcome), "unwired_baseline", "{outcome}");
    assert!(mock.requests().is_empty(), "the adapter issues no REST");
    mock.shutdown().await;
}

#[test]
fn control_baseline_power_adapter_keep_alive() {
    let (request, _) = base_power("kick");
    let response = <LiveGuild as BaselineEventVoicePowerCheck>::check_event_voice_power(&request);
    assert!(power_allowed(&response), "{response}");
    assert_eq!(power_reason(&response), "unwired_baseline", "{response}");
    assert!(power_audit(&response).is_none(), "{response}");
}
