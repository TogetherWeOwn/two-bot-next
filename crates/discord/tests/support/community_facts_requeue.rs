//! Protected acceptance for DeferredCommunityFacts::drain, not a replacement drain.
//! Run with ../run_community_facts_requeue.py. The scratch runner changes ONLY
//! the record_fact import to this store double and registers this child module.
//! Builder must retain that cfg(test) binding and registration in the real module;
//! non-test builds must still bind two_bot_core::community_store::record_fact.
//! No database, network connection, detached task or process-global store state.
//!
//! Breaks caught: dropping taken suffixes on error/cancellation/no pool; replaying
//! acknowledged writes (including Ok(false)); overwriting concurrent arrivals;
//! unbounded recovery or unaccounted eviction; healing/draining poisoned state.
//! Recovery mirrors game-activity: prepend unwritten work, keep newest on overflow.
//! The per-family bound is 8,192; counters are cumulative (pending, rules, joins).
#![cfg(test)]

use super::*;
use std::{collections::BTreeSet, time::Duration};
use two_bot_core::Classification;

const CAP: usize = 8_192;
const AT: &str = "2026-10-26T12:00:00.000Z";

type Counts = (u64, u64, u64);
type Batch = (
    Vec<FactWrite>,
    Vec<RulesAcceptedWrite>,
    Vec<MemberJoinWrite>,
);

// Baseline has no observable eviction counters. Zero is its truthful baseline,
// not an implementation of requeue. An inherent Builder method wins resolution.
// The qualified control below keeps this adapter live after that method exists.
trait BaselineDropCounts {
    fn dropped_counts(&self) -> Result<Counts, CommunityFactsError>;
}
impl BaselineDropCounts for DeferredCommunityFacts {
    fn dropped_counts(&self) -> Result<Counts, CommunityFactsError> {
        self.check_healthy()?;
        Ok((0, 0, 0))
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    ErrorAt(usize),
    BlockAt(usize),
}

struct StoreState {
    fault: Fault,
    attempts: Vec<FactWrite>,
    acknowledged: Vec<FactWrite>,
    duplicate_keys: BTreeSet<String>,
    duplicate_event_ids: BTreeSet<String>,
    arrival: Option<Box<dyn FnOnce() + Send>>,
}

impl StoreState {
    fn new(fault: Fault) -> Self {
        Self {
            fault,
            attempts: Vec::new(),
            acknowledged: Vec::new(),
            duplicate_keys: BTreeSet::new(),
            duplicate_event_ids: BTreeSet::new(),
            arrival: None,
        }
    }
}

tokio::task_local! {
    static STORE: Arc<Mutex<StoreState>>;
}

// External persistence boundary only. The queue, transforms, loops, successful
// progress, return values and cancellation behavior all belong to the REAL drain.
// Other existing library tests retain the real store outside our scoped double.
// Our cases never connect: a scoped store is active for every drain below.
pub(super) async fn record_fact(
    pool: &PgPool,
    write: &FactWrite,
) -> Result<bool, CommunityStoreError> {
    let Ok(state) = STORE.try_with(Arc::clone) else {
        return two_bot_core::community_store::record_fact(pool, write).await;
    };
    let (fault, arrival, duplicate) = {
        let mut state = state.lock().expect("store fixture lock");
        let index = state.attempts.len();
        state.attempts.push(write.clone());
        let fault = match state.fault {
            Fault::ErrorAt(at) if at == index => Fault::ErrorAt(at),
            Fault::BlockAt(at) if at == index => Fault::BlockAt(at),
            _ => Fault::None,
        };
        let arrival = state.arrival.take();
        let duplicate = state.duplicate_keys.contains(&write.idempotency_key)
            || state.duplicate_event_ids.contains(&write.source_event_id);
        if matches!(fault, Fault::None) {
            state.acknowledged.push(write.clone());
        }
        (fault, arrival, duplicate)
    };
    if let Some(arrival) = arrival {
        arrival();
    }
    match fault {
        Fault::ErrorAt(_) => Err(CommunityStoreError::Db(sqlx::Error::PoolClosed)),
        Fault::BlockAt(_) => std::future::pending().await,
        Fault::None => Ok(!duplicate),
    }
}

fn classification() -> Classification {
    Classification {
        classifier_version: "acceptance-v1".into(),
        classification: "eligible_human".into(),
        matched_rule: "fixture".into(),
    }
}

fn batch(prefix: &str, sizes: (usize, usize, usize)) -> Batch {
    let pending = (0..sizes.0)
        .map(|index| FactWrite {
            guild_id: "100".into(),
            event_type: match index % 3 {
                0 => "message_created",
                1 => "voice_session_started",
                _ => "voice_session_ended",
            }
            .into(),
            source_event_id: format!("{prefix}:pending:{index}"),
            actor_id: Some("200".into()),
            occurred_at: AT.into(),
            source: "gateway".into(),
            classification: classification(),
            metadata: Some(format!("{{\"fixtureIndex\":{index}}}")),
            idempotency_key: format!("{prefix}:pending-key:{index}"),
        })
        .collect();
    let rules = (0..sizes.1)
        .map(|index| RulesAcceptedWrite {
            guild_id: 100,
            member_id: 1_000 + index as u64,
            is_bot: index % 2 == 1,
            occurred_at: AT.into(),
            source_event_id: format!("{prefix}:rules:{index}"),
            source: "gateway".into(),
        })
        .collect();
    let joins = (0..sizes.2)
        .map(|index| MemberJoinWrite {
            guild_id: 100,
            member_id: 2_000 + index as u64,
            is_bot: index % 2 == 1,
            occurred_at: AT.into(),
            source_event_id: format!("{prefix}:joins:{index}"),
            source: "invite:fixture".into(),
            inviter_id: Some(300),
        })
        .collect();
    (pending, rules, joins)
}

fn append(buffer: &DeferredCommunityFacts, batch: Batch) {
    let mut state = buffer.0.lock().expect("healthy queue fixture");
    state.pending.extend(batch.0);
    state.rules.extend(batch.1);
    state.joins.extend(batch.2);
}

fn snapshot(buffer: &DeferredCommunityFacts) -> Batch {
    let state = buffer.0.lock().expect("healthy queue snapshot");
    (
        state.pending.clone(),
        state.rules.clone(),
        state.joins.clone(),
    )
}

fn armed(batch: Batch) -> DeferredCommunityFacts {
    let buffer = DeferredCommunityFacts::new();
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
        sqlx::postgres::PgConnectOptions::new_without_pgpass()
            .host("agent-testdb")
            .port(5432)
            .username("agent_test")
            .password("")
            .database("two_bot_test_lazy"),
    );
    buffer
        .enable_with_config(pool, ClassifierConfig::default())
        .expect("arm lazy capture without I/O");
    append(&buffer, batch);
    buffer
}

