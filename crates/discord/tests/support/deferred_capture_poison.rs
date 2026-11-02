//! Protected behavioral acceptance for the actual deferred buffers and owner.
//! Register as a child of leveling_runtime, not a substitute implementation.
//! A holder thread poisons only this fixture; no global hook/latch or sleeps.
#![cfg(test)]

use super::*;
use futures_util::FutureExt;
use serde_json::json;
use std::{fmt::Display, io::Write, panic::AssertUnwindSafe, time::Duration};
use two_bot_core::{ChannelClass, MemStore, MessageShape};

const AT: &str = "2026-10-20T12:00:00.000Z";
const GUILD: u64 = 100;
const MEMBER: u64 = 200;
const CHANNEL: u64 = 300;
const PAYLOAD: &str = "synthetic-poison-payload-not-a-diagnostic";
const LEVEL_REASON: &str = "deferred_leveling_poisoned";
const FACT_REASON: &str = "deferred_community_facts_poisoned";
const ACTION: &str = "supervisor_restart_inspection";

// Accommodate either typed synchronous refusals or infallible hook latching.
// Legacy values normalize to success, NEVER to the expected poison refusal.
trait Observed {
    type Value;
    fn observed(self) -> Result<Self::Value, String>;
}
impl<T, E: Display> Observed for Result<T, E> {
    type Value = T;
    fn observed(self) -> Result<T, String> {
        self.map_err(|error| error.to_string())
    }
}
impl<T> Observed for Vec<T> {
    type Value = Self;
    fn observed(self) -> Result<Self, String> {
        Ok(self)
    }
}
macro_rules! legacy_value {
    ($($kind:ty),+ $(,)?) => {$(
        impl Observed for $kind {
            type Value = Self;
            fn observed(self) -> Result<Self, String> { Ok(self) }
        }
    )+};
}
legacy_value!((), usize, bool);

fn poison<T: Send + 'static>(lock: Arc<Mutex<T>>) {
    let holder = std::thread::spawn(move || {
        let _held = lock.lock().expect("fixture starts with a healthy lock");
        std::panic::panic_any(PAYLOAD);
    });
    assert!(
        holder.join().is_err(),
        "joined holder must inject the fault"
    );
}

fn diagnostic(text: &str, reason: &str) {
    assert!(
        text.contains(reason),
        "missing bounded reason {reason}: {text}"
    );
    assert!(
        text.contains(ACTION),
        "missing actionable supervisor guidance: {text}"
    );
    assert!(text.len() <= 256, "diagnostic must remain bounded");
    for forbidden in [
        PAYLOAD,
        "postgres",
        "sqlx",
        "PoisonError",
        "100:200",
        "fixture-member-content",
    ] {
        assert!(
            !text.contains(forbidden),
            "unsafe diagnostic field: {forbidden}"
        );
    }
}

fn typed_level_refusal(result: Result<Vec<XpAward>, LevelingRuntimeError>, reason: &str) {
    let error = result.expect_err("poison cannot produce award/empty success");
    assert!(
        error.committed_awards().is_empty(),
        "poison never fabricates committed XP"
    );
    assert!(
        !matches!(
            error,
            LevelingRuntimeError::Store(_) | LevelingRuntimeError::Discord(_)
        ),
        "poison must be distinguished before any downstream operation"
    );
    diagnostic(&error.to_string(), reason);
    assert!(!format!("{error:?}").contains(PAYLOAD));
}

fn typed_fact_refusal(result: Result<usize, CommunityFactsError>) {
    let error = result.expect_err("poison is not a successful zero-row capture");
    diagnostic(&error.to_string(), FACT_REASON);
    assert!(!format!("{error:?}").contains(PAYLOAD));
}

