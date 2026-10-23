//! Mirror-delivery service for the operational audit sink (TOG-10345).
//!
//! Drives an [`AuditMirror`] through the durable [`AuditStore`] protocol from
//! `docs/audit-store.md` ("Crash and halt protocol"):
//!
//! 1. Route through the core guild/source/destination policies and `record`
//!    first — a failed durable write means nothing is ever delivered.
//! 2. `claim` splits work by intent: `Send` (unattempted) vs `Reconcile` (a
//!    boundary or accepted id already exists).
//! 3. `Send` preflights the private mirror (channel document, bounded dedup
//!    history scan) and persists `prepare_send` *before* any POST; the
//!    persistent halt is re-checked immediately before the POST. A halt before
//!    preparation releases unattempted ownership; a halt after preparation —
//!    with no POST begun — records definite non-acceptance.
//! 4. Accepted posts persist `note_accepted` then `complete`; a failed DB
//!    write never retriggers a send (the boundary survives for the next owner).
//! 5. `Reconcile` never POSTs: it verifies bot-author + exact marker identity
//!    at or after the boundary, finishes a recorded accepted id, or
//!    quarantines. Ambiguous reads stay fenced, never cleared by a read error.
//! 6. Halt is read fresh from the store on every check — no cache — through
//!    the core kill-switch model, which fails open on read failure.
//!
//! Gateway/moderation wiring that *produces* the events is TOG-10346; this
//! slice is the delivery engine only. It performs no pacing or retry policy
//! of its own — that lives in the [`AuditMirror`] implementation.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tokio::time::Instant as TokioInstant;

use crate::audit::{
    delivery_nonce, format_audit_event, route_for_sink, AuditChannelIds, AuditEvent, KillSwitchLog,
    KillSwitchSnapshot, SinkRoute,
};
use crate::audit_mirror::{
    find_mirror_in_page, mirror_channel_policy, next_snowflake, page_floor_id,
    snowflake_at_or_after, validate_history_page, AuditMirror, MirrorError, MirrorGate,
    DEDUP_PAGE_LIMIT, HISTORY_PAGE_LIMIT,
};
use crate::audit_store::{
    AuditClaim, AuditStore, AuditStoreError, DeliveryFailure, DeliveryIntent, PrepareSend,
    QuarantineReason, ReconcileBound,
};

/// Maximum history pages per Reconcile claim. At the cap, fully inspected
/// progress is checkpointed for the next owner without rereading the head.
/// Two consecutive bounds without progress quarantine as
/// [`QuarantineReason::ScanBoundExceeded`], not proof of marker absence.
pub const RECONCILE_MAX_PAGES: usize = 16;

/// Wall-clock history-read budget per Reconcile claim, alongside
/// [`RECONCILE_MAX_PAGES`]. A timed-out page cannot establish a checkpoint.
pub const RECONCILE_SCAN_TIMEOUT: Duration = Duration::from_secs(30);

/// Static destination wiring for the service.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// Per-channel-family destinations; voice and moderation fall back to
    /// `audit` when unset (`AuditChannelIds::channel_for`).
    pub channels: AuditChannelIds,
    /// Mirror channel ids used for loop suppression: a row whose
    /// `source_channel_id` is one of these is stored but never mirrored.
    pub configured: Vec<String>,
    /// Guild the mirror channels live in. `None`/empty disables mirroring:
    /// every requested route becomes `WrongGuild`.
    pub mirror_guild_id: Option<String>,
    /// This bot's own user id — the author identity reconciliation requires.
    pub bot_user_id: String,
}

/// What `record` decided for one incoming event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// New pending row carrying a mirror destination.
    Queued { mirror_channel_id: String },
    /// Durable replay: first facts won, nothing re-queued.
    Replay,
    /// Source channel is itself a configured mirror channel: stored, never
    /// mirrored (`stored` reports whether this insert was new).
    TamperLoop { stored: bool },
    /// Destination resolves but lives outside the event's guild: stored,
    /// never mirrored.
    WrongGuild { requested_channel_id: String },
    /// No destination resolves for the row's channel family: stored only.
    StoreOnly,
}

/// Outcome of one claimed row's delivery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliverOutcome {
    /// The POST was accepted and `note_accepted` + `complete` persisted.
    Delivered { message_id: String },
    /// A matching mirror already existed (or the accepted id was recorded):
    /// adopted as the durable accepted id without posting.
    Reconciled { message_id: String },
    /// The persistent halt held the row: released unattempted (before
    /// preparation), recorded as definite non-acceptance (prepared but no
    /// POST began), or left to lease expiry (reconcile owner).
    Held,
    /// Transient preflight failure: `defer_preflight` hid the row from
    /// discovery for the backoff window.
    Deferred,
    /// Provably-unsent POST failure recorded (`DefinitelyRejected`):
    /// retryable by the next claim.
    Rejected,
    /// Ambiguous POST/read outcome recorded (`UncertainAcceptance`): the row
    /// keeps its boundary and is reconcile-only.
    Ambiguous,
    /// Terminal hold with the bounded reason stored.
    Quarantined(QuarantineReason),
    /// The boundary scan hit [`RECONCILE_MAX_PAGES`] or [`RECONCILE_SCAN_TIMEOUT`]
    /// with the marker neither proven nor disproven: checkpoint verified
    /// progress and yield, preserving the boundary. A first no-progress bound
    /// also yields; a consecutive no-progress bound quarantines for inspection.
    ScanBoundReached,
    /// A store transition refused (stale/lost claim): nothing was sent and
    /// nothing was written by this pass.
    Unclaimed,
}