fn ids(batch: &Batch) -> Vec<String> {
    batch
        .0
        .iter()
        .map(|write| write.source_event_id.clone())
        .chain(batch.1.iter().map(|write| write.source_event_id.clone()))
        .chain(batch.2.iter().map(|write| write.source_event_id.clone()))
        .collect()
}

fn suffix(original: &Batch, acknowledged: usize) -> Batch {
    let pending_done = acknowledged.min(original.0.len());
    let rules_done = acknowledged
        .saturating_sub(original.0.len())
        .min(original.1.len());
    let joins_done = acknowledged
        .saturating_sub(original.0.len() + original.1.len())
        .min(original.2.len());
    (
        original.0[pending_done..].to_vec(),
        original.1[rules_done..].to_vec(),
        original.2[joins_done..].to_vec(),
    )
}

async fn failure_case(at: usize, timeout: bool, duplicate_first: bool) {
    failure_case_with_duplicate(at, timeout, duplicate_first.then_some(0)).await;
}

async fn failure_case_with_duplicate(at: usize, timeout: bool, duplicate_at: Option<usize>) {
    let original = batch("old", (3, 2, 2));
    let expected_ids = ids(&original);
    let buffer = armed(original.clone());
    let fault = if timeout {
        Fault::BlockAt(at)
    } else {
        Fault::ErrorAt(at)
    };
    let store = Arc::new(Mutex::new(StoreState::new(fault)));
    if let Some(index) = duplicate_at {
        let mut state = store.lock().expect("configure duplicate");
        if index < original.0.len() {
            state
                .duplicate_keys
                .insert(original.0[index].idempotency_key.clone());
        } else {
            state
                .duplicate_event_ids
                .insert(expected_ids[index].clone());
        }
    }
    STORE
        .scope(Arc::clone(&store), async {
            if timeout {
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), buffer.drain())
                        .await
                        .is_err(),
                    "the blocked write must reach the caller's timeout arm"
                );
            } else {
                let error = buffer.drain().await.expect_err("injected write failure");
                assert!(matches!(error, CommunityFactsError::Store(_)));
                assert!(!error.is_poison(), "store failure is not poison");
            }
            {
                let store = store.lock().expect("failure receipts");
                assert_eq!(
                    store.attempts.len(),
                    at + 1,
                    "stop on the failed/unacknowledged write"
                );
                let attempts: Vec<_> = store
                    .attempts
                    .iter()
                    .map(|write| write.source_event_id.clone())
                    .collect();
                assert_eq!(
                    attempts,
                    expected_ids[..=at],
                    "attempt only the prefix through the fault"
                );
                let acknowledged: Vec<_> = store
                    .acknowledged
                    .iter()
                    .map(|w| w.source_event_id.clone())
                    .collect();
                assert_eq!(
                    acknowledged,
                    expected_ids[..at],
                    "only successful prefix is acknowledged"
                );
            }
            assert_eq!(
                snapshot(&buffer),
                suffix(&original, at),
                "every unacknowledged payload in every family must remain queued"
            );
            assert_eq!(
                buffer.dropped_counts().expect("healthy counters"),
                (0, 0, 0)
            );
            store.lock().expect("recover store").fault = Fault::None;
            assert_eq!(buffer.drain().await.expect("recovery drain"), 7 - at);
            assert_eq!(buffer.drain().await.expect("empty drain"), 0);
            assert_eq!(snapshot(&buffer), (vec![], vec![], vec![]));
            let store = store.lock().expect("redrain receipts");
            let acknowledged: Vec<_> = store
                .acknowledged
                .iter()
                .map(|w| w.source_event_id.clone())
                .collect();
            assert_eq!(
                acknowledged, expected_ids,
                "never resubmit an acknowledged prefix, even Ok(false)"
            );
            let attempts: Vec<_> = store
                .attempts
                .iter()
                .map(|w| w.source_event_id.clone())
                .collect();
            let mut expected_attempts = expected_ids[..=at].to_vec();
            expected_attempts.extend_from_slice(&expected_ids[at..]);
            assert_eq!(
                attempts, expected_attempts,
                "retry precisely the unwritten suffix"
            );
        })
        .await;
}

