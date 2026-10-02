//! Expected/processed event evidence ledger (TOG-11019).
//!
//! The B2 staging soak must show **zero missed gateway events** over seven
//! days, but the funnel stores only semantic milestones: the first three
//! messages per member (`handlers.rs:566-626`), one row per voice boundary,
//! and checkpoint sequences that include unrelated/no-op dispatches. Counters
//! alone cannot prove completeness, so this module adds the missing
//! reconciliation seam: a bounded, in-memory ledger that pairs an independently
//! witnessed *expected* action list (QA's fixture script) against the bot's
//! actual store writes, and exports a sanitized packet for QA.
//!
//! Privacy bounds (not negotiable):
//!
//! * The ledger keys expected actions by caller-supplied **opaque aliases**.
//!   Raw member/channel/message IDs, contents, tokens and payloads never enter
//!   the exported packet; transient raw associations (idempotency keys, which
//!   embed member IDs) live only in memory and expire with the ledger.
//! * Bounded: at most [`MAX_EXPECTED_ACTIONS`] expected actions and
//!   [`MAX_RECEIPTS`] receipts. Overflow sets a flag and stops recording —
//!   coverage reads UNKNOWN/truncated, never assumed zero-loss.
//!
//! This crate never touches the network. The live collection procedure (which
//! fixture identity, which transport, caps, retention) is `docs/evidence-route.md`;
//! this module is the offline-testable data seam underneath it.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::funnel::{idempotency_key, now_iso, parse_iso_millis};
use crate::{EventType, FunnelEvent, FunnelStore, RecordOutcome, Snowflake};

/// Schema version stamped on every exported packet.
pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;

/// Procedure ceiling: expected fixture actions per collection window.
pub const MAX_EXPECTED_ACTIONS: usize = 20;

/// Procedure ceiling: processed receipts per collection window.
pub const MAX_RECEIPTS: usize = 60;

/// Nearest-match window pairing a receipt to an expected action when no exact
/// idempotency-key hint was supplied (live timestamps differ from planned ones).
pub const MATCH_WINDOW_MS: i64 = 60_000;

/// Event families the soak reconciles. Everything else (invite clicks,
/// onboarding prompts, inactivity sweeps, …) is out of scope: receipts for
/// those still count as collateral observations, never as matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventFamily {
    Join,
    Voice,
    Message,
}

impl EventFamily {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Join => "join",
            Self::Voice => "voice",
            Self::Message => "message",
        }
    }
}

/// Map a funnel event type to its evidence family, or `None` when out of scope.
#[must_use]
pub const fn family_of(event_type: EventType) -> Option<EventFamily> {
    match event_type {
        EventType::MemberJoin | EventType::MemberLeave | EventType::GateCleared => {
            Some(EventFamily::Join)
        }
        EventType::VoiceSessionStart
        | EventType::VoiceSessionEnd
        | EventType::FirstVoiceSession => Some(EventFamily::Voice),
        EventType::FirstMessage | EventType::SecondMessage | EventType::ThirdMessage => {
            Some(EventFamily::Message)
        }
        _ => None,
    }
}

/// Final disposition of one expected action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// A store write with this action's identity committed (`inserted: true`).
    Committed,
    /// Only duplicate writes observed (idempotent redelivery, `inserted: false`).
    Duplicate,
    /// Deliberately not a funnel row (bot/webhook/staff message, DM, ladder
    /// full). Declared by the collection procedure, never inferred.
    Excluded,
    /// The action failed before dispatch (failed fixture step, rejected write).
    Failed,
    /// Expected but never observed — a soak gap until proven otherwise.
    Unknown,
}

impl Disposition {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Duplicate => "duplicate",
            Self::Excluded => "excluded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// One independently witnessed expectation. `alias` is opaque (e.g. `m3`);
/// `key_hint` is the precomputed idempotency key when the fixture can pin it
/// (repeatable types with known stamps), enabling exact matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedAction {
    pub alias: String,
    pub family: EventFamily,
    pub expected_at: String,
    pub key_hint: Option<String>,
}