fn rules() -> RulesAcceptedFact<'static> {
    RulesAcceptedFact {
        guild_id: GUILD,
        member_id: MEMBER,
        is_bot: false,
        occurred_at: AT,
        source_event_id: "100:200:rules".into(),
        source: "gateway",
    }
}
fn join() -> MemberJoinFact<'static> {
    MemberJoinFact {
        guild_id: GUILD,
        member_id: MEMBER,
        is_bot: false,
        occurred_at: AT,
        source_event_id: "100:200:joined",
        source: "invite:fixture",
        inviter_id: Some(201),
    }
}
fn message_fact_input() -> MessageFact<'static> {
    MessageFact {
        guild_id: GUILD,
        member_id: MEMBER,
        is_bot: false,
        webhook_id: None,
        is_staff_automation: false,
        message_id: "fixture-message",
        channel_id: CHANNEL,
        channel_class: ChannelClass::Human,
        occurred_at: AT,
        content_length: Some(28),
    }
}
fn voice_start() -> VoiceStartedFact<'static> {
    VoiceStartedFact {
        guild_id: GUILD,
        member_id: MEMBER,
        is_bot: false,
        channel_id: CHANNEL,
        occurred_at: AT,
    }
}
fn voice_end() -> VoiceEndedFact<'static> {
    VoiceEndedFact {
        guild_id: GUILD,
        member_id: MEMBER,
        is_bot: false,
        session_key: "100:200:fixture-session".into(),
        channel_id: CHANNEL,
        occurred_at: AT,
        started_at: None,
        duration_seconds: None,
    }
}
fn message_event() -> Event {
    Event::MessageCreate(Box::new(serde_json::from_value(json!({
        "id":"400", "guild_id":"100", "channel_id":"300",
        "author":{"id":"200","username":"fixture","discriminator":"0","avatar":null,"bot":false},
        "content":"fixture-member-content", "timestamp":AT.replace('Z', "+00:00"), "edited_timestamp":null,
        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
        "attachments":[],"embeds":[],"pinned":false,"type":0,"components":[]
    })).expect("complete synthetic message")))
}
fn pipeline() -> OrderedLevelingPipeline<MemStore> {
    OrderedLevelingPipeline::new(MemStore::new(), None)
}

// Real clients are pinned to held, non-serving loopback sockets. A wrong path
// cannot reach any database/Discord, and an attempted connection is observable.
struct Tripwires {
    database: std::net::TcpListener,
    discord: std::net::TcpListener,
    pool: PgPool,
    executor: Arc<ActionExecutor>,
}
impl Tripwires {
    fn new() -> Self {
        let database = std::net::TcpListener::bind("127.0.0.1:0").expect("hold DB tripwire");
        let discord = std::net::TcpListener::bind("127.0.0.1:0").expect("hold REST tripwire");
        database
            .set_nonblocking(true)
            .expect("nonblocking DB observation");
        discord
            .set_nonblocking(true)
            .expect("nonblocking REST observation");
        let options = sqlx::postgres::PgConnectOptions::new()
            .host("127.0.0.1")
            .port(database.local_addr().expect("DB address").port())
            .username("offline_fixture")
            .password("")
            .database("offline_fixture")
            .ssl_mode(sqlx::postgres::PgSslMode::Disable);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .min_connections(0)
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy_with(options);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let origin = format!("http://{}", discord.local_addr().expect("REST address"));
        let executor = Arc::new(
            ActionExecutor::with_proxy("mock-only-token".into(), Some(origin))
                .expect("loopback executor"),
        );
        Self {
            database,
            discord,
            pool,
            executor,
        }
    }
    fn runtime(&self) -> LevelingRuntime {
        LevelingRuntime::new(
            self.pool.clone(),
            Arc::clone(&self.executor),
            GUILD,
            two_bot_core::OnboardingGates {
                mode: two_bot_core::OnboardingMode::Legacy,
                dry_run: false,
            },
        )
    }
    fn untouched(&self) {
        for listener in [&self.database, &self.discord] {
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "poison must refuse before DB or Discord I/O"
            );
        }
        assert_eq!(self.pool.size(), 0);
    }
}

fn preserved_level(buffer: &DeferredLeveling, expected: usize) {
    assert!(
        buffer.0.is_poisoned(),
        "never clear poison or replace the buffer"
    );
    let refusal = buffer
        .0
        .lock()
        .expect_err("untrusted buffer stays poisoned");
    assert_eq!(
        refusal.get_ref().len(),
        expected,
        "never drain untrusted requests"
    );
}
fn preserved_facts(buffer: &DeferredCommunityFacts, expected: (usize, usize, usize)) {
    assert!(
        buffer.0.is_poisoned(),
        "never heal/replace this fixture's lock"
    );
    let refusal = buffer.0.lock().expect_err("untrusted facts stay poisoned");
    let state = refusal.get_ref();
    assert_eq!(
        (state.pending.len(), state.rules.len(), state.joins.len()),
        expected,
        "all fact families must survive the refusal unchanged"
    );
}
fn seeded_level() -> DeferredLeveling {
    let buffer = DeferredLeveling::default();
    buffer.award_message(
        GUILD,
        MEMBER,
        AT,
        CHANNEL,
        MessageShape {
            content_length: Some(28),
            has_embed_or_attachment: false,
        },
    );
    buffer
}
fn seeded_facts() -> DeferredCommunityFacts {
    let buffer = DeferredCommunityFacts::new();
    buffer.record_rules_accepted(rules());
    buffer.record_member_join(join());
    buffer
}