macro_rules! failure_test {
    ($name:ident, $at:expr, $timeout:expr, $duplicate:expr) => {
        #[tokio::test(start_paused = true)]
        async fn $name() {
            failure_case($at, $timeout, $duplicate).await;
        }
    };
}
failure_test!(error_on_first_pending_retains_all_families, 0, false, false);
failure_test!(
    error_mid_pending_retains_voice_and_later_families,
    1,
    false,
    false
);
failure_test!(
    error_on_last_pending_retains_later_families,
    2,
    false,
    false
);
failure_test!(error_on_first_rule_retains_rules_and_joins, 3, false, false);
failure_test!(error_mid_rules_retains_joins, 4, false, false);
failure_test!(error_on_first_join_retains_joins, 5, false, false);
failure_test!(error_on_last_join_retains_failed_join, 6, false, false);
failure_test!(
    timeout_before_first_ack_retains_all_families,
    0,
    true,
    false
);
failure_test!(
    timeout_mid_pending_retains_all_unwritten_families,
    1,
    true,
    false
);
failure_test!(
    timeout_mid_rules_retains_all_unwritten_families,
    4,
    true,
    false
);
failure_test!(
    timeout_mid_joins_retains_all_unwritten_families,
    6,
    true,
    false
);
failure_test!(
    duplicate_acknowledgement_is_not_retried_after_error,
    1,
    false,
    true
);
failure_test!(
    duplicate_acknowledgement_is_not_retried_after_timeout,
    1,
    true,
    true
);

#[tokio::test(start_paused = true)]
async fn duplicate_rule_acknowledgement_is_not_retried_after_error() {
    failure_case_with_duplicate(4, false, Some(3)).await;
}

#[tokio::test(start_paused = true)]
async fn duplicate_rule_acknowledgement_is_not_retried_after_timeout() {
    failure_case_with_duplicate(4, true, Some(3)).await;
}

#[tokio::test(start_paused = true)]
async fn duplicate_join_acknowledgement_is_not_retried_after_error() {
    failure_case_with_duplicate(6, false, Some(5)).await;
}

#[tokio::test(start_paused = true)]
async fn duplicate_join_acknowledgement_is_not_retried_after_timeout() {
    failure_case_with_duplicate(6, true, Some(5)).await;
}

