//! Automod activation over the shared REST executor (TOG-10261).
//!
//! Each test drives `AutomodActivation` against the scripted mock REST double
//! and asserts on the recorded wire requests. The in-memory ledger mirrors the
//! `AutomodStore` SQL guards; the store itself is covered by the ignored DB
//! tests on agent-testdb/CI containers. No production or staging target.

#[allow(dead_code)]
mod common;

use std::collections::{HashMap, HashSet};
use std::future::{ready, Future, Ready};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use twilight_model::{
    channel::Message,
    gateway::{event::Event, payload::incoming::MessageCreate},
};
use two_bot_core::automod_runtime::{
    AutomodClaimLedger, AutomodMatch, AutomodRuntime, AutomodScope, CompletionKind, DeliveryKey,
    FunnelDisposition, LedgerClaim, MessageDelivery, MessageDeliveryKind, MessageSubject,
    StoredOutcome, TargetFacts, ViolationRecord, STAGING_GUILD_ID,
};
use two_bot_core::commands::PERM_MODERATE_MEMBERS;
use two_bot_core::{
    AutomodConfig, AutomodFilter, FactsSink, LevelOutcome, LevelingHook, MemStore, MemberJoinFact,
    MessageFact, ModerationPolicy, ModerationTarget, RulesAcceptedFact, VoiceEndedFact,
    VoiceStartedFact,
};
use two_bot_discord::automod::{event_to_automod, partial_edit_delivery, PartialEdit};
use two_bot_discord::automod_activation::{
    Activation, ActivationOutcome, AutomodActivation, AutomodFacts, FetchedMessage,
    RestAutomodFacts, RetainReason,
};
use two_bot_discord::{ActionExecutor, NoClassification, NoInvites, Pipeline};

const CHANNEL: &str = "222222222222222222";
const AUTHOR: &str = "444444444444444444";
const AUTHOR_ROLE: &str = "555555555555555555";
const BOT: &str = "999999999999999999";
const BOT_ROLE: &str = "777777777777777777";
const OWNER: &str = "111111111111111111";
const AT: &str = "2026-10-02T00:00:00.000Z";

// ---------------------------------------------------------------- ledger fake

#[derive(Default)]
struct Row {
    token: u64,
    result: Option<StoredOutcome>,
    matched: Option<AutomodMatch>,
    released: bool,
    started: bool,
    counted: bool,
}

#[derive(Default)]
struct LedgerState {
    rows: HashMap<String, Row>,
    processed: HashMap<(String, String), String>,
    violations: HashMap<(String, String), u64>,
    next_token: u64,
}

#[derive(Default)]
struct LedgerInner {
    state: Mutex<LedgerState>,
    refuse_fence: AtomicBool,
}

/// Mirrors the `AutomodStore` guards: owner-gated tokens, dry-run refusal,
/// counted/started claims never released, insert-first counting.
#[derive(Clone, Default)]
struct MemLedger(Arc<LedgerInner>);

#[derive(Debug, Clone)]
struct MemClaim {
    id: String,
    token: u64,
    dry_run: bool,
}

impl MemLedger {
    fn seed_violations(&self, count: u64) {
        let mut state = self.0.state.lock().unwrap();
        state
            .violations
            .insert((STAGING_GUILD_ID.into(), AUTHOR.into()), count);
    }

    fn violations(&self) -> u64 {
        let state = self.0.state.lock().unwrap();
        state
            .violations
            .get(&(STAGING_GUILD_ID.into(), AUTHOR.into()))
            .copied()
            .unwrap_or(0)
    }

    fn claims(&self) -> usize {
        self.0.state.lock().unwrap().rows.len()
    }

    fn with_row<T>(&self, claim: &MemClaim, f: impl FnOnce(&mut Row) -> T) -> Option<T> {
        let mut state = self.0.state.lock().unwrap();
        let row = state.rows.get_mut(&claim.id)?;
        if row.token != claim.token {
            return None;
        }
        Some(f(row))
    }
}

type Done<T> = Ready<Result<T, String>>;

fn done<T>(result: Result<T, String>) -> Done<T> {
    ready(result)
}

