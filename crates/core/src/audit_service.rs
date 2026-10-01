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

use futures_util::future::BoxFuture;

use crate::audit::{
    delivery_nonce, format_audit_event, route_for_sink, AuditChannelIds, AuditEvent, KillSwitchLog,
    KillSwitchSnapshot, SinkRoute,
};
use crate::audit_mirror::{
    find_mirror_in_page, mirror_channel_policy, next_snowflake, page_floor_id,
    snowflake_at_or_after, AuditMirror, MirrorError, MirrorGate, DEDUP_PAGE_LIMIT,
    HISTORY_PAGE_LIMIT,
};
use crate::audit_store::{
    AuditClaim, AuditStore, AuditStoreError, DeliveryFailure, DeliveryIntent, PrepareSend,
    QuarantineReason,
};

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
        let Some(claim) = self.store.claim(entry_id).await? else {
            return Ok(DeliverOutcome::Unclaimed);
        };
        self.deliver(&claim).await
    }

    /// Drive an already-held claim (Send or Reconcile by its stored intent).
    pub async fn deliver(&self, claim: &AuditClaim) -> Result<DeliverOutcome, AuditStoreError> {
        match claim.intent() {
            DeliveryIntent::Send => self.deliver_send(claim).await,
            DeliveryIntent::Reconcile => self.deliver_reconcile(claim).await,
        }
    }

    /// Claim and deliver every row `pending_ids` surfaces (bounded batch);
    /// per-row store errors are collected, never fatal to the sweep.
    pub async fn drain_pending(&self) -> Result<DrainReport, AuditStoreError> {
        let mut report = DrainReport::default();
        for entry_id in self.store.pending_ids().await? {
            match self.deliver_entry(&entry_id).await {
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
            {
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

        // Unbounded boundary scan (legacy `findMirror` with a cursor): page
        // downward until the marker is proven, the floor drops below the
        // boundary, or history runs out.
        let mut before: Option<String> = None;
        loop {
            let page = match self
                .mirror
                .channel_history(&destination, before.as_deref(), HISTORY_PAGE_LIMIT)
                .await
            {
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
            // Keep the lease alive across an unbounded scan.
            self.store.renew(claim).await?;
            before = Some(floor);
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