#[tokio::test]
async fn leveling_message_hook_latches_without_panics_and_owner_refuses() {
    let owner = pipeline();
    owner
        .pending
        .award_message(GUILD, MEMBER, AT, CHANNEL, MessageShape::unknown());
    poison(Arc::clone(&owner.pending.0));
    let mut callbacks_ok = true;
    for _ in 0..3 {
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            owner
                .pending
                .award_message(GUILD, MEMBER, AT, CHANNEL, MessageShape::unknown())
        }));
        callbacks_ok &= result
            .as_ref()
            .is_ok_and(|outcome| !outcome.leveled_up && outcome.level == 0);
    }
    let result =
        AssertUnwindSafe(owner.handle_at(&Event::Resumed, AT, MessageEligibility::default()))
            .catch_unwind()
            .await;
    preserved_level(&owner.pending, 1);
    assert!(
        callbacks_ok,
        "infallible enqueue hook must latch, never repeatedly panic or report a level-up"
    );
    typed_level_refusal(result.expect("awaited owner must not panic"), LEVEL_REASON);
}

#[tokio::test]
async fn leveling_voice_hook_latches_without_panics_and_owner_refuses() {
    let owner = pipeline();
    poison(Arc::clone(&owner.pending.0));
    let callback = std::panic::catch_unwind(AssertUnwindSafe(|| {
        owner.pending.award_voice(GUILD, MEMBER, 120, AT, CHANNEL)
    }));
    let result =
        AssertUnwindSafe(owner.handle_at(&Event::Resumed, AT, MessageEligibility::default()))
            .catch_unwind()
            .await;
    preserved_level(&owner.pending, 0);
    let outcome = callback.expect("voice callback must latch instead of panicking");
    assert!(!outcome.leveled_up && outcome.level == 0);
    typed_level_refusal(result.expect("awaited owner must not panic"), LEVEL_REASON);
}

#[tokio::test]
async fn leveling_take_is_typed_failure_not_empty_or_partial_success() {
    let buffer = seeded_level();
    poison(Arc::clone(&buffer.0));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| buffer.take().observed()));
    preserved_level(&buffer, 1);
    diagnostic(
        &result
            .expect("take must not panic")
            .expect_err("no empty/default take on poison"),
        LEVEL_REASON,
    );
}

#[tokio::test]
async fn leveling_owner_empty_event_refuses_even_without_runtime() {
    let owner = pipeline();
    poison(Arc::clone(&owner.pending.0));
    for _ in 0..3 {
        let result = AssertUnwindSafe(owner.handle(&Event::Resumed))
            .catch_unwind()
            .await;
        preserved_level(&owner.pending, 0);
        typed_level_refusal(result.expect("handle must not panic/recur"), LEVEL_REASON);
    }
}

#[tokio::test]
async fn leveling_owner_stops_before_store_or_reward_side_effects() {
    let tripwires = Tripwires::new();
    let owner = OrderedLevelingPipeline::new(MemStore::new(), Some(tripwires.runtime()));
    owner.pending.award_voice(GUILD, MEMBER, 120, AT, CHANNEL);
    poison(Arc::clone(&owner.pending.0));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        AssertUnwindSafe(owner.handle_at(&message_event(), AT, MessageEligibility::default()))
            .catch_unwind(),
    )
    .await;
    tripwires.untouched();
    preserved_level(&owner.pending, 1);
    typed_level_refusal(
        result
            .expect("finite refusal")
            .expect("owner must not panic"),
        LEVEL_REASON,
    );
}