#[tokio::test]
async fn no_pool_retains_all_payloads_until_armed() {
    let original = batch("unarmed", (3, 2, 2));
    let buffer = armed(original.clone());
    buffer.0.lock().expect("remove pool before drain").pool = None;
    let store = Arc::new(Mutex::new(StoreState::new(Fault::None)));
    STORE
        .scope(Arc::clone(&store), async {
            for _ in 0..2 {
                assert_eq!(buffer.drain().await.expect("disabled capture"), 0);
                assert_eq!(
                    snapshot(&buffer),
                    original,
                    "no pool is not permission to discard taken facts"
                );
                assert_eq!(
                    buffer.dropped_counts().expect("healthy counters"),
                    (0, 0, 0)
                );
            }
            assert!(store.lock().expect("unarmed receipts").attempts.is_empty());
            let pool = armed((vec![], vec![], vec![]))
                .0
                .lock()
                .expect("donor lazy pool")
                .pool
                .clone()
                .expect("armed donor");
            buffer
                .enable_with_config(pool, ClassifierConfig::default())
                .expect("enable later");
            assert_eq!(buffer.drain().await.expect("drain retained work"), 7);
            let written: Vec<_> = store
                .lock()
                .expect("rearmed receipts")
                .acknowledged
                .iter()
                .map(|w| w.source_event_id.clone())
                .collect();
            assert_eq!(written, ids(&original));
        })
        .await;
}

async fn arrivals_case(sizes: (usize, usize, usize), timeout: bool, expected_drops: Counts) {
    let original = batch("old", (2, 2, 2));
    let arrivals = batch("new", sizes);
    let buffer = armed(original.clone());
    let captured = buffer.clone();
    let to_append = arrivals.clone();
    let mut state = StoreState::new(if timeout {
        Fault::BlockAt(0)
    } else {
        Fault::ErrorAt(0)
    });
    state.arrival = Some(Box::new(move || append(&captured, to_append)));
    let store = Arc::new(Mutex::new(state));
    STORE.scope(Arc::clone(&store), async {
        if timeout {
            assert!(tokio::time::timeout(Duration::from_secs(1), buffer.drain()).await.is_err());
        } else {
            assert!(matches!(buffer.drain().await, Err(CommunityFactsError::Store(_))));
        }
        let mut expected = original;
        expected.0.extend(arrivals.0);
        expected.1.extend(arrivals.1);
        expected.2.extend(arrivals.2);
        let pending_overflow = expected.0.len().saturating_sub(CAP);
        let rules_overflow = expected.1.len().saturating_sub(CAP);
        let joins_overflow = expected.2.len().saturating_sub(CAP);
        expected.0.drain(..pending_overflow);
        expected.1.drain(..rules_overflow);
        expected.2.drain(..joins_overflow);
        assert_eq!(snapshot(&buffer), expected, "prepend failed work without clobbering arrivals; evict oldest past each family's cap");
        assert_eq!(buffer.dropped_counts().expect("observable drops"), expected_drops, "account for every eviction in its own family");
        store.lock().expect("healthy store").fault = Fault::None;
        let count = expected.0.len() + expected.1.len() + expected.2.len();
        assert_eq!(buffer.drain().await.expect("drain surviving facts"), count);
        assert_eq!(buffer.drain().await.expect("nothing duplicated"), 0);
        assert_eq!(buffer.dropped_counts().expect("cumulative drops"), expected_drops);
        let written: Vec<_> = store.lock().expect("survivor receipts").acknowledged.iter().map(|w| w.source_event_id.clone()).collect();
        assert_eq!(written, ids(&expected));
    }).await;
}