/// One observed store write. Transient: held in memory, never exported raw
/// (the key embeds member IDs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreReceipt {
    pub key: String,
    pub family: Option<EventFamily>,
    pub received_at: String,
    pub inserted: bool,
}

/// Reconciled outcome for one expected action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciledItem {
    pub alias: String,
    pub family: EventFamily,
    pub expected_at: String,
    pub received_at: Option<String>,
    pub disposition: Disposition,
    /// Bounded machine-readable reason (`ladder_full`, `bot_authored`, …).
    pub reason: Option<String>,
}

/// Full reconciliation: per-alias items plus collateral counts (committed or
/// duplicate writes that matched nothing expected — observed-but-unexpected,
/// counted, never attributed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    pub items: Vec<ReconciledItem>,
    pub collateral_committed: usize,
    pub collateral_duplicate: usize,
}

impl Reconciliation {
    #[must_use]
    pub fn matched(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.disposition == Disposition::Committed)
            .count()
    }

    #[must_use]
    pub fn gaps(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.disposition == Disposition::Unknown)
            .count()
    }
}

/// Bounded in-memory ledger pairing expected actions against store receipts.
#[derive(Debug, Default)]
pub struct EvidenceLedger {
    revision: String,
    expected: Vec<ExpectedAction>,
    receipts: Vec<StoreReceipt>,
    declared: Vec<ReconciledItem>,
    expected_overflow: bool,
    receipts_overflow: bool,
}

impl EvidenceLedger {
    /// Start a ledger for one collection window. `revision` is the tested /
    /// deployed bot revision the receipts must come from (caller-supplied).
    #[must_use]
    pub fn new(revision: String) -> Self {
        Self {
            revision,
            expected: Vec::new(),
            receipts: Vec::new(),
            declared: Vec::new(),
            expected_overflow: false,
            receipts_overflow: false,
        }
    }

    /// Record an expectation. Beyond [`MAX_EXPECTED_ACTIONS`] the action is
    /// dropped and the overflow flag is set (truncated, never silent).
    pub fn expect(&mut self, action: ExpectedAction) {
        if self.expected.len() >= MAX_EXPECTED_ACTIONS {
            self.expected_overflow = true;
            return;
        }
        self.expected.push(action);
    }

    /// Record one store write. Beyond [`MAX_RECEIPTS`] the receipt is dropped
    /// and the overflow flag is set.
    pub fn record_store_receipt(
        &mut self,
        key: String,
        event_type: EventType,
        received_at: String,
        inserted: bool,
    ) {
        if self.receipts.len() >= MAX_RECEIPTS {
            self.receipts_overflow = true;
            return;
        }
        self.receipts.push(StoreReceipt {
            key,
            family: family_of(event_type),
            received_at,
            inserted,
        });
    }

    /// Declare a procedure-known outcome for an expected alias
    /// (`Excluded` with a reason code, or `Failed`). A declared outcome wins
    /// over receipt matching; duplicates are a procedure error only in the
    /// sense that the first declaration wins.
    pub fn declare(&mut self, alias: &str, disposition: Disposition, reason: &str) {
        if !matches!(disposition, Disposition::Excluded | Disposition::Failed) {
            return;
        }
        let Some(action) = self.expected.iter().find(|e| e.alias == alias) else {
            return;
        };
        if self.declared.iter().any(|d| d.alias == alias) {
            return;
        }
        self.declared.push(ReconciledItem {
            alias: action.alias.clone(),
            family: action.family,
            expected_at: action.expected_at.clone(),
            received_at: None,
            disposition,
            reason: Some(reason.to_owned()),
        });
    }

    /// Feed receipts drained from a [`ReceiptingStore`]. Capped like direct
    /// records; `truncated` propagates the decorator's own overflow flag so a
    /// dropped receipt can never read as zero-loss downstream.
    pub fn ingest(&mut self, receipts: Vec<StoreReceipt>, truncated: bool) {
        self.receipts_overflow |= truncated;
        for r in receipts {
            if self.receipts.len() >= MAX_RECEIPTS {
                self.receipts_overflow = true;
                return;
            }
            self.receipts.push(r);
        }
    }

