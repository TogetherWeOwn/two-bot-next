//! Builder-owned pin: the stage/cause metadata on channel-moderation audit
//! rows is part of the documented operator contract, so it is asserted with
//! exact equality here, not `is_object`.
//!
//! Same direct-source harness as the protected
//! `channel_moderation_audit_acceptance` oracle (included unchanged via
//! `#[path = "../src/channel_moderation.rs"] pub mod subject`, with only the
//! persistence and transport boundaries substituted by in-memory fakes), kept
//! in this separate builder-owned file so the protected blob stays
//! byte-identical. No socket, database, environment credential, or
//! source-text assertion is used.
//!
//! Expected states: missing-channel and missing-member refusals each carry
//! exactly `{"stage": "validation", "cause": "missing_context"}`; the
//! in-flight retry row carries exactly `{"stage": "claim", "request_id":
//! "<interaction id>"}` under the `:in_progress` suffixed request id. Row
//! shape, replay and success controls live in the acceptance oracle.
#![cfg(feature = "db")]

extern crate self as sqlx;
extern crate self as two_bot_core;
extern crate two_bot_core as real_core;

pub use real_core::{
    channel_moderation, metrics, ChannelAuditRow, ComponentHandler, ComponentOutcome, HandlerId,
    InteractionHandler, InteractionRouter, LockdownRecord, ModerationAction, RouterGates,
    SlashContext, SlashOutcome,
};
pub use two_bot_discord::{
    route_interaction, ChannelCall, ChannelCallOutcome, DiscordError, EveryoneOverwrite,
    RoutedInteraction,
};

pub mod router {
    pub use real_core::router::*;
}

pub mod executor {
    pub use two_bot_discord::executor::*;
}

/// Merged preview surface for the included `subject`: the core planner
/// names (`two_bot_core::lockdown_preview`, via `extern crate self`) and the
/// discord render/pending names (`crate::lockdown_preview`) share one root
/// name, so both import lists in `channel_moderation.rs` resolve here. The
/// two modules have no overlapping item names.
pub mod lockdown_preview {
    pub use real_core::lockdown_preview::{
        plan_preview, PreviewError, PreviewSnapshot, RoleSnapshot,
    };
    pub use two_bot_discord::lockdown_preview::{
        cancel_custom_id, confirm_custom_id, parse_preview_custom_id, preview_audit_metadata,
        render_preview, PendingEntry, PendingFull, PendingStore, PreviewPress, RateLimiter,
        PREVIEW_EXPIRED, PREVIEW_TTL_SECS,
    };
}

// --- offline sqlx boundary -------------------------------------------------

/// Test-local stand-in for `sqlx::Error`. Only the closed-pool failure used by
/// the nested sibling unit test and generic mismatch failures are modelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl Error {
    fn closed() -> Self {
        Self("pool is closed".to_owned())
    }

    fn mismatch(message: &str) -> Self {
        Self(message.to_owned())
    }
}