#[tokio::test(start_paused = true)]
async fn failed_requeue_preserves_concurrent_arrivals_in_all_families() {
    arrivals_case((1, 1, 1), false, (0, 0, 0)).await;
}
#[tokio::test(start_paused = true)]
async fn cancelled_requeue_preserves_concurrent_arrivals_in_all_families() {
    arrivals_case((1, 1, 1), true, (0, 0, 0)).await;
}
#[tokio::test(start_paused = true)]
async fn failed_requeue_at_each_family_cap_drops_nothing() {
    arrivals_case((CAP - 2, CAP - 2, CAP - 2), false, (0, 0, 0)).await;
}
#[tokio::test(start_paused = true)]
async fn failed_requeue_caps_each_family_and_counts_exact_evictions() {
    arrivals_case((CAP, CAP, CAP), false, (2, 2, 2)).await;
}
#[tokio::test(start_paused = true)]
async fn cancelled_requeue_caps_each_family_and_counts_exact_evictions() {
    arrivals_case((CAP, CAP, CAP), true, (2, 2, 2)).await;
}
#[tokio::test(start_paused = true)]
async fn pending_overflow_never_consumes_rules_or_join_capacity() {
    arrivals_case((CAP, 1, 1), false, (2, 0, 0)).await;
}
#[tokio::test(start_paused = true)]
async fn rules_overflow_never_consumes_pending_or_join_capacity() {
    arrivals_case((1, CAP, 1), false, (0, 2, 0)).await;
}
#[tokio::test(start_paused = true)]
async fn joins_overflow_never_consumes_pending_or_rule_capacity() {
    arrivals_case((1, 1, CAP), false, (0, 0, 2)).await;
}

async fn cumulative_arrivals_case(timeout: bool) {
    let original = batch("old", (2, 2, 2));
    let first_arrivals = batch("first", (CAP, CAP, CAP));
    let second_arrivals = batch("second", (3, 4, 5));
    let buffer = armed(original.clone());
    let mut state = StoreState::new(Fault::None);
    let captured = buffer.clone();
    let first_to_append = first_arrivals.clone();
    state.arrival = Some(Box::new(move || append(&captured, first_to_append)));
    let store = Arc::new(Mutex::new(state));
    STORE.scope(Arc::clone(&store), async {
        let mut second_expected = (
            first_arrivals.0[3..].to_vec(),
            first_arrivals.1[4..].to_vec(),
            first_arrivals.2[5..].to_vec(),
        );
        second_expected.0.extend(second_arrivals.0.clone());
        second_expected.1.extend(second_arrivals.1.clone());
        second_expected.2.extend(second_arrivals.2.clone());
        for (at, expected, drops) in [
            (0, first_arrivals.clone(), (2, 2, 2)),
            (1, second_expected.clone(), (5, 6, 7)),
        ] {
            {
                let mut state = store.lock().expect("arm successive write faults");
                state.fault = if timeout { Fault::BlockAt(at) } else { Fault::ErrorAt(at) };
                if at == 1 {
                    let captured = buffer.clone();
                    let to_append = second_arrivals.clone();
                    state.arrival = Some(Box::new(move || append(&captured, to_append)));
                }
            }
            if timeout {
                assert!(tokio::time::timeout(Duration::from_secs(1), buffer.drain()).await.is_err());
            } else {
                assert!(matches!(buffer.drain().await, Err(CommunityFactsError::Store(_))));
            }
            assert_eq!(snapshot(&buffer), expected, "both positive overflows retain exactly the newest full payloads in each family");
            assert_eq!(buffer.dropped_counts().expect("successive positive evictions"), drops, "add evictions to the SAME buffer's counters; never replace earlier drops");
            let state = store.lock().expect("successive failure receipts");
            assert_eq!(state.attempts.len(), at + 1, "each drain stops at its first unacknowledged write");
            assert!(state.acknowledged.is_empty());
        }
        store.lock().expect("recover after both overflows").fault = Fault::None;
        assert_eq!(buffer.drain().await.expect("healthy drain after both overflows"), 3 * CAP);
        assert_eq!(buffer.dropped_counts().expect("healthy drain preserves cumulative counts"), (5, 6, 7));
        assert_eq!(buffer.drain().await.expect("empty drain after both overflows"), 0);
        assert_eq!(buffer.dropped_counts().expect("empty drain preserves cumulative counts"), (5, 6, 7));
        assert_eq!(snapshot(&buffer), (vec![], vec![], vec![]));
        let state = store.lock().expect("cumulative survivor receipts");
        let written: Vec<_> = state.acknowledged.iter().map(|write| write.source_event_id.clone()).collect();
        assert_eq!(written, ids(&second_expected), "drain every survivor once in family order");
        assert_eq!(state.acknowledged[..CAP], second_expected.0, "pending survivor payloads are unchanged");
        let attempts: Vec<_> = state.attempts.iter().map(|write| write.source_event_id.clone()).collect();
        let mut expected_attempts = vec![original.0[0].source_event_id.clone(), first_arrivals.0[0].source_event_id.clone()];
        expected_attempts.extend(ids(&second_expected));
        assert_eq!(attempts, expected_attempts, "failed writes evicted by overflow never return on retry");
    }).await;
}