enum SendStop {
    Held,
    LostClaim,
    Store(AuditStoreError),
}

/// Per-entry result of one `drain_pending` sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DrainReport {
    /// `(entry_id, outcome)` for every claimed row, in claim order.
    pub deliveries: Vec<(String, DeliverOutcome)>,
    /// `(entry_id, store error)` for rows whose delivery hit a DB failure —
    /// DB and Discord share no transaction, so these rows keep their durable
    /// evidence for the next owner.
    pub failed: Vec<(String, String)>,
}

/// The delivery engine: `AuditStore` fencing + `AuditMirror` transport +
/// core routing/policy. Cloning is not supported — one service per worker.
pub struct AuditMirrorService<M: AuditMirror> {
    store: AuditStore,
    mirror: M,
    config: MirrorConfig,
    /// Last *observed* halt state for transition logging only — never used
    /// as the truth; every check reads the store.
    observed: Mutex<Option<bool>>,
    /// Kill-switch transitions logged this run (test/observability surface).
    transitions: Mutex<Vec<KillSwitchLog>>,
    /// Test seam invoked between a successful `prepare_send` and the
    /// mandatory pre-POST halt re-check, so fault-injection tests can land a
    /// halt deterministically in that window.
    pre_send_gate: Option<Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>>,
    /// Fixed Reconcile scan deadline (tests only): `None` (production)
    /// derives it from [`RECONCILE_SCAN_TIMEOUT`] on every pass, while `Some`
    /// pins it — a past deadline stops the scan with `ScanBoundReached`
    /// before any read, pinning the timeout half of the bound without
    /// sleeping 30 s. See [`AuditMirrorService::with_reconcile_scan_deadline`].
    reconcile_scan_deadline: Option<Instant>,
}

impl<M: AuditMirror> AuditMirrorService<M> {
    pub fn new(store: AuditStore, mirror: M, config: MirrorConfig) -> Self {
        Self {
            store,
            mirror,
            config,
            observed: Mutex::new(None),
            transitions: Mutex::new(Vec::new()),
            pre_send_gate: None,
            reconcile_scan_deadline: None,
        }
    }

