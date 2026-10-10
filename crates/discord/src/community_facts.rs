//! Deferred community-facts capture for the gateway pipeline (M3 slices).
//!
//! The funnel handlers call [`FactsSink`] synchronously, but Postgres writes
//! are async. Like `DeferredLeveling`, this module only buffers the fact
//! inputs on the funnel half; the serial gateway worker drains them
//! afterwards through `CommunityFactsRuntime` (feature `db`), which
//! classifies via `classify` and writes via
//! `community_store::rules_accepted_fact` + `record_fact`.
//!
//! This slice wires `rules_accepted` only: gate-clearings from
//! `FunnelHandlers::on_gate_cleared` (fed by `MemberAdd` with
//! `pending:false` and `MemberUpdate` pending-to-cleared transitions).
//! Sibling slices add their streams behind the same buffer; until then the
//! other [`FactsSink`] arms stay no-ops.
//!
//! Timestamp/source contract (mirrors `GateClearedInput`): the live path
//! carries the gateway receipt time (`MemberUpdate`) or Discord's `joined_at`
//! (`MemberAdd`) with source `gateway` — never a guess. A backfill must pass
//! an explicit time with a `backfill:*` source; the drain preserves whatever
//! the handler received and never invents either field.

use std::sync::{Arc, Mutex};

#[cfg(feature = "db")]
use two_bot_core::{classify, ClassifierConfig, ClassifyInput, ScorecardGates};
use two_bot_core::{
    FactsSink, MemberJoinFact, MessageFact, RulesAcceptedFact, Snowflake, VoiceEndedFact,
    VoiceStartedFact,
};

/// One buffered gate-clearing: owned inputs for a later async
/// `community_store::rules_accepted_fact` write. Bots are captured like any
/// other member (classified `bot`, never funnel-counted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesAcceptedWrite {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub occurred_at: String,
    pub source_event_id: String,
    pub source: String,
}

/// Synchronous [`FactsSink`] over a shared buffer. The pipeline holds one
/// behind `Arc`; the gateway worker takes and drains it after each funnel
/// call, so a `MemberUpdate` burst buffers once per member at most (the
/// funnel half already gates on the pre-update pending flag, and the store
/// dedupes repeats on the idempotency key).
#[derive(Debug, Clone, Default)]
pub struct DeferredCommunityFacts(Arc<Mutex<Vec<RulesAcceptedWrite>>>);

impl DeferredCommunityFacts {
    /// Empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take every buffered write, leaving the buffer empty. Order is arrival
    /// order; gate-clear facts are order-insensitive (once per member).
    pub fn take(&self) -> Vec<RulesAcceptedWrite> {
        std::mem::take(&mut *self.0.lock().expect("community facts lock"))
    }

    /// Buffered write count (tests, health).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().expect("community facts lock").len()
    }

    /// Whether the buffer holds no writes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl FactsSink for DeferredCommunityFacts {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, fact: RulesAcceptedFact<'_>) {
        self.0
            .lock()
            .expect("community facts lock")
            .push(RulesAcceptedWrite {
                guild_id: fact.guild_id,
                member_id: fact.member_id,
                is_bot: fact.is_bot,
                occurred_at: fact.occurred_at.to_owned(),
                source_event_id: fact.source_event_id.clone(),
                source: fact.source.to_owned(),
            });
    }

    fn record_message(&self, _fact: MessageFact<'_>) {}

    fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
        None
    }

    fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
}

/// One drain report: inserted rows versus idempotency-key duplicates.
#[cfg(feature = "db")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommunityDrainOutcome {
    /// Writes that inserted a new `community_facts` row.
    pub inserted: usize,
    /// Writes whose key already held a row (repeat gate-clear, redelivery).
    pub duplicates: usize,
}

/// Live gateway writer for the buffered facts. Built once at boot from the
/// process environment: capture runs only while `TWO_COMMUNITY_SCORECARD=1`
/// (the same gate that arms the Monday job, so coverage is never claimed
/// without capture). The classifier resolves once at boot like the scorecard
/// job's, so a mid-week env edit cannot split fact classification.
#[cfg(feature = "db")]
#[derive(Clone)]
pub struct CommunityFactsRuntime {
    pool: sqlx::PgPool,
    classifier: ClassifierConfig,
}

#[cfg(feature = "db")]
impl CommunityFactsRuntime {
    /// Build the writer over an explicit classifier (tests).
    #[must_use]
    pub fn new(pool: sqlx::PgPool, classifier: ClassifierConfig) -> Self {
        Self { pool, classifier }
    }