/// Test-local stand-in for `sqlx::PgPool`. It can only record whether it was
/// closed; it never opens a socket.
#[derive(Debug, Clone, Default)]
pub struct PgPool {
    closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PgPool {
    pub async fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub mod postgres {
    use super::{Error, PgPool};

    /// Compatibility for the included source's existing closed-pool unit test.
    /// Unlike a lazy SQLx pool, this cannot open a socket at all.
    #[derive(Debug, Default)]
    pub struct PgPoolOptions;

    impl PgPoolOptions {
        pub fn new() -> Self {
            Self
        }

        pub fn connect_lazy(&self, _url: &str) -> Result<PgPool, Error> {
            Ok(PgPool::default())
        }
    }
}

// --- offline store boundary -----------------------------------------------

/// Test-local substitute for the real idempotency-claim outcome. The real
/// ticket fields are private, so the subject can only pass tickets opaquely;
/// this mirror keeps that property (fields are private here too).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelClaim {
    Claimed {
        ticket: ChannelClaimTicket,
    },
    Replayed {
        outcome: String,
        result_json: String,
    },
    InFlight,
    Mismatch,
}

/// Test-local substitute for the real claim ticket. Field privacy mirrors the
/// production type: only the store implementation below may inspect them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelClaimTicket {
    guild_id: String,
    idempotency_key: String,
    claim_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimState {
    InFlight,
    Done,
}

#[derive(Debug, Clone)]
struct ClaimEntry {
    action: String,
    hash: String,
    state: ClaimState,
    outcome: String,
    result_json: String,
    token: String,
}

#[derive(Debug, Default)]
struct StoreState {
    claims: std::collections::HashMap<(String, String), ClaimEntry>,
    lanes: std::collections::HashMap<String, (String, String, String)>,
    audits: Vec<ChannelAuditRow>,
    lockdowns: std::collections::HashMap<String, LockdownRecord>,
    next_token: u64,
    closed: bool,
}

/// In-memory substitute for `ChannelModerationStore` with the exact method
/// surface the subject uses. Claim/lane/audit semantics mirror the production
/// SQL (`INSERT ... ON CONFLICT DO NOTHING`, token-fenced lanes, request-id
/// audit uniqueness). A closed pool fails every attempted write instead of
/// pretending durable success.
#[derive(Debug, Clone)]
pub struct ChannelModerationStore {
    state: std::sync::Arc<std::sync::Mutex<StoreState>>,
}

impl ChannelModerationStore {
    pub fn memory() -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(StoreState::default())),
        }
    }

    pub fn from_pool(pool: PgPool) -> Self {
        let state = StoreState {
            closed: pool.is_closed(),
            ..StoreState::default()
        };
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(state)),
        }
    }

    pub fn audits(&self) -> Vec<ChannelAuditRow> {
        self.state.lock().expect("offline store").audits.clone()
    }

    pub async fn claim(
        &self,
        guild_id: &str,
        idempotency_key: &str,
        action: &str,
        request_hash: &str,
        _claimed_at: &str,
    ) -> Result<ChannelClaim, Error> {
        let mut state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        let key = (guild_id.to_owned(), idempotency_key.to_owned());
        if let Some(existing) = state.claims.get(&key).cloned() {
            if existing.action != action {
                return Ok(ChannelClaim::Mismatch);
            }
            if existing.state == ClaimState::Done && existing.outcome == "operator_released" {
                return Ok(ChannelClaim::Mismatch);
            }
            if existing.hash != request_hash {
                return Ok(ChannelClaim::Mismatch);
            }
            if existing.state == ClaimState::Done {
                return Ok(ChannelClaim::Replayed {
                    outcome: existing.outcome,
                    result_json: existing.result_json,
                });
            }
            return Ok(ChannelClaim::InFlight);
        }
        state.next_token += 1;
        let token = format!("offline-token-{}", state.next_token);
        state.claims.insert(
            key,
            ClaimEntry {
                action: action.to_owned(),
                hash: request_hash.to_owned(),
                state: ClaimState::InFlight,
                outcome: String::new(),
                result_json: String::new(),
                token: token.clone(),
            },
        );
        Ok(ChannelClaim::Claimed {
            ticket: ChannelClaimTicket {
                guild_id: guild_id.to_owned(),
                idempotency_key: idempotency_key.to_owned(),
                claim_token: token,
            },
        })
    }

    pub async fn claim_channel(
        &self,
        ticket: &ChannelClaimTicket,
        channel_id: &str,
    ) -> Result<bool, Error> {
        let mut state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        let key = (ticket.guild_id.clone(), ticket.idempotency_key.clone());
        let owned = state.claims.get(&key).is_some_and(|entry| {
            entry.token == ticket.claim_token && entry.state == ClaimState::InFlight
        });
        if !owned {
            return Ok(false);
        }
        if state.lanes.contains_key(channel_id) {
            return Ok(false);
        }
        state.lanes.insert(
            channel_id.to_owned(),
            (
                ticket.guild_id.clone(),
                ticket.idempotency_key.clone(),
                ticket.claim_token.clone(),
            ),
        );
        Ok(true)
    }

    pub async fn release(&self, ticket: &ChannelClaimTicket) -> Result<bool, Error> {
        let mut state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        let key = (ticket.guild_id.clone(), ticket.idempotency_key.clone());
        let owned = state.claims.get(&key).is_some_and(|entry| {
            entry.token == ticket.claim_token && entry.state == ClaimState::InFlight
        });
        if !owned {
            return Ok(false);
        }
        state.lanes.retain(|_, held| {
            !(held.0 == ticket.guild_id
                && held.1 == ticket.idempotency_key
                && held.2 == ticket.claim_token)
        });
        state.claims.remove(&key);
        Ok(true)
    }

    pub async fn record_audit(&self, row: &ChannelAuditRow) -> Result<(), Error> {
        let mut state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        if state.audits.iter().any(|r| r.request_id == row.request_id) {
            return Ok(());
        }
        state.audits.push(row.clone());
        Ok(())
    }

    /// Insert-or-nothing mirror of the production `try_record_audit`
    /// (`ON CONFLICT (request_id) DO NOTHING`): the preview arms only when
    /// exactly one row is created, so a redelivered retry reports `false`.
    pub async fn try_record_audit(&self, row: &ChannelAuditRow) -> Result<bool, Error> {
        let mut state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        if state.audits.iter().any(|r| r.request_id == row.request_id) {
            return Ok(false);
        }
        state.audits.push(row.clone());
        Ok(true)
    }

    pub async fn finish(
        &self,
        ticket: &ChannelClaimTicket,
        row: &ChannelAuditRow,
        result_json: &str,
        restored: Option<&LockdownRecord>,
    ) -> Result<bool, Error> {
        let mut state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        if row.guild_id != ticket.guild_id || row.idempotency_key != ticket.idempotency_key {
            return Err(Error::mismatch("audit does not match claim"));
        }
        if restored.is_some_and(|rec| {
            rec.guild_id != ticket.guild_id
                || row.channel_id.as_deref() != Some(rec.channel_id.as_str())
        }) {
            return Err(Error::mismatch("recovery does not match claim"));
        }
        let key = (ticket.guild_id.clone(), ticket.idempotency_key.clone());
        let current = state.claims.get(&key).cloned();
        let Some(entry) = current else {
            return Ok(false);
        };
        if entry.token != ticket.claim_token
            || entry.state != ClaimState::InFlight
            || entry.action != row.action
        {
            return Ok(false);
        }
        if let Some(rec) = restored {
            let live = state.lockdowns.get(&rec.channel_id).cloned();
            if live.as_ref() != Some(rec) {
                return Ok(false);
            }
        }
        let entry = state.claims.get_mut(&key).expect("checked claim");
        entry.state = ClaimState::Done;
        entry.outcome = row.outcome.clone();
        entry.result_json = result_json.to_owned();
        if !state.audits.iter().any(|r| r.request_id == row.request_id) {
            state.audits.push(row.clone());
        }
        if let Some(rec) = restored {
            state.lockdowns.remove(&rec.channel_id);
        }
        state.lanes.retain(|_, held| {
            !(held.0 == ticket.guild_id
                && held.1 == ticket.idempotency_key
                && held.2 == ticket.claim_token)
        });
        Ok(true)
    }

    pub async fn get_lockdown(&self, channel_id: &str) -> Result<Option<LockdownRecord>, Error> {
        let state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        Ok(state.lockdowns.get(channel_id).cloned())
    }

    pub async fn get_lockdown_details(
        &self,
        _record: &LockdownRecord,
    ) -> Result<Option<(String, Option<String>)>, Error> {
        panic!("lockdown metadata reads are outside this offline acceptance slice")
    }

    pub async fn record_lockdown(
        &self,
        channel_id: &str,
        guild_id: &str,
        seed: &real_core::channel_moderation::LockdownSeed,
        reason: &str,
        _locked_at: &str,
    ) -> Result<LockdownRecord, Error> {
        let mut state = self.state.lock().expect("offline store");
        if state.closed {
            return Err(Error::closed());
        }
        let live_deny = seed
            .prior_deny
            .parse::<u64>()
            .map_err(|_| Error::mismatch("invalid lockdown permission mask"))?;
        let refresh = live_deny & real_core::channel_moderation::SEND_MESSAGES_BIT == 0;
        if let Some(existing) = state.lockdowns.get(channel_id).cloned() {
            if !refresh {
                let mut kept = existing;
                kept.reason = reason.to_owned();
                state.lockdowns.insert(channel_id.to_owned(), kept.clone());
                return Ok(kept);
            }
        }
        state.next_token += 1;
        let rec = LockdownRecord {
            channel_id: channel_id.to_owned(),
            guild_id: guild_id.to_owned(),
            prior_allow: seed.prior_allow.clone(),
            prior_deny: seed.prior_deny.clone(),
            prior_exists: seed.prior_exists,
            reason: reason.to_owned(),
            recovery_generation: format!("offline-gen-{}", state.next_token),
        };
        state.lockdowns.insert(channel_id.to_owned(), rec.clone());
        Ok(rec)
    }
}