#[tokio::test]
async fn checkpoint_collect_at_exposes_poison_not_a_successful_empty_batch() {
    let owner = pipeline();
    poison(Arc::clone(&owner.pending.0));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        owner
            .collect_at(&Event::Resumed, AT, MessageEligibility::default())
            .observed()
    }));
    preserved_level(&owner.pending, 0);
    diagnostic(
        &result
            .expect("checkpoint collection must not panic")
            .expect_err("checkpoint caller needs a typed refusal"),
        LEVEL_REASON,
    );
}

#[tokio::test]
async fn checkpoint_disposition_collect_exposes_poison_even_when_no_award_is_due() {
    let owner = pipeline();
    poison(Arc::clone(&owner.pending.0));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        owner
            .collect_at_with_message_disposition(&message_event(), AT, FunnelDisposition::None)
            .observed()
    }));
    preserved_level(&owner.pending, 0);
    diagnostic(
        &result
            .expect("disposition collection must not panic")
            .expect_err("None is not a poison bypass"),
        LEVEL_REASON,
    );
}

#[tokio::test]
async fn leveling_poison_after_collection_refuses_before_award_drain() {
    let tripwires = Tripwires::new();
    let owner = OrderedLevelingPipeline::new(MemStore::new(), Some(tripwires.runtime()));
    owner.pending.award_voice(GUILD, MEMBER, 120, AT, CHANNEL);
    let requests = owner
        .collect_at(&Event::Resumed, AT, MessageEligibility::default())
        .observed()
        .expect("healthy collection before the fault");
    assert_eq!(requests.len(), 1);
    poison(Arc::clone(&owner.pending.0));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        AssertUnwindSafe(owner.drain(requests)).catch_unwind(),
    )
    .await;
    preserved_level(&owner.pending, 0);
    tripwires.untouched();
    typed_level_refusal(
        result
            .expect("bounded drain refusal")
            .expect("drain must not panic"),
        LEVEL_REASON,
    );
}

#[tokio::test]
async fn leveling_empty_drain_is_not_false_success_after_poison() {
    let owner = pipeline();
    poison(Arc::clone(&owner.pending.0));
    let result = AssertUnwindSafe(owner.drain(Vec::new()))
        .catch_unwind()
        .await;
    preserved_level(&owner.pending, 0);
    typed_level_refusal(result.expect("empty drain must not panic"), LEVEL_REASON);
}

macro_rules! fact_callback_test {
    ($name:ident, $method:ident, $input:expr) => {
        #[tokio::test]
        async fn $name() {
            let owner = pipeline();
            owner.facts.record_rules_accepted(rules());
            owner.facts.record_member_join(join());
            poison(Arc::clone(&owner.facts.0));
            let mut callback_ok = true;
            for _ in 0..3 {
                callback_ok &=
                    std::panic::catch_unwind(AssertUnwindSafe(|| owner.facts.$method($input)))
                        .is_ok();
            }
            let result = AssertUnwindSafe(owner.drain_facts()).catch_unwind().await;
            preserved_facts(&owner.facts, (0, 1, 1));
            assert!(
                callback_ok,
                "infallible facts callback must latch, not repeatedly panic"
            );
            typed_fact_refusal(result.expect("owning awaited drain must not panic"));
        }
    };
}
fact_callback_test!(
    community_join_callback_latches_into_owner_error,
    record_member_join,
    join()
);
fact_callback_test!(
    community_rules_callback_latches_into_owner_error,
    record_rules_accepted,
    rules()
);
fact_callback_test!(
    community_message_callback_latches_even_while_disabled,
    record_message,
    message_fact_input()
);
fact_callback_test!(
    community_voice_end_callback_latches_even_while_disabled,
    record_voice_ended,
    voice_end()
);

#[tokio::test]
async fn community_voice_start_never_returns_a_fabricated_session_key() {
    let owner = pipeline();
    poison(Arc::clone(&owner.facts.0));
    let callback = std::panic::catch_unwind(AssertUnwindSafe(|| {
        owner.facts.record_voice_started(voice_start())
    }));
    let result = AssertUnwindSafe(owner.drain_facts()).catch_unwind().await;
    preserved_facts(&owner.facts, (0, 0, 0));
    assert_eq!(callback.expect("voice callback must not panic"), None);
    typed_fact_refusal(result.expect("awaited drain must not panic"));
}