    /// Build the writer, or `None` when capture stays off. A disabled or
    /// misconfigured gate parks capture with a warn; it never fails boot.
    #[must_use]
    pub fn from_env(pool: sqlx::PgPool) -> Option<Self> {
        match ScorecardGates::from_env() {
            Ok(gates) if gates.enabled => Some(Self {
                pool,
                classifier: ClassifierConfig::from_env(),
            }),
            Ok(_) => {
                tracing::warn!(
                    "TWO_COMMUNITY_SCORECARD is not enabled; rules_accepted capture stays off"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "invalid community scorecard gates; rules_accepted capture stays off"
                );
                None
            }
        }
    }

    /// Classify and persist gate-clearings taken from the buffer. Each write
    /// is independent (distinct idempotency keys), so one failure never skips
    /// the rest; the first error is reported after the pass and the caller
    /// logs and continues. A lost fact fails the scorecard closed, while
    /// stalling the cursor would lose the funnel row too — and re-queueing
    /// would grow without bound when the table itself is missing. Never logs
    /// row contents or connection details.
    pub async fn drain_writes(
        &self,
        writes: Vec<RulesAcceptedWrite>,
    ) -> Result<CommunityDrainOutcome, two_bot_core::community_store::CommunityStoreError> {
        use two_bot_core::community_store::{record_fact, rules_accepted_fact};

        let mut outcome = CommunityDrainOutcome::default();
        let mut failures = 0usize;
        let mut failure = None;
        for write in writes {
            let input = ClassifyInput {
                guild_id: write.guild_id.to_string(),
                actor_id: write.member_id.to_string(),
                is_bot: write.is_bot,
                webhook_id: None,
                is_staff_automation: false,
                is_raid: false,
                is_staging: false,
                is_test: false,
            };
            let verdict = classify(&self.classifier, &input);
            let fact = rules_accepted_fact(
                &input.guild_id,
                &input,
                &write.occurred_at,
                &write.source_event_id,
                &write.source,
                verdict,
                None,
            );
            match record_fact(&self.pool, &fact).await {
                Ok(true) => outcome.inserted += 1,
                Ok(false) => outcome.duplicates += 1,
                Err(error) => {
                    failures += 1;
                    failure = Some(error);
                }
            }
        }
        if failures > 0 {
            tracing::warn!(
                failures,
                "community rules_accepted drain dropped writes; scorecard fails closed"
            );
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(outcome),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate<'a>(
        member_id: Snowflake,
        occurred_at: &'a str,
        source: &'a str,
    ) -> RulesAcceptedFact<'a> {
        RulesAcceptedFact {
            guild_id: 100,
            member_id,
            is_bot: false,
            occurred_at,
            source_event_id: format!("100:{member_id}:rules"),
            source,
        }
    }

    #[test]
    fn take_returns_arrival_order_and_empties() {
        let buffer = DeferredCommunityFacts::new();
        assert!(buffer.is_empty());
        buffer.record_rules_accepted(gate(1, "2026-09-20T12:00:00.000Z", "gateway"));
        buffer.record_rules_accepted(gate(2, "2026-09-20T12:01:00.000Z", "backfill:member_list"));
        assert_eq!(buffer.len(), 2);
        let writes = buffer.take();
        assert_eq!(
            writes.iter().map(|w| w.member_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(writes[1].source, "backfill:member_list");
        assert!(buffer.is_empty());
        assert!(buffer.take().is_empty());
    }

    #[test]
    fn other_streams_do_not_buffer() {
        let buffer = DeferredCommunityFacts::new();
        buffer.record_member_join(MemberJoinFact {
            guild_id: 100,
            member_id: 1,
            is_bot: false,
            occurred_at: "2026-09-20T12:00:00.000Z",
            source_event_id: "100:1:2026-09-20T12:00:00.000Z",
            source: "invite:abc",
            inviter_id: None,
        });
        buffer.record_message(MessageFact {
            guild_id: 100,
            member_id: 1,
            is_bot: false,
            webhook_id: None,
            is_staff_automation: false,
            message_id: "m1",
            channel_id: 10,
            channel_class: two_bot_core::ChannelClass::Human,
            occurred_at: "2026-09-20T12:01:00.000Z",
        });
        assert_eq!(
            buffer.record_voice_started(VoiceStartedFact {
                guild_id: 100,
                member_id: 1,
                is_bot: false,
                channel_id: 10,
                occurred_at: "2026-09-20T12:02:00.000Z",
            }),
            None
        );
        buffer.record_voice_ended(VoiceEndedFact {
            guild_id: 100,
            member_id: 1,
            is_bot: false,
            session_key: "s".to_owned(),
            channel_id: 10,
            occurred_at: "2026-09-20T12:03:00.000Z",
            started_at: None,
            duration_seconds: None,
        });
        assert!(buffer.is_empty());
    }
}