// --- offline transport boundary --------------------------------------------

/// Shape-only mirror of the executor's crate-private purge sample. Read paths
/// must compile with the included source but must never run in this harness.
pub(crate) struct PurgeSample {
    pub ids: Vec<twilight_model::id::Id<twilight_model::id::marker::MessageMarker>>,
    pub listed: usize,
    pub pinned: usize,
    pub own_bot: usize,
    pub system: usize,
}

#[derive(Debug, Default)]
struct Transport {
    mutations: Vec<ChannelCall>,
    failures: std::collections::VecDeque<DiscordError>,
}

/// Pure I/O fake for `ActionExecutor`. It records every executed channel
/// mutation and replays one scripted failure at a time; unscripted slowmode
/// calls succeed. Purge and lockdown reads are outside this acceptance slice
/// and panic instead of faking business logic.
#[derive(Debug, Clone, Default)]
pub struct ActionExecutor {
    inner: std::sync::Arc<std::sync::Mutex<Transport>>,
}

impl ActionExecutor {
    pub fn with_proxy(_token: String, _proxy: Option<String>) -> Result<Self, String> {
        Ok(Self::default())
    }

    pub fn fail_next(&self, error: DiscordError) {
        self.inner
            .lock()
            .expect("offline transport")
            .failures
            .push_back(error);
    }