macro_rules! fact_read_test {
    ($name:ident, $method:ident) => {
        #[tokio::test]
        async fn $name() {
            let buffer = seeded_facts();
            poison(Arc::clone(&buffer.0));
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| buffer.$method().observed()));
            preserved_facts(&buffer, (0, 1, 1));
            diagnostic(
                &result
                    .expect("poisoned read/take must not panic")
                    .expect_err("untrusted data cannot be returned as empty/success"),
                FACT_REASON,
            );
        }
    };
}
fact_read_test!(community_rules_take_does_not_clear_untrusted_state, take);
fact_read_test!(
    community_joins_take_does_not_clear_untrusted_state,
    take_joins
);
fact_read_test!(community_pending_take_is_not_empty_success, take_pending);
fact_read_test!(community_length_is_not_false_zero_or_success, len);
fact_read_test!(community_empty_is_not_a_successful_health_read, is_empty);
fact_read_test!(
    community_join_length_is_not_false_zero_or_success,
    joins_len
);

#[tokio::test]
async fn community_disabled_drain_refuses_repeatedly_without_clearing() {
    let owner = pipeline();
    owner.facts.record_rules_accepted(rules());
    owner.facts.record_member_join(join());
    poison(Arc::clone(&owner.facts.0));
    for _ in 0..3 {
        let result = AssertUnwindSafe(owner.drain_facts()).catch_unwind().await;
        preserved_facts(&owner.facts, (0, 1, 1));
        typed_fact_refusal(result.expect("poisoned drain must not panic/recur"));
    }
}

#[tokio::test]
async fn community_armed_drain_never_writes_a_partial_batch() {
    let tripwires = Tripwires::new();
    let owner = pipeline();
    owner
        .facts
        .enable_with_config(tripwires.pool.clone(), ClassifierConfig::default())
        .observed()
        .expect("healthy arming");
    owner.facts.record_message(message_fact_input());
    owner.facts.record_voice_started(voice_start());
    owner.facts.record_voice_ended(voice_end());
    owner.facts.record_rules_accepted(rules());
    owner.facts.record_member_join(join());
    poison(Arc::clone(&owner.facts.0));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        AssertUnwindSafe(owner.drain_facts()).catch_unwind(),
    )
    .await;
    tripwires.untouched();
    preserved_facts(&owner.facts, (3, 1, 1));
    typed_fact_refusal(
        result
            .expect("bounded refusal")
            .expect("armed drain must not panic"),
    );
}

macro_rules! fact_config_test {
    ($name:ident, $call:expr) => {
        #[tokio::test]
        async fn $name() {
            use crate::ChannelClassifier as _;

            let previous_io = Tripwires::new();
            let requested_io = Tripwires::new();
            let owner = pipeline();
            // The implicit enable boundaries read this same config. Do not
            // mutate process-global environment to make the fixture distinct.
            let requested = ClassifierConfig::from_env();
            let mut previous = requested.clone();
            previous.version.push_str("-protected-previous");
            let previous_class = if requested.channel_class(CHANNEL) == ChannelClass::Human {
                previous.human_channel_ids.remove(&CHANNEL.to_string());
                previous.welcome_channel_ids.remove(&CHANNEL.to_string());
                ChannelClass::Other
            } else {
                previous.human_channel_ids.insert(CHANNEL.to_string());
                previous.welcome_channel_ids.remove(&CHANNEL.to_string());
                ChannelClass::Human
            };
            assert_ne!(
                previous, requested,
                "refusal must not be a no-op config test"
            );
            assert_ne!(requested.channel_class(CHANNEL), previous_class);
            owner
                .refresh_classifier_config(previous.clone())
                .expect("healthy paired configuration");
            owner
                .facts
                .enable_with_config(previous_io.pool.clone(), previous.clone())
                .expect("healthy original sink");
            assert_eq!(owner.classifier.classify(CHANNEL), previous_class);
            owner.facts.record_rules_accepted(rules());
            poison(Arc::clone(&owner.facts.0));
            let call = std::panic::catch_unwind(AssertUnwindSafe(|| {
                ($call)(&owner, &requested_io, requested.clone())
            }));
            let drained = AssertUnwindSafe(owner.drain_facts()).catch_unwind().await;
            previous_io.untouched();
            requested_io.untouched();
            preserved_facts(&owner.facts, (0, 1, 0));
            let error = call
                .expect("configuration boundary must refuse, not panic")
                .expect_err("poisoned enable/refresh must not return inner Ok(())");
            assert!(matches!(error, CommunityFactsError::Poisoned));
            diagnostic(&error.to_string(), FACT_REASON);
            assert!(!format!("{error:?}").contains(PAYLOAD));
            let held = owner.facts.0.lock().expect_err("state stays poisoned");
            let state = held.get_ref();
            assert_eq!(state.config, previous, "refusal must not refresh the sink");
            assert_eq!(
                state
                    .pool
                    .as_ref()
                    .expect("original sink stays armed")
                    .connect_options()
                    .get_port(),
                previous_io.pool.connect_options().get_port(),
                "refusal must not replace the original sink pool"
            );
            assert_eq!(
                owner.classifier.classify(CHANNEL),
                previous_class,
                "refusal must not refresh the paired channel classifier"
            );
            typed_fact_refusal(
                drained.expect("configuration failure must reach the awaited owner"),
            );
        }
    };
}
fact_config_test!(
    community_enable_poison_reaches_owner,
    |p: &OrderedLevelingPipeline<MemStore>, t: &Tripwires, _: ClassifierConfig| p
        .facts
        .enable(t.pool.clone())
);
fact_config_test!(
    community_explicit_enable_poison_reaches_owner,
    |p: &OrderedLevelingPipeline<MemStore>, t: &Tripwires, requested: ClassifierConfig| p
        .facts
        .enable_with_config(t.pool.clone(), requested)
);
fact_config_test!(
    community_refresh_poison_reaches_owner,
    |p: &OrderedLevelingPipeline<MemStore>, _: &Tripwires, requested: ClassifierConfig| p
        .facts
        .refresh_config(requested)
);
fact_config_test!(
    pipeline_enable_poison_reaches_owner,
    |p: &OrderedLevelingPipeline<MemStore>, t: &Tripwires, _: ClassifierConfig| p
        .enable_community_facts(t.pool.clone())
);
fact_config_test!(
    pipeline_refresh_poison_reaches_owner,
    |p: &OrderedLevelingPipeline<MemStore>, _: &Tripwires, requested: ClassifierConfig| p
        .refresh_classifier_config(requested)
);