    /// Pair every expected action against receipts: exact idempotency-key match
    /// first, then nearest unclaimed same-family receipt inside
    /// [`MATCH_WINDOW_MS`]. Each receipt is claimed at most once.
    #[must_use]
    pub fn reconcile(&self) -> Reconciliation {
        let mut claimed: HashSet<usize> = HashSet::new();
        let mut items = Vec::with_capacity(self.expected.len());

        for action in &self.expected {
            if let Some(declared) = self.declared.iter().find(|d| d.alias == action.alias) {
                items.push(declared.clone());
                continue;
            }
            let mut best: Option<usize> = None;
            // Pass 1: exact key hint.
            if let Some(hint) = action.key_hint.as_deref() {
                best = self
                    .receipts
                    .iter()
                    .enumerate()
                    .position(|(i, r)| !claimed.contains(&i) && r.key == hint);
            }
            // Pass 2: nearest same-family receipt inside the window.
            if best.is_none() {
                let expected_ms = parse_iso_millis(&action.expected_at);
                let mut best_dist = i64::MAX;
                for (i, r) in self.receipts.iter().enumerate() {
                    if claimed.contains(&i) || r.family != Some(action.family) {
                        continue;
                    }
                    let dist = match (expected_ms, parse_iso_millis(&r.received_at)) {
                        (Some(e), Some(g)) => (g - e).abs(),
                        _ => continue,
                    };
                    if dist <= MATCH_WINDOW_MS && dist < best_dist {
                        best_dist = dist;
                        best = Some(i);
                    }
                }
            }
            match best {
                None => items.push(ReconciledItem {
                    alias: action.alias.clone(),
                    family: action.family,
                    expected_at: action.expected_at.clone(),
                    received_at: None,
                    disposition: Disposition::Unknown,
                    reason: None,
                }),
                Some(i) => {
                    claimed.insert(i);
                    let r = &self.receipts[i];
                    // Same-key twins are one redelivery set: claim them all, and
                    // a committed write anywhere in the set wins over duplicates.
                    let mut committed = r.inserted;
                    let received_at = r.received_at.clone();
                    let key = r.key.clone();
                    let family = r.family;
                    for (j, o) in self.receipts.iter().enumerate() {
                        if !claimed.contains(&j) && o.key == key && o.family == family {
                            claimed.insert(j);
                            committed |= o.inserted;
                        }
                    }
                    items.push(ReconciledItem {
                        alias: action.alias.clone(),
                        family: action.family,
                        expected_at: action.expected_at.clone(),
                        received_at: Some(received_at),
                        disposition: if committed {
                            Disposition::Committed
                        } else {
                            Disposition::Duplicate
                        },
                        reason: None,
                    });
                }
            }
        }

        let mut collateral_committed = 0;
        let mut collateral_duplicate = 0;
        for (i, r) in self.receipts.iter().enumerate() {
            if claimed.contains(&i) {
                continue;
            }
            if r.inserted {
                collateral_committed += 1;
            } else {
                collateral_duplicate += 1;
            }
        }

        Reconciliation {
            items,
            collateral_committed,
            collateral_duplicate,
        }
    }

    /// Sanitized QA packet: aliases, families, timestamps, dispositions and
    /// counts only. Idempotency keys and any raw identifiers never leave the
    /// process through this path.
    #[must_use]
    pub fn export(&self) -> serde_json::Value {
        let r = self.reconcile();
        serde_json::json!({
            "schema": EVIDENCE_SCHEMA_VERSION,
            "revision": self.revision,
            "generated_at": now_iso(),
            "caps": {
                "max_expected": MAX_EXPECTED_ACTIONS,
                "max_receipts": MAX_RECEIPTS,
                "match_window_ms": MATCH_WINDOW_MS,
            },
            "truncated": {
                "expected_overflow": self.expected_overflow,
                "receipts_overflow": self.receipts_overflow,
            },
            "items": r.items.iter().map(|i| serde_json::json!({
                "alias": i.alias,
                "family": i.family.as_str(),
                "expected_at": i.expected_at,
                "received_at": i.received_at,
                "disposition": i.disposition.as_str(),
                "reason": i.reason,
            })).collect::<Vec<_>>(),
            "counts": {
                "expected": self.expected.len(),
                "matched": r.matched(),
                "gaps": r.gaps(),
                "collateral_committed": r.collateral_committed,
                "collateral_duplicate": r.collateral_duplicate,
            },
        })
    }
}