fn row_id(key: &DeliveryKey) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        key.guild_id,
        key.message_id,
        key.kind_name(),
        key.dry_run,
        key.request_hash
    )
}

fn active(row: &Row) -> bool {
    row.result.is_none() && !row.released && !row.started && !row.counted
}

impl AutomodClaimLedger for MemLedger {
    type Claim = MemClaim;
    type Error = String;

    fn ledger_claim(
        &self,
        key: &DeliveryKey,
    ) -> impl Future<Output = Result<LedgerClaim<MemClaim>, String>> + Send {
        let id = row_id(key);
        let mut state = self.0.state.lock().unwrap();
        state.next_token += 1;
        let token = state.next_token;
        let claim = MemClaim {
            id: id.clone(),
            token,
            dry_run: key.dry_run,
        };
        let existing = match state.rows.get_mut(&id) {
            None => None,
            Some(Row {
                result: Some(stored),
                ..
            }) => Some(LedgerClaim::Replayed(stored.clone())),
            Some(row) if row.released && !row.started && !row.counted => {
                row.token = token;
                row.released = false;
                let matched = row.matched.clone().expect("released rows keep a decision");
                Some(LedgerClaim::Preserved(claim.clone(), matched))
            }
            Some(_) => Some(LedgerClaim::InFlight),
        };
        let result = existing.unwrap_or_else(|| {
            let row = Row {
                token,
                ..Row::default()
            };
            state.rows.insert(id, row);
            LedgerClaim::Acquired(claim)
        });
        done(Ok(result))
    }

    fn ledger_preserve(
        &self,
        claim: &MemClaim,
        matched: &AutomodMatch,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        let result = if claim.dry_run {
            Err("dry-run claim cannot preserve".to_owned())
        } else {
            Ok(self
                .with_row(claim, |row| {
                    let ok = active(row);
                    if ok {
                        row.matched = Some(matched.clone());
                    }
                    ok
                })
                .unwrap_or(false))
        };
        done(result)
    }

    fn ledger_mark_started(
        &self,
        claim: &MemClaim,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        let refuse = self.0.refuse_fence.load(Ordering::SeqCst);
        let started = self
            .with_row(claim, |row| {
                let ok = !refuse && row.result.is_none() && !row.released;
                if ok {
                    row.started = true;
                }
                ok
            })
            .unwrap_or(false);
        done(Ok(started))
    }

    fn ledger_complete(
        &self,
        claim: &MemClaim,
        outcome: &StoredOutcome,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        if claim.dry_run
            && (outcome.deleted
                || !matches!(
                    outcome.outcome,
                    CompletionKind::Accepted | CompletionKind::DryRun
                ))
        {
            return done(Err("dry-run claim cannot complete a mutation".to_owned()));
        }
        let settled = self
            .with_row(claim, |row| {
                let ok = row.result.is_none() && !row.released;
                if ok {
                    row.result = Some(outcome.clone());
                }
                ok
            })
            .unwrap_or(false);
        done(Ok(settled))
    }

    fn ledger_release(
        &self,
        claim: &MemClaim,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        let mut state = self.0.state.lock().unwrap();
        let decided = state
            .rows
            .get(&claim.id)
            .filter(|row| row.token == claim.token && active(row))
            .map(|row| row.matched.is_some());
        match decided {
            Some(true) => state.rows.get_mut(&claim.id).unwrap().released = true,
            Some(false) => {
                state.rows.remove(&claim.id);
            }
            None => {}
        }
        done(Ok(decided.is_some()))
    }

    fn ledger_record(
        &self,
        claim: &MemClaim,
        subject: &MessageSubject,
        _filter: AutomodFilter,
        _at_iso: &str,
    ) -> impl Future<Output = Result<ViolationRecord, String>> + Send {
        let mut state = self.0.state.lock().unwrap();
        let ok = !claim.dry_run
            && state
                .rows
                .get(&claim.id)
                .is_some_and(|row| row.token == claim.token && active(row));
        if !ok {
            return done(Err(
                "violation requires an active unmutated claim".to_owned()
            ));
        }
        let message = (subject.guild_id.clone(), subject.message_id.clone());
        let inserted = !state.processed.contains_key(&message);
        let user = if inserted {
            state.processed.insert(message, subject.author_id.clone());
            subject.author_id.clone()
        } else {
            state.processed[&message].clone()
        };
        let count = state
            .violations
            .entry((subject.guild_id.clone(), user))
            .or_insert(0);
        if inserted {
            *count += 1;
        }
        let count = *count;
        state.rows.get_mut(&claim.id).unwrap().counted = true;
        done(Ok(ViolationRecord { count, inserted }))
    }
}