#[tokio::test(start_paused = true)]
async fn repeated_failed_requeues_accumulate_positive_evictions_per_family() {
    cumulative_arrivals_case(false).await;
}

#[tokio::test(start_paused = true)]
async fn repeated_cancelled_requeues_accumulate_positive_evictions_per_family() {
    cumulative_arrivals_case(true).await;
}

#[tokio::test]
async fn healthy_control_drains_each_family_once_and_preserves_payloads() {
    let original = batch("healthy", (3, 2, 2));
    let buffer = armed(original.clone());
    let store = Arc::new(Mutex::new(StoreState::new(Fault::None)));
    STORE
        .scope(Arc::clone(&store), async {
            assert_eq!(buffer.drain().await.expect("successful drain"), 7);
            assert_eq!(buffer.drain().await.expect("second drain"), 0);
            assert_eq!(snapshot(&buffer), (vec![], vec![], vec![]));
            assert_eq!(buffer.dropped_counts().expect("no drops"), (0, 0, 0));
            assert_eq!(
                <DeferredCommunityFacts as BaselineDropCounts>::dropped_counts(&buffer)
                    .expect("baseline observation"),
                (0, 0, 0)
            );
            let store = store.lock().expect("successful receipts");
            assert_eq!(store.attempts, store.acknowledged);
            assert_eq!(
                store.attempts[..3],
                original.0,
                "pending payloads pass through unchanged"
            );
            let written: Vec<_> = store
                .attempts
                .iter()
                .map(|w| w.source_event_id.clone())
                .collect();
            assert_eq!(written, ids(&original));
            for write in &store.attempts[3..] {
                assert_eq!(write.guild_id, "100");
                assert_eq!(write.occurred_at, AT);
                assert!(matches!(
                    write.event_type.as_str(),
                    "rules_accepted" | "member_joined"
                ));
                assert!(!write.idempotency_key.is_empty());
            }
            let metadata: serde_json::Value = serde_json::from_str(
                store.attempts[5]
                    .metadata
                    .as_deref()
                    .expect("join metadata"),
            )
            .expect("JSON");
            assert_eq!(metadata["inviterId"], "300");
        })
        .await;
}

#[tokio::test]
async fn healthy_control_duplicate_is_acknowledged_but_not_counted_as_inserted() {
    let original = batch("duplicate", (3, 2, 2));
    let buffer = armed(original.clone());
    let mut state = StoreState::new(Fault::None);
    state
        .duplicate_keys
        .insert(original.0[0].idempotency_key.clone());
    let store = Arc::new(Mutex::new(state));
    STORE
        .scope(Arc::clone(&store), async {
            assert_eq!(buffer.drain().await.expect("duplicate return"), 6);
            assert_eq!(buffer.drain().await.expect("no replay of duplicate"), 0);
            assert_eq!(store.lock().expect("ack receipts").acknowledged.len(), 7);
            assert_eq!(snapshot(&buffer), (vec![], vec![], vec![]));
        })
        .await;
}

#[tokio::test]
async fn healthy_control_poison_refuses_typed_before_taking_any_family() {
    let original = batch("poison", (3, 2, 2));
    let buffer = armed(original.clone());
    let captured = buffer.clone();
    assert!(std::thread::spawn(move || {
        let _guard = captured.0.lock().expect("healthy before poison");
        std::panic::panic_any("synthetic-buffer-poison");
    })
    .join()
    .is_err());
    let store = Arc::new(Mutex::new(StoreState::new(Fault::None)));
    STORE
        .scope(Arc::clone(&store), async {
            for _ in 0..2 {
                assert!(
                    matches!(buffer.dropped_counts(), Err(CommunityFactsError::Poisoned)),
                    "poisoned counters must never look like healthy zero drops"
                );
                assert!(matches!(
                    buffer.drain().await,
                    Err(CommunityFactsError::Poisoned)
                ));
                assert!(
                    matches!(buffer.dropped_counts(), Err(CommunityFactsError::Poisoned)),
                    "neither observation nor drain may clear the poison latch"
                );
                let poisoned = buffer.0.lock().expect_err("poison remains latched");
                let state = poisoned.get_ref();
                assert_eq!(
                    (&state.pending, &state.rules, &state.joins),
                    (&original.0, &original.1, &original.2),
                    "poison must not take even one family"
                );
            }
            assert!(store
                .lock()
                .expect("no store calls on poison")
                .attempts
                .is_empty());
        })
        .await;
}