#[tokio::test]
async fn community_collect_failure_reaches_leveling_owner_before_awards() {
    let tripwires = Tripwires::new();
    let owner = OrderedLevelingPipeline::new(MemStore::new(), Some(tripwires.runtime()));
    owner.pending.award_voice(GUILD, MEMBER, 120, AT, CHANNEL);
    poison(Arc::clone(&owner.facts.0));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        AssertUnwindSafe(owner.handle_at(&message_event(), AT, MessageEligibility::default()))
            .catch_unwind(),
    )
    .await;
    tripwires.untouched();
    preserved_facts(&owner.facts, (0, 0, 0));
    typed_level_refusal(
        result
            .expect("finite owner refusal")
            .expect("community callback must not panic"),
        FACT_REASON,
    );
}

#[tokio::test]
async fn community_capture_only_collect_exposes_refusal_to_checkpoint_caller() {
    let owner = pipeline();
    poison(Arc::clone(&owner.facts.0));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        owner
            .collect_at_with_message_disposition(
                &message_event(),
                AT,
                FunnelDisposition::CaptureOnly,
            )
            .observed()
    }));
    preserved_facts(&owner.facts, (0, 0, 0));
    diagnostic(
        &result
            .expect("capture-only callback cannot panic")
            .expect_err("capture-only must surface failure"),
        FACT_REASON,
    );
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("test log writer")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn repeated_fault_diagnostics_for_both_buffers_do_not_flood_or_leak() {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    let _local = tracing::dispatcher::set_default(&dispatch);
    let owner = pipeline();
    poison(Arc::clone(&owner.facts.0));
    poison(Arc::clone(&owner.pending.0));
    let mut errors = Vec::new();
    let mut level_errors = Vec::new();
    let mut callbacks_ok = true;
    for _ in 0..3 {
        callbacks_ok &= std::panic::catch_unwind(AssertUnwindSafe(|| {
            owner.facts.record_rules_accepted(rules())
        }))
        .is_ok();
        callbacks_ok &= std::panic::catch_unwind(AssertUnwindSafe(|| {
            owner.pending.award_voice(GUILD, MEMBER, 120, AT, CHANNEL)
        }))
        .is_ok();
        errors.push(AssertUnwindSafe(owner.drain_facts()).catch_unwind().await);
        level_errors.push(
            AssertUnwindSafe(owner.drain(Vec::new()))
                .catch_unwind()
                .await,
        );
    }
    let text = String::from_utf8(logs.0.lock().expect("test logs").clone()).expect("UTF-8 logs");
    assert!(!text.contains(PAYLOAD));
    assert!(
        text.matches(FACT_REASON).count() <= 1,
        "one failed buffer must not flood diagnostic logs"
    );
    assert!(
        text.matches(LEVEL_REASON).count() <= 1,
        "one failed leveling buffer must not flood logs"
    );
    assert!(callbacks_ok, "repeated callback faults must latch");
    for error in errors {
        typed_fact_refusal(error.expect("no follow-on panic"));
    }
    for error in level_errors {
        typed_level_refusal(error.expect("no follow-on leveling panic"), LEVEL_REASON);
    }
}