// ----------------------------------------------------------------- facts fake

#[derive(Default)]
struct FactsInner {
    target: Mutex<Option<TargetFacts>>,
    target_calls: AtomicUsize,
}

#[derive(Clone, Default)]
struct StaticFacts(Arc<FactsInner>);

impl StaticFacts {
    fn allowed() -> Self {
        let facts = Self::default();
        facts.set(Some(target_facts(false, 1)));
        facts
    }

    fn set(&self, facts: Option<TargetFacts>) {
        *self.0.target.lock().unwrap() = facts;
    }

    fn target_calls(&self) -> usize {
        self.0.target_calls.load(Ordering::SeqCst)
    }
}

impl AutomodFacts for StaticFacts {
    fn fetch_message(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> impl Future<Output = Option<FetchedMessage>> + Send {
        ready(None)
    }

    fn target_facts(&self, _: &MessageSubject) -> impl Future<Output = Option<TargetFacts>> + Send {
        self.0.target_calls.fetch_add(1, Ordering::SeqCst);
        ready(self.0.target.lock().unwrap().clone())
    }
}

fn target_facts(is_guild_owner: bool, target_position: i64) -> TargetFacts {
    TargetFacts {
        target: ModerationTarget {
            user_id: AUTHOR.into(),
            role_ids: vec![AUTHOR_ROLE.into(), STAGING_GUILD_ID.into()],
            highest_role_position: target_position,
            is_bot: false,
            is_guild_owner,
        },
        policy: ModerationPolicy {
            owen_user_id: OWNER.into(),
            protected_role_ids: HashSet::new(),
            bot_user_id: Some(BOT.into()),
        },
        bot_highest_role_position: 5,
        bot_permissions: PERM_MODERATE_MEMBERS | (1 << 13),
    }
}

// -------------------------------------------------------------------- helpers

fn runtime(enforce: bool) -> AutomodRuntime {
    let mut vars = HashMap::from([
        ("TWO_AUTOMOD".to_owned(), "1".to_owned()),
        ("TWO_AUTOMOD_BAD_WORDS".to_owned(), "blocked".to_owned()),
    ]);
    if enforce {
        vars.insert("TWO_AUTOMOD_ENFORCE".to_owned(), "1".to_owned());
    }
    AutomodRuntime::new(
        AutomodConfig::from_map(&vars).unwrap(),
        AutomodScope {
            guild_id: STAGING_GUILD_ID.into(),
            live_approved: false,
        },
    )
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("automod-activation-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

fn message_json(id: &str, content: &str) -> serde_json::Value {
    json!({
        "id": id, "channel_id": CHANNEL, "guild_id": STAGING_GUILD_ID, "type": 0,
        "content": content,
        "author": {"id": AUTHOR, "username": "mock", "discriminator": "0", "avatar": null, "bot": false},
        "member": {"roles": [AUTHOR_ROLE], "joined_at": null, "deaf": false, "mute": false, "flags": 0},
        "timestamp": "2026-10-02T00:00:00.000000+00:00", "edited_timestamp": null,
        "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "tts": false
    })
}

fn create_event(id: &str, content: &str) -> Event {
    let message: Message = serde_json::from_value(message_json(id, content)).unwrap();
    Event::MessageCreate(Box::new(MessageCreate(message)))
}

fn create(id: &str, content: &str) -> MessageDelivery {
    event_to_automod(&create_event(id, content), 1_790_726_400_000).unwrap()
}

fn delete_path(id: &str) -> String {
    format!("/api/v10/channels/{CHANNEL}/messages/{id}")
}

fn calls(mock: &MockRest) -> Vec<(String, String)> {
    mock.requests()
        .into_iter()
        .map(|request| (request.method, request.path))
        .collect()
}

fn completed(deleted: bool, outcome: CompletionKind) -> ActivationOutcome {
    ActivationOutcome::Completed(StoredOutcome {
        matched: true,
        deleted,
        outcome,
    })
}

#[derive(Clone, Default)]
struct Awards(Arc<AtomicUsize>);

impl FactsSink for Awards {
    fn record_member_join(&self, _: MemberJoinFact<'_>) {}
    fn record_rules_accepted(&self, _: RulesAcceptedFact<'_>) {}
    fn record_message(&self, _: MessageFact<'_>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn record_voice_started(&self, _: VoiceStartedFact<'_>) -> Option<String> {
        None
    }
    fn record_voice_ended(&self, _: VoiceEndedFact<'_>) {}
}

impl LevelingHook for Awards {
    fn award_message(&self, _: u64, _: u64, _: &str, _: u64) -> LevelOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }
    fn award_voice(&self, _: u64, _: u64, _: u64, _: &str, _: u64) -> LevelOutcome {
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }
}

fn assert_send<T: Send>(_: &T) {}

// ---------------------------------------------------------------------- tests

#[tokio::test]
async fn dry_run_match_captures_only_and_sends_nothing() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let (ledger, facts) = (MemLedger::default(), StaticFacts::allowed());
    let activation = AutomodActivation::new(
        runtime(false),
        ledger.clone(),
        facts.clone(),
        executor(&mock),
    );

    let processing = activation.process(create("1001", "blocked"), AT);
    assert_send(&processing);
    let result = processing.await;

    assert_eq!(
        result,
        Activation {
            disposition: FunnelDisposition::CaptureOnly,
            outcome: completed(false, CompletionKind::DryRun),
        }
    );
    assert!(calls(&mock).is_empty(), "dry-run must not reach Discord");
    assert_eq!(facts.target_calls(), 0, "dry-run needs no target fetch");
    assert_eq!(ledger.violations(), 0, "dry-run never counts");
    mock.shutdown().await;
}

#[tokio::test]
async fn protected_target_is_counted_but_untouched() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let (ledger, facts) = (MemLedger::default(), StaticFacts::default());
    facts.set(Some(target_facts(true, 1)));
    let activation = AutomodActivation::new(runtime(true), ledger.clone(), facts, executor(&mock));

    let result = activation.process(create("1002", "blocked"), AT).await;

    assert_eq!(result.disposition, FunnelDisposition::CaptureOnly);
    assert_eq!(result.outcome, completed(false, CompletionKind::Protected));
    assert!(
        calls(&mock).is_empty(),
        "protected target must stay untouched"
    );
    assert_eq!(ledger.violations(), 1, "legacy counts protected matches");
    mock.shutdown().await;
}

#[tokio::test]
async fn ladder_deletes_then_warns_then_times_out() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let ledger = MemLedger::default();
    let activation = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );

    let first = activation.process(create("2001", "blocked"), AT).await;
    let second = activation.process(create("2002", "blocked"), AT).await;
    let third = activation.process(create("2003", "blocked"), AT).await;

    assert_eq!(first.outcome, completed(true, CompletionKind::Deleted));
    assert_eq!(second.outcome, completed(true, CompletionKind::Warned));
    assert_eq!(third.outcome, completed(true, CompletionKind::TimedOut));
    for result in [&first, &second, &third] {
        assert_eq!(result.disposition, FunnelDisposition::CaptureOnly);
    }
    let member = format!("/api/v10/guilds/{STAGING_GUILD_ID}/members/{AUTHOR}");
    assert_eq!(
        calls(&mock),
        vec![
            ("DELETE".to_owned(), delete_path("2001")),
            ("DELETE".to_owned(), delete_path("2002")),
            ("DELETE".to_owned(), delete_path("2003")),
            ("PATCH".to_owned(), member),
        ],
        "warn is the ledger row only; timeout follows the exact delete"
    );
    let timeout = &mock.requests()[3];
    let body: serde_json::Value = serde_json::from_slice(&timeout.body).unwrap();
    assert!(body["communication_disabled_until"].is_string());
    assert_eq!(ledger.violations(), 3);
    mock.shutdown().await;
}

#[tokio::test]
async fn duplicate_delivery_is_processed_once() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let ledger = MemLedger::default();
    let activation = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );
    let awards = Awards::default();
    let pipeline = Pipeline::new(
        MemStore::new(),
        Some(awards.clone()),
        Some(awards.clone()),
        NoInvites,
        NoClassification,
    );

    // A clean create awards once; its redelivery is cache-only.
    let clean = create_event("3001", "clean");
    for _ in 0..2 {
        let delivery = event_to_automod(&clean, 1_790_726_400_000).unwrap();
        let result = activation.process(delivery, AT).await;
        pipeline.handle_with_message_disposition(&clean, result.disposition);
    }
    assert_eq!(
        awards.0.load(Ordering::SeqCst),
        2,
        "one fact + one XP award"
    );

    // A matched create deletes once; its redelivery sends nothing.
    let first = activation.process(create("3002", "blocked"), AT).await;
    let again = activation.process(create("3002", "blocked"), AT).await;
    assert_eq!(first.outcome, completed(true, CompletionKind::Deleted));
    assert_eq!(
        again,
        Activation {
            disposition: FunnelDisposition::None,
            outcome: ActivationOutcome::Duplicate(Some(StoredOutcome {
                matched: true,
                deleted: true,
                outcome: CompletionKind::Deleted,
            })),
        }
    );
    assert_eq!(
        calls(&mock),
        vec![("DELETE".to_owned(), delete_path("3002"))]
    );
    assert_eq!(ledger.violations(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn crash_before_checkpoint_restores_the_funnel_once_without_resending() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let ledger = MemLedger::default();
    let clean = create_event("5001", "clean");
    let blocked = create_event("5002", "blocked");
    let deliver = |event: &Event| event_to_automod(event, 1_790_726_400_000).unwrap();

    // First run settles both claims, then dies before its checkpoint commits:
    // none of its funnel writes survive.
    let first = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );
    for event in [&clean, &blocked] {
        first.process(deliver(event), AT).await;
    }

    let restarted = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );
    let awards = Awards::default();
    let pipeline = Pipeline::new(
        MemStore::new(),
        Some(awards.clone()),
        Some(awards.clone()),
        NoInvites,
        NoClassification,
    );

    let result = restarted.process(deliver(&clean), AT).await;
    assert_eq!(result.disposition, FunnelDisposition::None);
    let disposition = result.uncommitted_disposition(MessageDeliveryKind::Create);
    assert_eq!(disposition, FunnelDisposition::Accept);
    pipeline.handle_with_message_disposition(&clean, disposition);
    assert_eq!(
        awards.0.load(Ordering::SeqCst),
        2,
        "one fact + one XP award"
    );

    let result = restarted.process(deliver(&blocked), AT).await;
    let disposition = result.uncommitted_disposition(MessageDeliveryKind::Create);
    assert_eq!(disposition, FunnelDisposition::CaptureOnly);
    pipeline.handle_with_message_disposition(&blocked, disposition);
    assert_eq!(
        awards.0.load(Ordering::SeqCst),
        3,
        "the matched message adds its fact but no XP"
    );
    assert_eq!(
        calls(&mock),
        vec![("DELETE".to_owned(), delete_path("5002"))],
        "the crashed run's delete is never resent"
    );
    // An update never awards, whatever the claim says.
    assert_eq!(
        result.uncommitted_disposition(MessageDeliveryKind::Update),
        FunnelDisposition::None
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn mode_change_after_crash_funnels_the_message_once() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let ledger = MemLedger::default();
    let clean = create_event("5101", "clean");
    let delivery = || event_to_automod(&clean, 1_790_726_400_000).unwrap();

    // Dry-run settles the claim and crashes before the checkpoint commits.
    let preview = AutomodActivation::new(
        runtime(false),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );
    preview.process(delivery(), AT).await;

    // The enforce restart owns a different claim for the same message.
    let enforce = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );
    let awards = Awards::default();
    let pipeline = Pipeline::new(
        MemStore::new(),
        Some(awards.clone()),
        Some(awards.clone()),
        NoInvites,
        NoClassification,
    );
    let result = enforce.process(delivery(), AT).await;
    let disposition = result.uncommitted_disposition(MessageDeliveryKind::Create);
    assert_eq!(disposition, FunnelDisposition::Accept);
    pipeline.handle_with_message_disposition(&clean, disposition);

    assert_eq!(ledger.claims(), 2);
    assert_eq!(
        awards.0.load(Ordering::SeqCst),
        2,
        "one fact + one XP award"
    );
    assert!(calls(&mock).is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn uncertain_delete_retains_claim_and_sends_no_timeout() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let ledger = MemLedger::default();
    ledger.seed_violations(2);
    let activation = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );

    let result = activation.process(create("4001", "blocked"), AT).await;
    let retry = activation.process(create("4001", "blocked"), AT).await;

    assert_eq!(
        result.outcome,
        ActivationOutcome::Retained(RetainReason::UncertainDelete)
    );
    assert_eq!(
        retry.outcome,
        ActivationOutcome::Duplicate(None),
        "no auto-resend"
    );
    assert_eq!(
        calls(&mock),
        vec![("DELETE".to_owned(), delete_path("4001"))]
    );
    assert_eq!(ledger.claims(), 1, "started claim kept for reconciliation");
    mock.shutdown().await;
}