    pub fn mutations(&self) -> Vec<ChannelCall> {
        self.inner
            .lock()
            .expect("offline transport")
            .mutations
            .clone()
    }

    pub async fn answer_interaction_with_blocked_retry(
        &self,
        _interaction_id: u64,
        _interaction_token: &str,
        _response: &twilight_model::http::interaction::InteractionResponse,
    ) -> Result<(), DiscordError> {
        Ok(())
    }

    pub async fn edit_interaction_response_with_blocked_retry(
        &self,
        _application_id: u64,
        _interaction_token: &str,
        _content: &str,
    ) -> Result<(), DiscordError> {
        Ok(())
    }

    pub async fn edit_interaction_response_without_mention_neutralization_with_blocked_retry(
        &self,
        _application_id: u64,
        _interaction_token: &str,
        _content: &str,
    ) -> Result<(), DiscordError> {
        panic!("read-command replies are outside this offline acceptance slice")
    }

    pub async fn edit_interaction_response_with_components_and_blocked_retry(
        &self,
        _application_id: u64,
        _interaction_token: &str,
        _content: &str,
        _components: &[twilight_model::channel::message::Component],
    ) -> Result<(), DiscordError> {
        panic!("preview renders are outside this offline acceptance slice")
    }

    pub async fn read_preview_channel(
        &self,
        _channel_id: &str,
        _guild_id: &str,
    ) -> Result<two_bot_discord::executor::PreviewChannel, DiscordError> {
        panic!("lockdown preview reads are outside this offline acceptance slice")
    }

    pub async fn read_preview_roles(
        &self,
        _guild_id: &str,
    ) -> Result<Vec<real_core::lockdown_preview::RoleSnapshot>, DiscordError> {
        panic!("lockdown preview reads are outside this offline acceptance slice")
    }

    pub async fn fetch_channel_document(
        &self,
        _channel_id: &str,
    ) -> Result<serde_json::Value, DiscordError> {
        panic!("channel document reads are outside this offline acceptance slice")
    }

    pub(crate) async fn sample_purge_messages(
        &self,
        _channel_id: &str,
        _count: u64,
    ) -> Result<PurgeSample, DiscordError> {
        panic!("purge sample reads are outside this offline acceptance slice")
    }

    pub async fn execute_channel(
        &self,
        call: &ChannelCall,
    ) -> Result<ChannelCallOutcome, DiscordError> {
        let failure = {
            let mut transport = self.inner.lock().expect("offline transport");
            transport.mutations.push(call.clone());
            transport.failures.pop_front()
        };
        if let Some(error) = failure {
            return Err(error);
        }
        match call {
            ChannelCall::Slowmode { .. } => Ok(ChannelCallOutcome::SlowmodeUpdated),
            _ => panic!("purge/lockdown calls are outside this offline acceptance slice"),
        }
    }