/// [`FunnelStore`] decorator capturing per-write receipts: `inserted: true`
/// means the write committed (durable receipt), `false` means idempotent
/// redelivery. Drain with [`ReceiptingStore::drain_receipts`] and feed the
/// ledger; the decorator itself stays out of the export path.
#[derive(Debug, Default)]
pub struct ReceiptingStore<S> {
    inner: S,
    receipts: std::sync::Mutex<ReceiptLog>,
}

#[derive(Debug, Default)]
struct ReceiptLog {
    receipts: Vec<StoreReceipt>,
    overflow: bool,
}

impl<S> ReceiptingStore<S> {
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            receipts: std::sync::Mutex::new(ReceiptLog::default()),
        }
    }

    /// Access the wrapped store (row reads, milestone checks).
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Take captured receipts plus whether any were dropped at the cap.
    pub fn drain_receipts(&self) -> (Vec<StoreReceipt>, bool) {
        let mut log = self.receipts.lock().expect("receipts lock");
        let overflow = log.overflow;
        log.overflow = false;
        (std::mem::take(&mut log.receipts), overflow)
    }
}

impl<S: FunnelStore> FunnelStore for ReceiptingStore<S> {
    fn mark_bot(&self, guild_id: Snowflake, member_id: Snowflake) {
        self.inner.mark_bot(guild_id, member_id);
    }

    fn record(&self, event: FunnelEvent) -> RecordOutcome {
        let key = idempotency_key(&event);
        let family = family_of(event.event_type);
        let outcome = self.inner.record(event);
        let mut log = self.receipts.lock().expect("receipts lock");
        if log.receipts.len() >= MAX_RECEIPTS {
            log.overflow = true;
        } else {
            log.receipts.push(StoreReceipt {
                key,
                family,
                received_at: now_iso(),
                inserted: outcome.inserted,
            });
        }
        outcome
    }

    fn touch_activity(&self, guild_id: Snowflake, member_id: Snowflake, at: &str) {
        self.inner.touch_activity(guild_id, member_id, at);
    }

    fn next_message_rung(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
    ) -> Option<EventType> {
        self.inner.next_message_rung(guild_id, member_id, at)
    }

    fn has_event(&self, guild_id: Snowflake, member_id: Snowflake, event_type: EventType) -> bool {
        self.inner.has_event(guild_id, member_id, event_type)
    }