    /// Install the pre-POST seam (tests only; see field docs).
    #[must_use]
    pub fn with_pre_send_gate<F>(mut self, gate: F) -> Self
    where
        F: Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        self.pre_send_gate = Some(Box::new(gate));
        self
    }

    /// Fix the Reconcile scan deadline (tests only; see field docs).
    #[must_use]
    pub fn with_reconcile_scan_deadline(mut self, deadline: Instant) -> Self {
        self.reconcile_scan_deadline = Some(deadline);
        self
    }

    /// Kill-switch transitions observed so far, in order.
    pub fn transitions(&self) -> Vec<KillSwitchLog> {
        self.transitions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Route + record: the durable write precedes any delivery consideration,
    /// and its failure stops the caller before any Discord traffic.
    pub async fn record(&self, event: &AuditEvent) -> Result<RecordOutcome, AuditStoreError> {
        // Preview the route to learn the destination; the preview's `stored`
        // flag is meaningless and discarded with it.
        let preview = route_for_sink(
            event,
            &self.config.channels,
            &self.config.configured,
            self.config.mirror_guild_id.as_deref(),
            false,
        );
        let mirror_channel_id = match &preview {
            SinkRoute::Mirror { mirror_channel_id } => Some(mirror_channel_id.as_str()),
            _ => None,
        };
        let stored = self.store.record(event, mirror_channel_id).await?;
        // Re-route with the real stored flag so TamperLoop reports it.
        Ok(
            match route_for_sink(
                event,
                &self.config.channels,
                &self.config.configured,
                self.config.mirror_guild_id.as_deref(),
                stored,
            ) {
                SinkRoute::Mirror { mirror_channel_id } if stored => {
                    RecordOutcome::Queued { mirror_channel_id }
                }
                SinkRoute::Mirror { .. } => RecordOutcome::Replay,
                SinkRoute::TamperLoop { stored } => RecordOutcome::TamperLoop { stored },
                SinkRoute::WrongGuild {
                    requested_channel_id,
                } => RecordOutcome::WrongGuild {
                    requested_channel_id,
                },
                SinkRoute::StoreOnly => RecordOutcome::StoreOnly,
            },
        )
    }

    /// Read the persistent halt fresh and classify through the core model.
    /// A failed read fails open (`false`) with a `ReadFailed` log entry —
    /// legacy `deliveryHalted()`. Never errors.
    async fn check_halt(&self) -> bool {
        let (halted, read_failed) = match self.store.delivery_halt().await {
            Ok(halt) => (halt.is_some(), false),
            Err(err) => {
                tracing::warn!(error = %err, "audit delivery halt read failed; failing open");
                (false, true)
            }
        };
        let observed = *self.observed.lock().unwrap_or_else(|e| e.into_inner());
        let decision = KillSwitchSnapshot {
            observed_halted: observed,
            halted,
            read_failed,
        }
        .decide();
        *self.observed.lock().unwrap_or_else(|e| e.into_inner()) = decision.observed_halted;
        if let Some(log) = decision.log {
            self.transitions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(log);
        }
        decision.halted
    }

    /// Claim one pending row and drive it to a terminal-or-held outcome.
    /// `Unclaimed` covers missing, halted, deferred and lost-fence rows.
    pub async fn deliver_entry(&self, entry_id: &str) -> Result<DeliverOutcome, AuditStoreError> {
        self.deliver_entry_before(entry_id, None).await
    }

    async fn deliver_entry_before(
        &self,
        entry_id: &str,
        deadline: Option<TokioInstant>,
    ) -> Result<DeliverOutcome, AuditStoreError> {
        let Some(claim) = self.store.claim(entry_id).await? else {
            return Ok(DeliverOutcome::Unclaimed);
        };
        self.deliver_before(&claim, deadline).await
    }

    /// Drive an already-held claim (Send or Reconcile by its stored intent).
    pub async fn deliver(&self, claim: &AuditClaim) -> Result<DeliverOutcome, AuditStoreError> {
        self.deliver_before(claim, None).await
    }

    async fn deliver_before(
        &self,
        claim: &AuditClaim,
        deadline: Option<TokioInstant>,
    ) -> Result<DeliverOutcome, AuditStoreError> {
        match claim.intent() {
            DeliveryIntent::Send => self.deliver_send(claim).await,
            DeliveryIntent::Reconcile => self.deliver_reconcile(claim, deadline).await,
        }
    }

    /// Claim and deliver every row `pending_ids` surfaces (bounded batch);
    /// per-row store errors are collected, never fatal to the sweep.
    pub async fn drain_pending(&self) -> Result<DrainReport, AuditStoreError> {
        self.drain_pending_before(None).await
    }

    /// Drain within one caller's remaining sweep budget. Stop claiming rows at
    /// the deadline and cap each Reconcile scan at that same instant, leaving
    /// checkpoint/release time reserved by the caller before its outer timeout.
    /// An in-flight Send keeps its existing durable delivery protocol.
    pub async fn drain_pending_until(
        &self,
        deadline: TokioInstant,
    ) -> Result<DrainReport, AuditStoreError> {
        self.drain_pending_before(Some(deadline)).await
    }

    async fn drain_pending_before(
        &self,
        deadline: Option<TokioInstant>,
    ) -> Result<DrainReport, AuditStoreError> {
        let mut report = DrainReport::default();
        for entry_id in self.store.pending_ids().await? {
            if deadline.is_some_and(|deadline| TokioInstant::now() >= deadline) {
                break;
            }
            match self.deliver_entry_before(&entry_id, deadline).await {
                Ok(outcome) => report.deliveries.push((entry_id, outcome)),
                Err(err) => report.failed.push((entry_id, err.to_string())),
            }
        }
        Ok(report)
    }

    /// `Send` intent: preflight the destination, persist the boundary, then
    /// post — with the persistent halt checked immediately before the POST.
    async fn deliver_send(&self, claim: &AuditClaim) -> Result<DeliverOutcome, AuditStoreError> {
        let row = claim.row();
        let Some(destination) = row.mirror_channel_id.clone() else {
            // A pending row without a destination can never send; leave it
            // claimed (lease expiry re-queues it) rather than release it into
            // a claim/release spin.
            return Ok(DeliverOutcome::Unclaimed);
        };
        let entry_id = row.event.entry_id.clone();
        let event_guild_id = row.event.guild_id.clone();

        // Persistent halt first: while engaged, rows are released unattempted
        // and no Discord traffic happens.
        if self.check_halt().await {
            self.store.release_unattempted(claim).await?;
            return Ok(DeliverOutcome::Held);
        }

        // Preflight 1 — the channel document gates guild + privacy. A refused
        // read is proof the permission is gone (or the channel is): terminal.
        // A transient read failure defers without counting an attempt.
        let document = match self.mirror.channel_document(&destination).await {
            Ok(document) => document,
            Err(MirrorError::Rejected(_)) => {
                return self
                    .quarantine(claim, QuarantineReason::PermissionRevoked)
                    .await;
            }
            Err(MirrorError::RateLimited | MirrorError::Uncertain(_)) => {
                return self.defer(claim).await;
            }
        };
        match mirror_channel_policy(&document, &event_guild_id) {
            MirrorGate::Clear => {}
            MirrorGate::WrongGuild => {
                return self
                    .quarantine(claim, QuarantineReason::EvidenceConflict)
                    .await;
            }
            MirrorGate::PublicChannel => {
                return self
                    .quarantine(claim, QuarantineReason::PermissionRevoked)
                    .await;
            }
        }

        // Preflight 2 — bounded dedup scan (legacy `findMirror`): a mirror
        // carrying this event's marker may already exist. Page 1's newest id
        // seeds the durable `search_before` boundary (`"0"` on empty history).
        let mut search_before = String::from("0");
        let mut found: Option<String> = None;
        let mut before: Option<String> = None;
        for page_no in 0..DEDUP_PAGE_LIMIT {
            let page = match self
                .mirror
                .channel_history(&destination, before.as_deref(), HISTORY_PAGE_LIMIT)
                .await
                .and_then(|page| {
                    // Enforce the contract even for alternate mirror adapters:
                    // no marker, boundary or floor may use an invalid page.
                    validate_history_page(&page, before.as_deref(), HISTORY_PAGE_LIMIT)?;
                    Ok(page)
                }) {
                Ok(page) => page,
                Err(MirrorError::Rejected(_)) => {
                    return self
                        .quarantine(claim, QuarantineReason::PermissionRevoked)
                        .await;
                }
                Err(MirrorError::RateLimited | MirrorError::Uncertain(_)) => {
                    return self.defer(claim).await;
                }
            };
            if page_no == 0 {
                search_before = match page.first() {
                    None => String::from("0"),
                    Some(newest) => match next_snowflake(&newest.id) {
                        Some(cursor) => cursor,
                        // A malformed newest id cannot seed a boundary; treat
                        // the page as unreadable rather than record `"0"`.
                        None => return self.defer(claim).await,
                    },
                };
            }
            if let Some(id) = find_mirror_in_page(&page, &entry_id, &self.config.bot_user_id, None)
            {
                found = Some(id);
                break;
            }
            match page_floor_id(&page) {
                // A short page reached the channel's start: stop.
                Some(floor) if page.len() == usize::from(HISTORY_PAGE_LIMIT) => {
                    before = Some(floor);
                }
                _ => break,
            }
        }

        // Adoption must survive a crash before note_accepted: its recovery
        // boundary includes the known mirror, not the newest id's successor.
        let boundary = found.as_deref().unwrap_or(&search_before);
        // Persist the boundary + attempt count *before* the POST decision.
        match self.store.prepare_send(claim, boundary).await? {
            PrepareSend::Prepared => {}
            PrepareSend::Halted => {
                // Halt landed between claim and preparation; no boundary was
                // committed, so unattempted release is safe.
                self.store.release_unattempted(claim).await?;
                return Ok(DeliverOutcome::Held);
            }
            PrepareSend::LostClaim => return Ok(DeliverOutcome::Unclaimed),
        }

        if let Some(message_id) = found {
            // The mirror already carries this event: adopt its id as the
            // accepted message (no POST — dedup fulfilled delivery).
            return Ok(
                if self.store.note_accepted(claim, &message_id).await?
                    && self.store.complete(claim).await?
                {
                    DeliverOutcome::Reconciled { message_id }
                } else {
                    DeliverOutcome::Unclaimed
                },
            );
        }

        // Deterministic seam for "halt lands between prepare and POST".
        if let Some(gate) = &self.pre_send_gate {
            gate().await;
        }
        let nonce = row
            .nonce
            .clone()
            .unwrap_or_else(|| delivery_nonce(&entry_id));
        let content = format_audit_event(&row.event);
        let posted = match self
            .mirror
            .post_mirror_checked(&destination, &content, &nonce, async {
                // The adapter runs this only AFTER reserving its pacing lane.
                // A halt is definite non-acceptance; a lost fence must never
                // POST or clear the replacement owner's recovery evidence.
                let permission = if self.check_halt().await {
                    PrepareSend::Halted
                } else {
                    self.store
                        .check_prepared_send(claim)
                        .await
                        .map_err(SendStop::Store)?
                };
                match permission {
                    PrepareSend::Prepared => Ok(()),
                    PrepareSend::LostClaim => Err(SendStop::LostClaim),
                    PrepareSend::Halted => {
                        if self
                            .store
                            .fail_attempt(claim, DeliveryFailure::DefinitelyRejected)
                            .await
                            .map_err(SendStop::Store)?
                        {
                            Err(SendStop::Held)
                        } else {
                            Err(SendStop::LostClaim)
                        }
                    }
                }
            })
            .await
        {
            Ok(posted) => posted,
            Err(SendStop::Held) => return Ok(DeliverOutcome::Held),
            Err(SendStop::LostClaim) => return Ok(DeliverOutcome::Unclaimed),
            Err(SendStop::Store(error)) => return Err(error),
        };
        match posted {
            Ok(message_id) if !message_id.is_empty() => {
                // Accepted with a real id: persist acceptance, then finish.
                // Either write failing must NOT resend — the boundary holds.
                Ok(
                    if self.store.note_accepted(claim, &message_id).await?
                        && self.store.complete(claim).await?
                    {
                        DeliverOutcome::Delivered { message_id }
                    } else {
                        DeliverOutcome::Unclaimed
                    },
                )
            }
            Ok(_) => {
                // Accepted status without an id: ambiguous — reconcile-only.
                self.store
                    .fail_attempt(claim, DeliveryFailure::UncertainAcceptance)
                    .await?;
                Ok(DeliverOutcome::Ambiguous)
            }
            Err(MirrorError::Rejected(_) | MirrorError::RateLimited) => {
                // Provably unsent (4xx, or a pre-handler rate-limit refusal):
                // clear the boundary, retry is legal.
                self.store
                    .fail_attempt(claim, DeliveryFailure::DefinitelyRejected)
                    .await?;
                Ok(DeliverOutcome::Rejected)
            }
            Err(MirrorError::Uncertain(_)) => {
                // Timeout/transport/5xx: the post may exist. Keep the
                // boundary; the next owner reconciles instead of resending.
                self.store
                    .fail_attempt(claim, DeliveryFailure::UncertainAcceptance)
                    .await?;
                Ok(DeliverOutcome::Ambiguous)
            }
        }
    }

    /// `Reconcile` intent: never POSTs. Finish a recorded accepted id, or
    /// prove the mirror exists by bot-author + exact marker at/after the
    /// boundary; anything else quarantines or stays fenced.
    async fn deliver_reconcile(
        &self,
        claim: &AuditClaim,
        caller_deadline: Option<TokioInstant>,
    ) -> Result<DeliverOutcome, AuditStoreError> {
        let row = claim.row();
        // The durable accepted id wins outright: a crash between the POST and
        // the `complete` write needs no Discord traffic at all.
        if let Some(message_id) = row.mirror_message_id.clone() {
            return Ok(if self.store.complete(claim).await? {
                DeliverOutcome::Reconciled { message_id }
            } else {
                DeliverOutcome::Unclaimed
            });
        }
        let (Some(destination), Some(boundary)) =
            (row.mirror_channel_id.clone(), row.search_before.clone())
        else {
            // Reconcile intent guarantees a boundary or an accepted id; a row
            // missing both is outside this protocol.
            return Ok(DeliverOutcome::Unclaimed);
        };
        let entry_id = row.event.entry_id.clone();

        // Halt holds the claim: no writes, lease expiry is the backstop.
        if self.check_halt().await {
            return Ok(DeliverOutcome::Held);
        }

        // Resume below fully inspected history. A progressing bound saves
        // the floor and yields; only consecutive no-progress bounds quarantine.
        // Neither transition proves absence or permits another POST.
        // Convert the explicit std-clock seam once; comparisons and read
        // cancellation must use the same monotonic clock. A sweep never grants
        // later rows a fresh caller budget after earlier rows consumed it.
        let local_deadline = self
            .reconcile_scan_deadline
            .map(TokioInstant::from_std)
            .unwrap_or_else(|| TokioInstant::now() + RECONCILE_SCAN_TIMEOUT);
        let scan_deadline = caller_deadline.map_or(local_deadline, |d| d.min(local_deadline));
        let mut pages: usize = 0;
        let mut before = row.reconcile_before.clone();
        loop {
            if pages >= RECONCILE_MAX_PAGES || TokioInstant::now() >= scan_deadline {
                return Ok(
                    match self
                        .store
                        .bound_reconcile_scan(claim, before.as_deref())
                        .await?
                    {
                        ReconcileBound::Yielded => DeliverOutcome::ScanBoundReached,
                        ReconcileBound::Quarantined => {
                            DeliverOutcome::Quarantined(QuarantineReason::ScanBoundExceeded)
                        }
                        ReconcileBound::LostClaim => DeliverOutcome::Unclaimed,
                    },
                );
            }
            let read = match tokio::time::timeout_at(
                scan_deadline,
                self.mirror
                    .channel_history(&destination, before.as_deref(), HISTORY_PAGE_LIMIT),
            )
            .await
            {
                Ok(read) => read,
                // Re-enter the bound transition with only earlier verified
                // pages. A cancelled read never establishes a checkpoint.
                Err(_) => continue,
            };
            let page = match read.and_then(|page| {
                // Enforce the contract even for alternate mirror adapters:
                // no marker, boundary or floor may use an invalid page.
                validate_history_page(&page, before.as_deref(), HISTORY_PAGE_LIMIT)?;
                Ok(page)
            }) {
                Ok(page) => page,
                Err(MirrorError::Rejected(_)) => {
                    // Permission vanished between post and reconcile: terminal.
                    return self
                        .quarantine(claim, QuarantineReason::PermissionRevoked)
                        .await;
                }
                Err(MirrorError::RateLimited | MirrorError::Uncertain(_)) => {
                    // Ambiguous read: stays fenced for the next owner — a read
                    // error can never clear the earlier POST's ambiguity.
                    self.store
                        .fail_attempt(claim, DeliveryFailure::UncertainAcceptance)
                        .await?;
                    return Ok(DeliverOutcome::Ambiguous);
                }
            };
            pages += 1;
            if let Some(message_id) =
                find_mirror_in_page(&page, &entry_id, &self.config.bot_user_id, Some(&boundary))
            {
                return Ok(
                    if self.store.note_accepted(claim, &message_id).await?
                        && self.store.complete(claim).await?
                    {
                        DeliverOutcome::Reconciled { message_id }
                    } else {
                        DeliverOutcome::Unclaimed
                    },
                );
            }
            let Some(floor) = page_floor_id(&page) else {
                // Empty page: history exhausted with no match.
                return self
                    .quarantine(claim, QuarantineReason::MarkerMissing)
                    .await;
            };
            // Below the boundary nothing can match; a short page means the
            // channel's start was reached. Either way: marker proven absent.
            if !snowflake_at_or_after(&floor, &boundary)
                || page.len() < usize::from(HISTORY_PAGE_LIMIT)
            {
                return self
                    .quarantine(claim, QuarantineReason::MarkerMissing)
                    .await;
            }
            before = Some(floor);
            // A completed budget goes directly through the locked checkpoint
            // fence. Do not renew a lost/accepted owner while yielding it.
            if pages < RECONCILE_MAX_PAGES
                && TokioInstant::now() < scan_deadline
                && !self.store.renew(claim).await?
            {
                return Ok(DeliverOutcome::Unclaimed);
            }
        }
    }

    async fn defer(&self, claim: &AuditClaim) -> Result<DeliverOutcome, AuditStoreError> {
        Ok(if self.store.defer_preflight(claim).await? {
            DeliverOutcome::Deferred
        } else {
            DeliverOutcome::Unclaimed
        })
    }

    async fn quarantine(
        &self,
        claim: &AuditClaim,
        reason: QuarantineReason,
    ) -> Result<DeliverOutcome, AuditStoreError> {
        Ok(if self.store.quarantine(claim, reason).await? {
            DeliverOutcome::Quarantined(reason)
        } else {
            DeliverOutcome::Unclaimed
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::PgPool;

    use super::*;
    use crate::audit::AuditKind;
    use crate::audit_mirror::{MirrorChannel, MirrorMessage};
    use crate::audit_store::{AuditStore, DeliveryState};

    const MIGRATION_0340: &str =
        include_str!("../../cutover/migrations/0340_operational_audit.sql");

    const GUILD: &str = "18446744073709551615";
    const AUDIT_CH: &str = "1111";
    const VOICE_CH: &str = "2222";
    const MOD_CH: &str = "3333";
    const BOT: &str = "9999";

    /// Only an absent URL skips the test. Configured setup failures are errors.
    async fn test_pool(prefix: &str) -> Result<Option<(PgPool, String)>, sqlx::Error> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => {
                return Err(sqlx::Error::Configuration(
                    "TWO_TEST_DATABASE_URL must be Unicode".into(),
                ))
            }
        };
        let options: PgConnectOptions = url.parse()?;
        // No DATABASE_URL or inherited credentials: tests accept only the
        // approved agent container or the ephemeral Postgres service in
        // GitHub Actions (see the `community-db` CI job).
        let host = options.get_host();
        assert!(
            host == "agent-testdb"
                || (host == "localhost"
                    && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
            "test-container-only URL"
        );
        assert_eq!(
            options.get_username(),
            "agent_test",
            "test-container-only user"
        );
        // No shared fixed-name fixtures: each invocation owns its schema,
        // including concurrent invocations of the same test in other processes.
        static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!(
            "{prefix}_{}_{nonce}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        assert!(is_test_schema_name(&schema), "safe unique test schema");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        // SAFETY: only the guarded identifier interpolates; values bind.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        sqlx::raw_sql(MIGRATION_0340).execute(&pool).await?;
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0462_audit_reconcile_cursor.sql"
        ))
        .execute(&pool)
        .await?;
        Ok(Some((pool, schema)))
    }

    /// Test-only schema names: lowercase identifier characters, so DDL
    /// interpolation cannot break out of the identifier position.
    fn is_test_schema_name(schema: &str) -> bool {
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    async fn drop_schema(pool: &PgPool, schema: &str) {
        assert!(is_test_schema_name(schema));
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    type HistoryPage = Result<Vec<MirrorMessage>, MirrorError>;

    /// Scripted mirror double: empty history is the default; reads and actual
    /// POSTs count separately so reconciliation cannot hide a second send.
    #[derive(Clone, Default)]
    struct ScriptMirror {
        post_script: Arc<Mutex<VecDeque<Result<String, MirrorError>>>>,
        history: Arc<Mutex<VecDeque<HistoryPage>>>,
        history_reads: Arc<AtomicU64>,
        posts: Arc<AtomicU64>,
    }

    impl ScriptMirror {
        fn reads(&self) -> u64 {
            self.history_reads.load(Ordering::Relaxed)
        }
    }

    impl AuditMirror for ScriptMirror {
        async fn post_mirror_checked<Fut, E>(
            &self,
            channel_id: &str,
            content: &str,
            nonce: &str,
            authorize: Fut,
        ) -> Result<Result<String, MirrorError>, E>
        where
            Fut: std::future::Future<Output = Result<(), E>> + Send,
            E: Send,
        {
            authorize.await?;
            let _ = (channel_id, content, nonce);
            self.posts.fetch_add(1, Ordering::Relaxed);
            Ok(self
                .post_script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("900000".to_owned())))
        }

        async fn channel_document(&self, channel_id: &str) -> Result<MirrorChannel, MirrorError> {
            let _ = channel_id;
            Ok(MirrorChannel {
                guild_id: GUILD.to_owned(),
                everyone: Some(crate::audit_mirror::MirrorOverwrite {
                    allow: "0".to_owned(),
                    deny: "1024".to_owned(),
                }),
            })
        }

        async fn channel_history(
            &self,
            channel_id: &str,
            _before: Option<&str>,
            _limit: u8,
        ) -> Result<Vec<MirrorMessage>, MirrorError> {
            let _ = channel_id;
            self.history_reads.fetch_add(1, Ordering::Relaxed);
            self.history
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(vec![]))
        }
    }

    fn event(entry: &str) -> AuditEvent {
        let mut event = AuditEvent::new(
            entry.to_owned(),
            AuditKind::MemberUpdate,
            GUILD.to_owned(),
            "2026-09-30T00:01:02.003Z".to_owned(),
        );
        event.actor_id = Some("18446744073709551614".to_owned());
        event.target_id = Some("18446744073709551613".to_owned());
        event.metadata_json = r#"{"nicknameChanged":true}"#.to_owned();
        event
    }

    fn config() -> MirrorConfig {
        MirrorConfig {
            channels: AuditChannelIds {
                audit: Some(AUDIT_CH.to_owned()),
                voice: Some(VOICE_CH.to_owned()),
                moderation: Some(MOD_CH.to_owned()),
            },
            configured: vec![AUDIT_CH.to_owned(), VOICE_CH.to_owned(), MOD_CH.to_owned()],
            mirror_guild_id: Some(GUILD.to_owned()),
            bot_user_id: BOT.to_owned(),
        }
    }

    async fn ambiguous_entry(pool: &PgPool, entry: &str, mirror: &ScriptMirror) {
        let service = AuditMirrorService::new(AuditStore::new(pool), mirror.clone(), config());
        service.record(&event(entry)).await.expect("record");
        mirror
            .post_script
            .lock()
            .unwrap()
            .push_back(Err(MirrorError::Uncertain("synthetic timeout".to_owned())));
        assert_eq!(
            service.deliver_entry(entry).await.expect("send pass"),
            DeliverOutcome::Ambiguous
        );
    }

    fn deep_history(mirror: &ScriptMirror) {
        let mut history = mirror.history.lock().unwrap();
        assert!(history.is_empty(), "previous scan consumed its pages");
        for page in 0..RECONCILE_MAX_PAGES {
            history.push_back(Ok((0..u64::from(HISTORY_PAGE_LIMIT))
                .map(|offset| MirrorMessage {
                    id: (900_000 - page as u64 * u64::from(HISTORY_PAGE_LIMIT) - offset)
                        .to_string(),
                    author_id: "1234".to_owned(),
                    content: "synthetic noise".to_owned(),
                })
                .collect()));
        }
    }

    #[tokio::test]
    async fn repeated_no_progress_quarantines_across_restart_and_read_error() {
        let Some((pool, schema)) = test_pool("audit_scan_repeat")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping audit_service test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let entry = "scan-repeat";
        let store = AuditStore::new(&pool);
        let mirror = ScriptMirror::default();
        ambiguous_entry(&pool, entry, &mirror).await;
        let service = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config());
        deep_history(&mirror);
        let reads_before = mirror.reads();
        assert_eq!(
            service.deliver_entry(entry).await.expect("first bound"),
            DeliverOutcome::ScanBoundReached
        );
        assert_eq!(mirror.reads() - reads_before, RECONCILE_MAX_PAGES as u64);
        assert_eq!(
            store
                .get(entry)
                .await
                .expect("row")
                .expect("row exists")
                .reconcile_before
                .as_deref(),
            Some("898401")
        );
        let timed = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config())
            .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            timed
                .deliver_entry(entry)
                .await
                .expect("first no-progress bound"),
            DeliverOutcome::ScanBoundReached
        );
        let first = store.get(entry).await.expect("row").expect("row exists");
        assert_eq!(first.state, DeliveryState::Pending);
        assert_eq!(first.search_before.as_deref(), Some("0"));
        drop(service);

        // Neither a new service nor an intervening read failure resets the
        // durable bound-hit evidence. Otherwise deep rows can retry forever.
        let restarted = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config());
        mirror
            .history
            .lock()
            .unwrap()
            .push_back(Err(MirrorError::Uncertain(
                "synthetic read failure".to_owned(),
            )));
        assert_eq!(
            restarted.deliver_entry(entry).await.expect("read error"),
            DeliverOutcome::Ambiguous
        );
        assert_eq!(
            store
                .get(entry)
                .await
                .expect("row")
                .expect("row exists")
                .last_error,
            first.last_error,
            "read errors must not reset bound-hit evidence"
        );
        let reads_before = mirror.reads();
        let outcome = timed
            .deliver_entry(entry)
            .await
            .expect("second no-progress bound");
        assert!(
            matches!(outcome, DeliverOutcome::Quarantined(_)),
            "{outcome:?}"
        );
        assert_eq!(mirror.reads() - reads_before, 0);
        assert_eq!(
            store
                .get(entry)
                .await
                .expect("row")
                .expect("row exists")
                .reconcile_before,
            first.reconcile_before
        );
        let terminal = store.get(entry).await.expect("row").expect("row exists");
        assert_eq!(terminal.state, DeliveryState::Quarantined);
        assert_eq!(
            terminal.last_error.as_deref(),
            Some("discord_reconcile_scan_exhausted")
        );
        assert_eq!(terminal.search_before, first.search_before);
        assert_eq!(terminal.nonce, first.nonce);
        assert_eq!(terminal.attempts, 1, "scan bounds are not POST attempts");
        assert!(terminal.mirror_message_id.is_none());
        assert!(!store
            .pending_ids()
            .await
            .expect("queue")
            .contains(&entry.to_owned()));
        assert!(store.claim(entry).await.expect("claim terminal").is_none());
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1, "never re-POST");
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn first_scan_bound_can_still_reconcile_after_restart() {
        let Some((pool, schema)) = test_pool("audit_scan_recover")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping audit_service test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let entry = "scan-recover";
        let mirror = ScriptMirror::default();
        ambiguous_entry(&pool, entry, &mirror).await;
        let timed = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config())
            .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            timed.deliver_entry(entry).await.expect("first timeout"),
            DeliverOutcome::ScanBoundReached
        );
        drop(timed);
        mirror
            .history
            .lock()
            .unwrap()
            .push_back(Ok(vec![MirrorMessage {
                id: "640".to_owned(),
                author_id: BOT.to_owned(),
                content: format_audit_event(&event(entry)),
            }]));
        let restarted = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config());
        assert_eq!(
            restarted
                .deliver_entry(entry)
                .await
                .expect("recover marker"),
            DeliverOutcome::Reconciled {
                message_id: "640".to_owned()
            }
        );
        let row = AuditStore::new(&pool)
            .get(entry)
            .await
            .expect("row")
            .expect("row exists");
        assert_eq!(row.state, DeliveryState::Delivered);
        assert!(row.last_error.is_none());
        assert_eq!(row.attempts, 1);
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn scan_bound_refuses_stale_unprepared_and_accepted_claims() {
        let Some((pool, schema)) = test_pool("audit_scan_fence")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping audit_service test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let store = AuditStore::new(&pool);
        store
            .record(&event("unprepared"), Some(AUDIT_CH))
            .await
            .expect("record");
        let send = store
            .claim("unprepared")
            .await
            .expect("claim")
            .expect("owner");
        assert_eq!(
            store
                .bound_reconcile_scan(&send, None)
                .await
                .expect("unprepared fence"),
            ReconcileBound::LostClaim
        );
        let unprepared = store
            .get("unprepared")
            .await
            .expect("row")
            .expect("row exists");
        assert_eq!(unprepared.attempts, 0);
        assert!(unprepared.last_error.is_none());

        let entry = "scan-fence";
        let mirror = ScriptMirror::default();
        ambiguous_entry(&pool, entry, &mirror).await;
        let stale = store.claim(entry).await.expect("claim").expect("owner");
        sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second' WHERE entry_id = $1")
            .bind(entry).execute(&pool).await.expect("expire fixture");
        let timed = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config())
            .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            timed.deliver(&stale).await.expect("expired fence"),
            DeliverOutcome::Unclaimed
        );
        let replacement = store
            .claim(entry)
            .await
            .expect("replacement")
            .expect("owner");
        assert_eq!(
            timed.deliver(&stale).await.expect("stale generation"),
            DeliverOutcome::Unclaimed
        );
        let untouched = store.get(entry).await.expect("row").expect("row exists");
        assert_eq!(untouched.state, DeliveryState::Delivering);
        assert_eq!(
            untouched.last_error.as_deref(),
            Some("discord_post_ambiguous")
        );
        assert_eq!(
            timed.deliver(&replacement).await.expect("first bound"),
            DeliverOutcome::ScanBoundReached
        );
        // Reusing a yielded capability cannot spend a second scan attempt.
        assert_eq!(
            timed.deliver(&replacement).await.expect("reused fence"),
            DeliverOutcome::Unclaimed
        );
        let live = store.claim(entry).await.expect("claim").expect("owner");
        assert!(store.note_accepted(&live, "640").await.expect("accept"));
        // The claim snapshot predates acceptance. The locked database row is
        // authoritative: it must not be quarantined or have evidence cleared.
        assert_eq!(
            timed.deliver(&live).await.expect("accepted fence"),
            DeliverOutcome::Unclaimed
        );
        let accepted = store.get(entry).await.expect("row").expect("row exists");
        assert_eq!(accepted.state, DeliveryState::Delivering);
        assert_eq!(accepted.mirror_message_id.as_deref(), Some("640"));
        assert!(accepted.accepted_at.is_some());
        assert!(store.complete(&live).await.expect("complete accepted"));
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn repeated_scan_timeout_quarantines_without_history_reads() {
        let Some((pool, schema)) = test_pool("audit_scan_timeouts")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping audit_service test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let entry = "scan-timeouts";
        let mirror = ScriptMirror::default();
        ambiguous_entry(&pool, entry, &mirror).await;
        let timed = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config())
            .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
        let reads_before = mirror.reads();
        assert_eq!(
            timed.deliver_entry(entry).await.expect("first timeout"),
            DeliverOutcome::ScanBoundReached
        );
        assert_eq!(
            timed.deliver_entry(entry).await.expect("second timeout"),
            DeliverOutcome::Quarantined(QuarantineReason::ScanBoundExceeded)
        );
        assert_eq!(mirror.reads(), reads_before);
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        drop_schema(&pool, &schema).await;
    }

    /// A past scan deadline stops the Reconcile pass with `ScanBoundReached`
    /// before any history read: pins the timeout half of the bound (the page
    /// cap alone would read on and quarantine this empty history as
    /// `MarkerMissing`). The yield is pinned too: the row is pending with its
    /// boundary intact, not left claimed.
    #[tokio::test]
    async fn past_scan_deadline_stops_before_any_read_and_yields() {
        let Some((pool, schema)) = test_pool("audit_scan_deadline")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping audit_service test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let store = AuditStore::new(&pool);
        let mirror = ScriptMirror::default();
        let service = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config());
        // Drive the row to reconcile-eligible through the Send path: empty
        // dedup seeds a boundary, then the POST times out ambiguously.
        service
            .record(&event("scan-deadline"))
            .await
            .expect("record");
        mirror
            .post_script
            .lock()
            .unwrap()
            .push_back(Err(MirrorError::Uncertain(
                "accepted but timed out".to_owned(),
            )));
        assert_eq!(
            service
                .deliver_entry("scan-deadline")
                .await
                .expect("send pass"),
            DeliverOutcome::Ambiguous
        );
        let timed = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config())
            .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
        let reads_before = mirror.reads();
        assert_eq!(
            timed
                .deliver_entry("scan-deadline")
                .await
                .expect("reconcile pass"),
            DeliverOutcome::ScanBoundReached
        );
        assert_eq!(
            mirror.reads() - reads_before,
            0,
            "timeout stops before any read"
        );
        let row = store
            .get("scan-deadline")
            .await
            .expect("row")
            .expect("row exists");
        assert_eq!(row.state, DeliveryState::Pending);
        assert!(row.search_before.is_some(), "yield preserves the boundary");
        drop_schema(&pool, &schema).await;
    }
}