#[tokio::test]
async fn uncertain_timeout_is_retained_after_confirmed_delete() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let ledger = MemLedger::default();
    ledger.seed_violations(2);
    let activation = AutomodActivation::new(
        runtime(true),
        ledger,
        StaticFacts::allowed(),
        executor(&mock),
    );

    let result = activation.process(create("4002", "blocked"), AT).await;

    assert_eq!(
        result.outcome,
        ActivationOutcome::Retained(RetainReason::UncertainTimeout)
    );
    assert_eq!(calls(&mock).len(), 2, "one delete, one timeout, no retry");
    mock.shutdown().await;
}

#[tokio::test]
async fn rejected_delete_settles_without_followup() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let ledger = MemLedger::default();
    ledger.seed_violations(2);
    let activation = AutomodActivation::new(
        runtime(true),
        ledger,
        StaticFacts::allowed(),
        executor(&mock),
    );

    let result = activation.process(create("4003", "blocked"), AT).await;

    assert_eq!(
        result.outcome,
        completed(false, CompletionKind::SanctionRefused)
    );
    assert_eq!(
        calls(&mock),
        vec![("DELETE".to_owned(), delete_path("4003"))]
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn refused_fence_sends_nothing() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let ledger = MemLedger::default();
    ledger.0.refuse_fence.store(true, Ordering::SeqCst);
    let activation = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );

    let result = activation.process(create("4004", "blocked"), AT).await;

    assert_eq!(
        result.outcome,
        ActivationOutcome::Retained(RetainReason::FenceRefused)
    );
    assert!(calls(&mock).is_empty(), "no mutation without the fence");
    assert_eq!(ledger.violations(), 1, "counted claim stays as evidence");
    mock.shutdown().await;
}