    // Forwarded explicitly: the trait default is a no-op, which would silently
    // drop invite snapshots when wrapping a durable store.
    fn stage_invite_snapshot(&self, snapshot: crate::gateway_funnel::InviteSnapshotWrite) {
        self.inner.stage_invite_snapshot(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(alias: &str, family: EventFamily, at: &str) -> ExpectedAction {
        ExpectedAction {
            alias: alias.to_owned(),
            family,
            expected_at: at.to_owned(),
            key_hint: None,
        }
    }

    #[test]
    fn family_mapping_covers_soak_events_only() {
        assert_eq!(family_of(EventType::MemberJoin), Some(EventFamily::Join));
        assert_eq!(family_of(EventType::MemberLeave), Some(EventFamily::Join));
        assert_eq!(family_of(EventType::GateCleared), Some(EventFamily::Join));
        assert_eq!(
            family_of(EventType::VoiceSessionStart),
            Some(EventFamily::Voice)
        );
        assert_eq!(
            family_of(EventType::VoiceSessionEnd),
            Some(EventFamily::Voice)
        );
        assert_eq!(
            family_of(EventType::FirstVoiceSession),
            Some(EventFamily::Voice)
        );
        assert_eq!(
            family_of(EventType::FirstMessage),
            Some(EventFamily::Message)
        );
        assert_eq!(
            family_of(EventType::ThirdMessage),
            Some(EventFamily::Message)
        );
        assert_eq!(family_of(EventType::InviteClick), None);
        assert_eq!(family_of(EventType::MemberInactive), None);
    }

    #[test]
    fn exact_key_match_beats_window_match() {
        let mut ledger = EvidenceLedger::new("rev-test".to_owned());
        ledger.expect(ExpectedAction {
            key_hint: Some("k-exact".to_owned()),
            ..action("a", EventFamily::Join, "2026-09-20T12:00:00.000Z")
        });
        ledger.record_store_receipt(
            "k-other".to_owned(),
            EventType::MemberJoin,
            "2026-09-20T12:00:01.000Z".to_owned(),
            true,
        );
        ledger.record_store_receipt(
            "k-exact".to_owned(),
            EventType::MemberJoin,
            "2026-09-20T12:00:02.000Z".to_owned(),
            true,
        );
        let r = ledger.reconcile();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].disposition, Disposition::Committed);
        assert_eq!(
            r.items[0].received_at.as_deref(),
            Some("2026-09-20T12:00:02.000Z")
        );
        assert_eq!(r.collateral_committed, 1);
    }

    #[test]
    fn missing_receipt_is_gap_not_zero_loss() {
        let mut ledger = EvidenceLedger::new("rev-test".to_owned());
        ledger.expect(action(
            "m4",
            EventFamily::Message,
            "2026-09-20T12:04:00.000Z",
        ));
        let r = ledger.reconcile();
        assert_eq!(r.items[0].disposition, Disposition::Unknown);
        assert_eq!(r.gaps(), 1);
        assert_eq!(r.matched(), 0);
    }

    #[test]
    fn duplicate_only_match_is_reported_not_merged() {
        let mut ledger = EvidenceLedger::new("rev-test".to_owned());
        ledger.expect(ExpectedAction {
            key_hint: Some("k-dup".to_owned()),
            ..action("j", EventFamily::Join, "2026-09-20T12:00:00.000Z")
        });
        ledger.record_store_receipt(
            "k-dup".to_owned(),
            EventType::MemberJoin,
            "2026-09-20T12:00:01.000Z".to_owned(),
            false,
        );
        let r = ledger.reconcile();
        assert_eq!(r.items[0].disposition, Disposition::Duplicate);
        assert_eq!(r.matched(), 0);
    }

    #[test]
    fn declared_exclusion_wins_over_receipts() {
        let mut ledger = EvidenceLedger::new("rev-test".to_owned());
        ledger.expect(action(
            "b1",
            EventFamily::Message,
            "2026-09-20T12:00:00.000Z",
        ));
        ledger.record_store_receipt(
            "k-b1".to_owned(),
            EventType::FirstMessage,
            "2026-09-20T12:00:00.000Z".to_owned(),
            true,
        );
        ledger.declare("b1", Disposition::Excluded, "bot_authored");
        let r = ledger.reconcile();
        assert_eq!(r.items[0].disposition, Disposition::Excluded);
        assert_eq!(r.items[0].reason.as_deref(), Some("bot_authored"));
        // The unclaimed receipt is collateral, not silently absorbed.
        assert_eq!(r.collateral_committed, 1);
    }

    #[test]
    fn caps_truncate_with_flags() {
        let mut ledger = EvidenceLedger::new("rev-test".to_owned());
        for i in 0..MAX_EXPECTED_ACTIONS + 5 {
            ledger.expect(action(
                &format!("a{i}"),
                EventFamily::Join,
                "2026-09-20T12:00:00.000Z",
            ));
        }
        for i in 0..MAX_RECEIPTS + 5 {
            ledger.record_store_receipt(
                format!("k{i}"),
                EventType::MemberJoin,
                "2026-09-20T12:00:00.000Z".to_owned(),
                true,
            );
        }
        let packet = ledger.export();
        assert_eq!(packet["truncated"]["expected_overflow"], true);
        assert_eq!(packet["truncated"]["receipts_overflow"], true);
        assert_eq!(packet["counts"]["expected"], MAX_EXPECTED_ACTIONS);
    }