    pub async fn list_purge_messages(
        &self,
        _channel_id: &str,
        _count: u64,
    ) -> Result<Vec<twilight_model::id::Id<twilight_model::id::marker::MessageMarker>>, DiscordError>
    {
        panic!("purge reads are outside this offline acceptance slice")
    }

    pub async fn purge_messages(
        &self,
        _channel_id: &str,
        _ids: &[twilight_model::id::Id<twilight_model::id::marker::MessageMarker>],
        _reason: &str,
    ) -> Result<real_core::channel_moderation::PurgeReport, DiscordError> {
        panic!("purge deletes are outside this offline acceptance slice")
    }

    pub async fn get_everyone_overwrite(
        &self,
        _channel_id: &str,
        _guild_id: &str,
    ) -> Result<Option<EveryoneOverwrite>, DiscordError> {
        panic!("lockdown reads are outside this offline acceptance slice")
    }
}

// --- production orchestration under test ------------------------------------

#[path = "../src/channel_moderation.rs"]
pub mod subject;

// --- fixtures ---------------------------------------------------------------

use serde_json::{json, Value};
use twilight_model::application::interaction::Interaction;

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const ACTOR: &str = "333333333333333333";
const PERMISSIONS: u64 = (1 << 4) | (1 << 13);

const MISSING_CONTEXT_COPY: &str = "A guild member and channel are required.";

fn router() -> InteractionRouter {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        moderation: true,
        voice: false,
        voice_assistant: false,
        scorecard: false,
        automations: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    subject::register_channel_handlers(&mut router);
    router
}

fn interaction(id: u64, action: &str, options: Value, permissions: u64) -> Interaction {
    interaction_shaped(id, action, options, permissions, true, true)
}

/// A synthetic slowmode-class interaction with member and/or channel context
/// present (`false` clears that side of the context).
fn interaction_shaped(
    id: u64,
    action: &str,
    options: Value,
    permissions: u64,
    include_member: bool,
    include_channel: bool,
) -> Interaction {
    let member: Value = if include_member {
        json!({"user": {"id": ACTOR, "username": "test-member", "discriminator": "0", "avatar": null},
               "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": permissions.to_string()})
    } else {
        Value::Null
    };
    let channel: Value = if include_channel {
        json!({"id": CHANNEL, "type": 0, "permissions": permissions.to_string()})
    } else {
        Value::Null
    };
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "444444444444444444", "type": 2,
        "guild_id": GUILD, "channel": channel,
        "data": {"id": "555555555555555555", "type": 1, "name": action, "options": options},
        "member": member,
        "token": "synthetic-interaction-token", "version": 1, "locale": "en-US", "guild_locale": "en-US",
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": []
    }))
    .expect("synthetic Twilight interaction")
}

fn options(numeric: Option<(&str, i64)>) -> Value {
    let mut options = vec![json!({"name":"reason", "type":3, "value":"mock audit reason"})];
    if let Some((name, number)) = numeric {
        options.push(json!({"name":name, "type":4, "value":number}));
    }
    json!(options)
}

fn harness() -> (
    ChannelModerationStore,
    ActionExecutor,
    subject::ChannelModerationRuntime,
) {
    let store = ChannelModerationStore::memory();
    let executor =
        ActionExecutor::with_proxy("synthetic-test-token".to_owned(), None).expect("fake executor");
    let runtime = subject::ChannelModerationRuntime::new(store.clone(), executor.clone());
    (store, executor, runtime)
}

fn slowmode(id: u64) -> Interaction {
    interaction(id, "slowmode", options(Some(("seconds", 10))), PERMISSIONS)
}

// --- terminal outcomes --------------------------------------------------------

#[tokio::test]
async fn missing_channel_refusal_pins_exact_validation_metadata() {
    let (store, executor, runtime) = harness();
    let request = interaction_shaped(
        81,
        "slowmode",
        options(Some(("seconds", 10))),
        PERMISSIONS,
        true,
        false,
    );
    let reply = runtime
        .execute(&router(), &request)
        .await
        .expect("channel slice answers slowmode")
        .expect("channel slice owns slowmode");
    assert_eq!(reply.outcome, "refused");
    assert_eq!(reply.text, MISSING_CONTEXT_COPY);
    assert!(!reply.replayed);
    assert!(
        executor.mutations().is_empty(),
        "no Discord mutation precedes context"
    );

    let rows = store.audits();
    assert_eq!(rows.len(), 1, "one refused row for missing channel");
    let row = &rows[0];
    assert_eq!(row.request_id, "81");
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.actor_id, ACTOR);
    assert_eq!(row.action, "moderation.slowmode");
    assert!(row.channel_id.is_none(), "no channel was in context");
    assert_eq!(row.reason, MISSING_CONTEXT_COPY);
    assert_eq!(row.outcome, "refused");
    assert_eq!(row.idempotency_key, "81");
    let metadata: Value = serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        json!({"stage": "validation", "cause": "missing_context"})
    );
}