#[tokio::test]
async fn healthy_leveling_hooks_preserve_order_duplicates_metadata_and_no_threshold_claim() {
    let buffer = DeferredLeveling::default();
    let message = buffer.award_message(
        GUILD,
        MEMBER,
        AT,
        CHANNEL,
        MessageShape {
            content_length: Some(28),
            has_embed_or_attachment: true,
        },
    );
    let duplicate = buffer.award_message(
        GUILD,
        MEMBER,
        AT,
        CHANNEL,
        MessageShape {
            content_length: Some(28),
            has_embed_or_attachment: true,
        },
    );
    let voice = buffer.award_voice(GUILD, MEMBER + 1, 120, AT, CHANNEL + 1);
    for outcome in [message, duplicate, voice] {
        assert!(!outcome.leveled_up && outcome.level == 0);
    }
    let requests = buffer.take().observed().expect("healthy take");
    assert_eq!(
        requests.len(),
        3,
        "do not silently drop/dedupe before the store"
    );
    assert_eq!(
        requests
            .iter()
            .map(|r| (
                r.guild_id,
                r.member_id,
                r.channel_id,
                r.duration,
                r.content_length,
                r.has_embed_or_attachment
            ))
            .collect::<Vec<_>>(),
        vec![
            (100, 200, 300, None, Some(28), true),
            (100, 200, 300, None, Some(28), true),
            (100, 201, 301, Some(120), None, false)
        ]
    );
    assert!(requests.iter().all(|r| r.at == AT));
    assert!(buffer
        .take()
        .observed()
        .expect("healthy empty take")
        .is_empty());
}

#[tokio::test]
async fn healthy_disabled_rules_joins_and_empty_drain_keep_existing_behavior() {
    let owner = pipeline();
    owner.facts.record_rules_accepted(rules());
    owner.facts.record_rules_accepted(rules());
    owner.facts.record_member_join(join());
    owner.facts.record_message(message_fact_input());
    assert_eq!(owner.facts.record_voice_started(voice_start()), None);
    owner.facts.record_voice_ended(voice_end());
    assert_eq!(owner.facts.len().observed().expect("healthy count"), 2);
    assert_eq!(
        owner
            .facts
            .joins_len()
            .observed()
            .expect("healthy join count"),
        1
    );
    assert!(!owner
        .facts
        .is_empty()
        .observed()
        .expect("healthy health read"));
    assert!(owner
        .facts
        .take_pending()
        .observed()
        .expect("healthy disabled messages")
        .is_empty());
    assert_eq!(
        owner
            .drain_facts()
            .await
            .expect("disabled is legitimately zero"),
        0
    );
    assert!(owner
        .facts
        .is_empty()
        .observed()
        .expect("healthy emptied buffer"));
    assert_eq!(
        owner
            .facts
            .joins_len()
            .observed()
            .expect("healthy emptied joins"),
        0
    );
}