    #[test]
    fn export_carries_no_raw_identifiers() {
        let mut ledger = EvidenceLedger::new("rev-test".to_owned());
        ledger.expect(ExpectedAction {
            alias: "m1".to_owned(),
            family: EventFamily::Message,
            expected_at: "2026-09-20T12:01:00.000Z".to_owned(),
            key_hint: Some(
                "100000000000000001:900000000000001111:first_message:2026-09-20T12:01:00.000Z"
                    .to_owned(),
            ),
        });
        ledger.record_store_receipt(
            "100000000000000001:900000000000001111:first_message:2026-09-20T12:01:00.000Z"
                .to_owned(),
            EventType::FirstMessage,
            "2026-09-20T12:01:00.000Z".to_owned(),
            true,
        );
        let raw = serde_json::to_string(&ledger.export()).expect("serializes");
        assert!(raw.contains("\"alias\":\"m1\""));
        assert!(!raw.contains("900000000000001111"), "member id leaked");
        assert!(!raw.contains("100000000000000001"), "guild id leaked");
        assert!(!raw.contains("first_message:2026"), "key material leaked");
    }

    #[test]
    fn receipting_store_captures_inserted_flag() {
        use crate::MemStore;
        let store = ReceiptingStore::new(MemStore::new());
        let event = FunnelEvent {
            guild_id: 1,
            member_id: Some(2),
            event_type: EventType::MemberJoin,
            occurred_at: "2026-09-20T12:00:00.000Z".to_owned(),
            source: "invite:x".to_owned(),
            metadata: None,
            dedupe_token: None,
        };
        assert!(store.record(event.clone()).inserted);
        assert!(!store.record(event).inserted);
        let (receipts, overflow) = store.drain_receipts();
        assert!(!overflow);
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].family, Some(EventFamily::Join));
        assert!(receipts[0].inserted);
        assert!(!receipts[1].inserted);
        assert_eq!(receipts[0].key, receipts[1].key);
    }

    #[test]
    fn window_match_tolerates_live_clock_skew() {
        let mut ledger = EvidenceLedger::new("rev-test".to_owned());
        // Live receipts carry processing time, not the planned fixture stamp:
        // 30 s of skew still pairs; 90 s stays a gap.
        ledger.expect(action("v", EventFamily::Voice, "2026-09-20T12:02:00.000Z"));
        ledger.record_store_receipt(
            "live-key-near".to_owned(),
            EventType::VoiceSessionStart,
            "2026-09-20T12:02:30.000Z".to_owned(),
            true,
        );
        ledger.expect(action(
            "late",
            EventFamily::Voice,
            "2026-09-20T12:10:00.000Z",
        ));
        ledger.record_store_receipt(
            "live-key-far".to_owned(),
            EventType::VoiceSessionEnd,
            "2026-09-20T12:11:30.000Z".to_owned(),
            true,
        );
        let r = ledger.reconcile();
        assert_eq!(r.items[0].disposition, Disposition::Committed);
        assert_eq!(
            r.items[0].received_at.as_deref(),
            Some("2026-09-20T12:02:30.000Z")
        );
        assert_eq!(r.items[1].disposition, Disposition::Unknown);
        assert_eq!(r.gaps(), 1);
    }

    #[test]
    fn offline_fixture_end_to_end_over_receipting_handlers() {
        use crate::{
            ChannelClass, FunnelHandlers, JoinInput, MemStore, MessageInput, NoopFacts,
            NoopLeveling, VoiceInput,
        };

        const G: u64 = 1;
        const MEMBER: u64 = 100;
        const TEXT_CHANNEL: u64 = 500;
        const VOICE_CHANNEL: u64 = 600;
        const T_JOIN: &str = "2026-09-20T12:00:00.000Z";
        const T_MSG: &str = "2026-09-20T12:01:00.000Z";
        const T_VOICE_START: &str = "2026-09-20T12:02:00.000Z";
        const T_VOICE_END: &str = "2026-09-20T12:05:00.000Z";
        const T_GHOST: &str = "2026-09-20T12:06:00.000Z";

        let handlers = FunnelHandlers::new(
            ReceiptingStore::new(MemStore::new()),
            Some(NoopLeveling),
            Some(NoopFacts),
        );

        // Independently witnessed fixture script: join, one message, one
        // voice session. The ghost member never acts; x1 is procedure-declared
        // out (bot-authored), exercising the declare path end to end.
        let join_out = handlers.on_join(JoinInput {
            guild_id: G,
            member_id: MEMBER,
            is_bot: false,
            source: "invite:fixture".to_owned(),
            occurred_at: Some(T_JOIN.to_owned()),
            inviter_id: None,
            source_event_id: None,
        });
        let msg_out = handlers.on_message(MessageInput {
            guild_id: G,
            member_id: MEMBER,
            is_bot: false,
            message_id: Some("fixture-m1".to_owned()),
            webhook_id: None,
            is_staff_automation: false,
            channel_id: TEXT_CHANNEL,
            channel_class: ChannelClass::Human,
            capture_only: false,
            occurred_at: Some(T_MSG.to_owned()),
        });
        let voice_start_out = handlers.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: MEMBER,
            is_bot: false,
            channel_id: VOICE_CHANNEL,
            occurred_at: Some(T_VOICE_START.to_owned()),
        });
        let voice_end_out = handlers.on_voice_leave(VoiceInput {
            guild_id: G,
            member_id: MEMBER,
            is_bot: false,
            channel_id: VOICE_CHANNEL,
            occurred_at: Some(T_VOICE_END.to_owned()),
        });

        let mut ledger = EvidenceLedger::new("rev-fixture".to_owned());
        for (alias, family, at, outcome) in [
            ("j1", EventFamily::Join, T_JOIN, &join_out),
            ("m1", EventFamily::Message, T_MSG, &msg_out),
            ("v1", EventFamily::Voice, T_VOICE_START, &voice_start_out),
            ("v2", EventFamily::Voice, T_VOICE_END, &voice_end_out),
        ] {
            let event = outcome.event.clone().expect("fixture step writes");
            ledger.expect(ExpectedAction {
                alias: alias.to_owned(),
                family,
                expected_at: at.to_owned(),
                key_hint: Some(idempotency_key(&event)),
            });
        }
        ledger.expect(action("ghost", EventFamily::Join, T_GHOST));
        ledger.expect(action("x1", EventFamily::Message, T_MSG));
        ledger.declare("x1", Disposition::Excluded, "bot_authored");

        let (receipts, truncated) = handlers.store().drain_receipts();
        assert!(!truncated);
        assert_eq!(receipts.len(), 5);
        ledger.ingest(receipts, truncated);

        let r = ledger.reconcile();
        assert_eq!(r.matched(), 4);
        assert_eq!(r.gaps(), 1);
        // The per-session start row matched nothing expected: observed but
        // unexpected, counted as collateral rather than absorbed.
        assert_eq!(r.collateral_committed, 1);
        assert_eq!(r.collateral_duplicate, 0);

        let packet = ledger.export();
        assert_eq!(packet["counts"]["expected"], 6);
        assert_eq!(packet["counts"]["matched"], 4);
        assert_eq!(packet["counts"]["gaps"], 1);
        let raw = serde_json::to_string(&packet).expect("serializes");
        assert!(!raw.contains("member_join"), "event type material leaked");
        assert!(!raw.contains("voice_session"), "event type material leaked");
        assert!(!raw.contains("first_message"), "event type material leaked");
        assert!(!raw.contains("channel:"), "channel material leaked");
        assert!(!raw.contains("fixture-m1"), "message id leaked");
        assert!(!raw.contains("invite:fixture"), "source material leaked");
        assert!(!raw.contains(":100:"), "member id leaked");
    }
}