#[tokio::test]
async fn missing_member_refusal_pins_exact_validation_metadata() {
    let (store, executor, runtime) = harness();
    let request = interaction_shaped(
        82,
        "slowmode",
        options(Some(("seconds", 10))),
        PERMISSIONS,
        false,
        true,
    );
    let reply = runtime
        .execute(&router(), &request)
        .await
        .expect("channel slice answers slowmode")
        .expect("channel slice owns slowmode");
    assert_eq!(reply.outcome, "refused");
    assert_eq!(reply.text, MISSING_CONTEXT_COPY);
    assert!(!reply.replayed);
    assert!(
        executor.mutations().is_empty(),
        "no Discord mutation precedes context"
    );

    // The actor is unknown here, so only the known bindings are pinned; the
    // missing-channel test above pins the full row shape.
    let rows = store.audits();
    assert_eq!(rows.len(), 1, "one refused row for missing member");
    let row = &rows[0];
    assert_eq!(row.request_id, "82");
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.action, "moderation.slowmode");
    assert_eq!(row.channel_id.as_deref(), Some(CHANNEL));
    assert_eq!(row.reason, MISSING_CONTEXT_COPY);
    assert_eq!(row.outcome, "refused");
    assert_eq!(row.idempotency_key, "82");
    let metadata: Value = serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(
        metadata,
        json!({"stage": "validation", "cause": "missing_context"})
    );
}

#[tokio::test]
async fn inflight_retry_pins_exact_claim_metadata() {
    let (store, executor, runtime) = harness();
    let router = router();
    let request = slowmode(83);

    // An unreachable Discord proves nothing about the mutation, so the
    // uncertain path audits `in_progress` (GREEN control: audited today).
    executor.fail_next(DiscordError::Unavailable("offline 503".to_owned()));
    let first = runtime
        .execute(&router, &request)
        .await
        .expect("channel slice answers slowmode")
        .expect("channel slice owns slowmode");
    assert_eq!(first.outcome, "in_progress");
    assert!(!first.replayed);
    assert_eq!(store.audits().len(), 1);
    assert_eq!(executor.mutations().len(), 1);

    // Same interaction retried: the held claim answers in-flight without a
    // second Discord call, and that answer leaves its own audit row. The
    // request id must differ from the first row's plain id, or the
    // `ON CONFLICT DO NOTHING` insert silently drops it.
    let second = runtime
        .execute(&router, &request)
        .await
        .expect("channel slice answers the retry")
        .expect("channel slice owns the retry");
    assert_eq!(second.outcome, "in_progress");
    assert!(!second.replayed);
    assert_eq!(
        executor.mutations().len(),
        1,
        "in-flight retry makes no Discord call"
    );
    let mut rows = store.audits();
    rows.sort_by(|a, b| a.request_id.cmp(&b.request_id));
    assert_eq!(rows.len(), 2, "uncertain row plus in-flight row");
    let row = rows
        .iter()
        .find(|row| row.request_id != "83")
        .expect("in-flight row carries a distinct request id");
    assert!(
        row.request_id.starts_with("83"),
        "in-flight row stays bound to the request, got {}",
        row.request_id
    );
    assert_eq!(row.guild_id, GUILD);
    assert_eq!(row.actor_id, ACTOR);
    assert_eq!(row.action, "moderation.slowmode");
    assert_eq!(row.channel_id.as_deref(), Some(CHANNEL));
    assert_eq!(row.outcome, "in_progress");
    assert_eq!(row.idempotency_key, "83");
    assert_eq!(row.request_id, "83:in_progress");
    let metadata: Value = serde_json::from_str(&row.metadata_json).expect("audit metadata is JSON");
    assert_eq!(metadata, json!({"stage": "claim", "request_id": "83"}));
}