#[tokio::test]
async fn healthy_armed_fact_families_keep_classification_order_keys_and_take_semantics() {
    let tripwires = Tripwires::new();
    let buffer = DeferredCommunityFacts::new();
    let config = ClassifierConfig::from_map(
        &[(
            "TWO_COMMUNITY_CLASSIFIER_VERSION".into(),
            "fixture-v7".into(),
        )]
        .into_iter()
        .collect(),
    );
    buffer
        .enable_with_config(tripwires.pool.clone(), config)
        .observed()
        .expect("healthy configuration");
    buffer.record_message(message_fact_input());
    let key = buffer
        .record_voice_started(voice_start())
        .expect("armed start gets real deferred key");
    buffer.record_voice_ended(VoiceEndedFact {
        session_key: key.clone(),
        started_at: Some(AT),
        duration_seconds: Some(120),
        ..voice_end()
    });
    buffer.record_rules_accepted(rules());
    buffer.record_member_join(join());
    let pending = buffer
        .take_pending()
        .observed()
        .expect("healthy pending take");
    assert_eq!(pending.len(), 3);
    assert_eq!(
        pending
            .iter()
            .map(|write| write.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "message_created",
            "voice_session_started",
            "voice_session_ended"
        ]
    );
    assert!(pending.iter().all(
        |write| write.classification.classification == "eligible_human"
            && write.classification.classifier_version == "fixture-v7"
    ));
    assert_eq!(pending[0].source_event_id, "fixture-message");
    let message_metadata: serde_json::Value =
        serde_json::from_str(pending[0].metadata.as_deref().expect("message metadata"))
            .expect("JSON metadata");
    assert_eq!(message_metadata["contentLength"], 28);
    assert!(!pending[0]
        .metadata
        .as_deref()
        .expect("message metadata")
        .contains("fixture-member-content"));
    assert_eq!(pending[1].source_event_id, key);
    assert_eq!(pending[2].source_event_id, key);
    let end_metadata: serde_json::Value =
        serde_json::from_str(pending[2].metadata.as_deref().expect("end metadata"))
            .expect("JSON metadata");
    assert_eq!(end_metadata["durationSeconds"], 120);
    assert_eq!(end_metadata["startKnown"], true);
    assert_eq!(
        buffer.take().observed().expect("healthy rules")[0].source_event_id,
        "100:200:rules"
    );
    let joins = buffer.take_joins().observed().expect("healthy joins");
    assert_eq!(
        (joins[0].inviter_id, joins[0].source.as_str()),
        (Some(201), "invite:fixture")
    );
    assert!(buffer
        .take_pending()
        .observed()
        .expect("healthy empty take")
        .is_empty());
    assert_eq!(buffer.drain().await.expect("already taken"), 0);
    tripwires.untouched();
}

#[tokio::test]
async fn healthy_gateway_message_runs_through_real_owner() {
    let owner = pipeline();
    owner.set_message_content_delivered(true);
    assert!(owner
        .handle_at(&message_event(), AT, MessageEligibility::default())
        .await
        .expect("healthy gateway message")
        .is_empty());
    assert_eq!(
        owner.drain_facts().await.expect("healthy disabled capture"),
        0
    );
    assert!(owner
        .pending
        .take()
        .observed()
        .expect("owner already drained")
        .is_empty());
}

#[tokio::test]
async fn healthy_instances_continue_after_an_unrelated_instance_is_poisoned() {
    let damaged = pipeline();
    poison(Arc::clone(&damaged.pending.0));
    poison(Arc::clone(&damaged.facts.0));
    // Drive detection, not just poisoning: a broken global latch would only
    // activate when one of these real hooks/drains first observes the fault.
    // The refusal assertions live in the RED cases; this is the independent
    // healthy control and must stay green even on the unfixed runtime.
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        damaged.pending.award_voice(GUILD, MEMBER, 120, AT, CHANNEL)
    }));
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        damaged.facts.record_member_join(join())
    }));
    let _ = AssertUnwindSafe(damaged.drain(Vec::new()))
        .catch_unwind()
        .await;
    let _ = AssertUnwindSafe(damaged.drain_facts()).catch_unwind().await;
    let healthy = pipeline();
    healthy.facts.record_rules_accepted(rules());
    healthy.facts.record_member_join(join());
    assert_eq!(
        healthy
            .facts
            .take()
            .observed()
            .expect("unrelated rules")
            .len(),
        1
    );
    assert_eq!(
        healthy
            .facts
            .take_joins()
            .observed()
            .expect("unrelated joins")
            .len(),
        1
    );
    assert!(healthy
        .handle_at(&Event::Resumed, AT, MessageEligibility::default())
        .await
        .expect("unrelated pipeline")
        .is_empty());
    assert_eq!(healthy.drain_facts().await.expect("unrelated facts"), 0);
    assert!(!healthy.pending.0.is_poisoned() && !healthy.facts.0.is_poisoned());
}