#[tokio::test]
async fn unavailable_target_releases_then_preserved_retry_enforces() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let (ledger, facts) = (MemLedger::default(), StaticFacts::default());
    let activation = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        facts.clone(),
        executor(&mock),
    );

    let first = activation.process(create("5001", "blocked"), AT).await;
    assert_eq!(
        first,
        Activation {
            disposition: FunnelDisposition::CaptureOnly,
            outcome: ActivationOutcome::Unavailable,
        }
    );
    assert_eq!(ledger.violations(), 0, "unavailable facts count nothing");
    assert!(calls(&mock).is_empty());

    // The released claim replays its preserved decision once facts resolve.
    facts.set(Some(target_facts(false, 1)));
    let retry = activation.process(create("5001", "blocked"), AT).await;
    assert_eq!(retry.outcome, completed(true, CompletionKind::Deleted));
    assert_eq!(
        calls(&mock),
        vec![("DELETE".to_owned(), delete_path("5001"))]
    );
    assert_eq!(ledger.violations(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn outside_scope_bypasses_without_claim() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let ledger = MemLedger::default();
    let activation = AutomodActivation::new(
        runtime(true),
        ledger.clone(),
        StaticFacts::allowed(),
        executor(&mock),
    );
    let mut delivery = create("6001", "blocked");
    delivery.guild_id = Some("326474832151838730".into());

    let result = activation.process(delivery, AT).await;

    assert_eq!(
        result,
        Activation {
            disposition: FunnelDisposition::Accept,
            outcome: ActivationOutcome::Bypassed,
        }
    );
    assert_eq!(ledger.claims(), 0);
    assert!(calls(&mock).is_empty());
    mock.shutdown().await;
}

fn guild_json() -> serde_json::Value {
    json!({
        "id": STAGING_GUILD_ID, "owner_id": OWNER,
        "roles": [
            {"id": STAGING_GUILD_ID, "position": 0, "permissions": "0"},
            {"id": AUTHOR_ROLE, "position": 1, "permissions": "0"},
            {"id": BOT_ROLE, "position": 5, "permissions": (PERM_MODERATE_MEMBERS | (1 << 13)).to_string()}
        ]
    })
}

fn member_json(user: &str, role: &str) -> serde_json::Value {
    json!({"user": {"id": user, "username": "mock"}, "roles": [role]})
}

#[tokio::test]
async fn partial_edit_fetches_reinspects_and_never_awards() {
    let mut edited = message_json("7001", "now blocked");
    edited["edited_timestamp"] = json!("2026-10-02T00:01:00.000000+00:00");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, edited),
            ScriptedResponse::json(200, member_json(AUTHOR, AUTHOR_ROLE)),
            ScriptedResponse::json(200, guild_json()),
            ScriptedResponse::json(200, json!({"id": BOT, "username": "bot", "bot": true})),
            ScriptedResponse::json(200, member_json(BOT, BOT_ROLE)),
            ScriptedResponse::json(200, member_json(AUTHOR, AUTHOR_ROLE)),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let facts = RestAutomodFacts::new(rest.clone(), OWNER.into(), HashSet::new());
    let activation = AutomodActivation::new(runtime(true), MemLedger::default(), facts, rest);
    let edit = PartialEdit::from_dispatch(&json!({
        "id": "7001", "channel_id": CHANNEL, "guild_id": STAGING_GUILD_ID,
        "edited_timestamp": "2026-10-02T00:01:00.000000+00:00"
    }))
    .unwrap();

    let result = activation
        .process(partial_edit_delivery(&edit, 1_790_726_460_000), AT)
        .await;

    assert_eq!(
        result,
        Activation {
            disposition: FunnelDisposition::None,
            outcome: completed(true, CompletionKind::Deleted),
        },
        "an edit never awards and is enforced from the fetched revision"
    );
    let guild = format!("/api/v10/guilds/{STAGING_GUILD_ID}");
    assert_eq!(
        calls(&mock),
        vec![
            (
                "GET".to_owned(),
                format!("/api/v10/channels/{CHANNEL}/messages/7001")
            ),
            ("GET".to_owned(), format!("{guild}/members/{AUTHOR}")),
            ("GET".to_owned(), guild.clone()),
            ("GET".to_owned(), "/api/v10/users/@me".to_owned()),
            ("GET".to_owned(), format!("{guild}/members/{BOT}")),
            ("GET".to_owned(), format!("{guild}/members/{AUTHOR}")),
            ("DELETE".to_owned(), delete_path("7001")),
        ]
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn failed_edit_fetch_is_unavailable_without_claim() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(404)],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let ledger = MemLedger::default();
    let facts = RestAutomodFacts::new(rest.clone(), OWNER.into(), HashSet::new());
    let activation = AutomodActivation::new(runtime(true), ledger.clone(), facts, rest);
    let edit = PartialEdit::from_dispatch(&json!({
        "id": "7002", "channel_id": CHANNEL, "guild_id": STAGING_GUILD_ID
    }))
    .unwrap();

    let result = activation
        .process(partial_edit_delivery(&edit, 1_790_726_460_000), AT)
        .await;

    assert_eq!(
        result,
        Activation {
            disposition: FunnelDisposition::None,
            outcome: ActivationOutcome::Unavailable,
        }
    );
    assert_eq!(
        ledger.claims(),
        0,
        "no claim without an authoritative snapshot"
    );
    assert_eq!(
        calls(&mock).len(),
        1,
        "a missing message is not empty content"
    );
    mock.shutdown().await;
}
