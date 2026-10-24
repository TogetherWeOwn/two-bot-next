//! Ticket orchestration over the existing guild-scoped store and REST executor.
//! No transcript body is logged, audited, or returned in an interaction reply.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex as TaskMutex,
    },
    time::Duration,
};

use serde_json::Value;
use sqlx::PgPool;
use tokio::{
    sync::{watch, Mutex},
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior},
};
use twilight_model::{application::interaction::Interaction, guild::Permissions};
use two_bot_core::{funnel::now_millis_for_test, metrics, settings::LiveSettings, tickets::*};
use two_bot_cutover::tickets::{OpenResult, StoreError, TicketStore};
use two_bot_discord::{
    executor::{ChannelPresence, TicketChannelRequest, TicketMessage},
    ActionExecutor, DiscordError,
};

use crate::interaction_admission::{UserSlot, UserSlots, PER_USER_IN_FLIGHT};
use crate::jobs::{ErrorClass, JobAction};

const RECOVERY_CADENCE: Duration = Duration::from_secs(RECOVERY_INTERVAL_SECONDS);
const PURGE_CADENCE: Duration = Duration::from_secs(PURGE_INTERVAL_SECONDS);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(240);
const PURGE_TIMEOUT: Duration = Duration::from_secs(60);
const BUTTON_TIMEOUT: Duration = Duration::from_secs(120);
const LANE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const RECOVERY_ITEM_TIMEOUT: Duration = Duration::from_secs(20);
const RECOVERY_ROWS_BUDGET: Duration = Duration::from_secs(180);
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(6);
const PANEL_TIMEOUT: Duration = Duration::from_secs(30);

/// Base delay before a failed Open row becomes eligible for retry: one
/// recovery cadence. Immediate recover/Ready bursts at the same clock reading
/// must not replay a just-failed row.
const OPEN_RETRY_BASE_BACKOFF: Duration = Duration::from_secs(RECOVERY_INTERVAL_SECONDS);
/// Upper bound for the doubling backoff: a persistently failing row is
/// re-attempted at least hourly, never more often than the base delay.
const OPEN_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Retained failed Open rows per runtime. Beyond this bound a new failure is
/// attempted once without gaining eligibility; the bound keeps the set finite
/// instead of growing with the ticket table.
const OPEN_RETRY_RETAINED_CAP: usize = 128;
/// Eligible Open retries attempted per recovery pass, after the forward scan
/// and inside the same row budget, so new rows and the panel slot keep their
/// existing priority.
const OPEN_RETRY_PER_PASS_CAP: usize = 64;

/// Doubling backoff for the nth consecutive Open failure (0-based): 300 s,
/// 600 s, 1200 s, 2400 s, then the hourly cap. Every failure stays eligible
/// within twelve five-minute passes.
fn open_retry_backoff(attempts: u32) -> Duration {
    let shift = attempts.min(4);
    Duration::from_secs(
        (OPEN_RETRY_BASE_BACKOFF.as_secs() << shift).min(OPEN_RETRY_MAX_BACKOFF.as_secs()),
    )
}

/// In-memory retry eligibility for one failed Open row. The durable row,
/// re-read on every attempt, remains truth: status, channel and receipt are
/// revalidated before any Discord mutation, and terminal or removed rows shed
/// their entry without I/O. `eligible_at` rides the Tokio clock so paused
/// test time advances eligibility exactly like production time.
struct OpenRetryEntry {
    eligible_at: Instant,
    attempts: u32,
}

/// Fixed-vocabulary cause for the `ticket open retry` diagnostics. Store
/// failures reuse the shared `db_cause` class, Discord failures reuse the
/// executor's `cause()`, and selection/interruption use local tokens; no arm
/// carries remote text, topics, message content or member names.
fn open_retry_reason(error: &Failure) -> &'static str {
    match error {
        Failure::Store(StoreError::Database(error)) => two_bot_core::error_chain::db_cause(error),
        Failure::Store(_) => "store_error",
        Failure::Rest(error) => error.cause(),
        Failure::InvalidEvidence => "invalid_evidence",
        Failure::Timeout => "timeout",
        Failure::LaneBusy => "lane_busy",
        Failure::NoTicket => "no_ticket",
        Failure::TicketClaimedBy(_) => "already_claimed",
        Failure::TicketNotOpen => "not_open",
    }
}

/// Concurrently admitted ticket component executions per runtime. Admission
/// is non-waiting and happens before spawning: beyond this bound (or the
/// per-member cap) dispatch refuses synchronously with one ephemeral busy
/// acknowledgement on the shared busy lane instead of queueing a waiter.
/// Ticket work is at least as heavy as shared interactions, so the bound
/// matches the shared interaction lane rather than exceeding it.
pub(crate) const MAX_TICKET_COMPONENT_IN_FLIGHT: usize = 16;

/// Seconds between saturation `warn!` lines per runtime. The drops counter
/// increments on every refusal; the log samples the first refusal in each
/// window so a burst is O(1) lines.
const COMPONENT_SATURATION_LOG_WINDOW_SECS: u64 = 60;

// Ticket fairness rides the shared per-member budget: one member holds at
// most PER_USER_IN_FLIGHT executions, strictly below the global cap, so a
// single-member burst can never fill the runtime and another member at
// available capacity still makes progress.
const _: () = assert!(
    PER_USER_IN_FLIGHT < MAX_TICKET_COMPONENT_IN_FLIGHT,
    "per-member ticket budget must leave room for other members"
);

#[path = "ticket_history.rs"]
mod history;

async fn bounded<T>(duration: Duration, work: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(duration, work)
        .await
        .unwrap_or(Err(Failure::Timeout))
}

/// The gateway owns this scope. Dropping a cancelled shard also cancels all
/// ticket work; normal shutdown additionally joins it before returning.
pub(crate) struct TicketSupervisor {
    runtime: Arc<TicketRuntime>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl TicketSupervisor {
    pub async fn shutdown(self) {
        self.runtime.stop_tasks();
        self.shutdown.send_replace(true);
        // Await by reference because Drop remains the cancellation fallback.
        let mut this = self;
        let _ = (&mut this.task).await;
        let mut tasks = {
            let mut tasks = this.runtime.tasks.lock().expect("ticket task scope");
            std::mem::take(&mut *tasks)
        };
        tasks.shutdown().await;
    }
}

impl Drop for TicketSupervisor {
    fn drop(&mut self) {
        self.runtime.stop_tasks_from_drop();
        self.shutdown.send_replace(true);
        self.task.abort();
    }
}

/// Fixed-vocabulary token for the `ticket recovery deferred` warn's `action`
/// field. `None` selects no action (nothing was attempted).
fn recovery_action_token(action: &RecoveryAction) -> Option<&'static str> {
    match action {
        RecoveryAction::ReattachOpenControls { .. } => Some("reattach_open_controls"),
        RecoveryAction::FindCreatingChannel { .. } => Some("find_creating_channel"),
        RecoveryAction::DeleteInterruptedCreate { .. } => Some("delete_interrupted_create"),
        RecoveryAction::RestoreOpenerThenReopen { .. } => Some("restore_opener_then_reopen"),
        RecoveryAction::RecoverSavedClose { .. } => Some("recover_saved_close"),
        RecoveryAction::RetryCleanup { .. } => Some("retry_cleanup"),
        RecoveryAction::None => None,
    }
}

/// Emit one `ticket_lifecycle` line and its counter sample for a terminal
/// ticket event. Every retire, abandon, and successful staff-button claim and
/// close executed through the button path reports exactly once with a
/// fixed-vocabulary cause; the purge pass reports separately with its deleted
/// count. Recovery completions and the open-path controls-post rollback reach
/// their terminal states without emitting. See
/// `docs/ticket-lifecycle-observability.md` for the vocabularies.
fn emit_lifecycle(
    event: &'static str,
    outcome: &'static str,
    ticket_id: &str,
    prior_status: &'static str,
    cause: &'static str,
) {
    tracing::info!(
        event = %event,
        ticket_id = %ticket_id,
        prior_status = %prior_status,
        cause = %cause,
        "ticket_lifecycle"
    );
    metrics::global().ticket_lifecycle(event, outcome);
}

/// Storage-failure outcome for the purge pass: the shared `db_cause` class
/// for SQL failures, so the failure mode survives in the counter and the log
/// instead of collapsing to a bare `database`. Non-SQL store errors keep the
/// generic class.
fn purge_outcome(error: &StoreError) -> &'static str {
    match error {
        StoreError::Database(error) => two_bot_core::error_chain::db_cause(error),
        StoreError::Domain(_) | StoreError::InvalidRow(_) | StoreError::NotFound => "database",
    }
}

async fn recovery_batch<F, Fut>(
    cursor: &TaskMutex<Option<(i64, String)>>,
    mut tickets: Vec<Ticket>,
    action: F,
) -> Option<ErrorClass>
where
    F: Fn(Ticket) -> Fut,
    Fut: Future<Output = (Result<()>, Option<&'static str>)>,
{
    if tickets.is_empty() {
        return None;
    }
    // Imported timestamps may have sub-ms precision that decoding discards.
    // Sort by the same decoded key the cursor uses, not the database's full instant.
    tickets
        .sort_unstable_by(|a, b| (a.created_at, a.id.as_str()).cmp(&(b.created_at, b.id.as_str())));
    let after = cursor.lock().expect("ticket recovery cursor").clone();
    if let Some((created_at, id)) = after {
        // Steady state is a skip, not a replay: behind-cursor Open rows are
        // settled and never attempted again. Any other status is an in-flight
        // workflow whose immutable created_at key can sit behind the cursor
        // while the row is still unfinished — a Creating row turns actionable
        // only after the 15-minute stale cutoff, an Open row can move to
        // Closing/CleanupPending after a pass attempted it, and a failed or
        // cancelled attempt leaves the row behind the cursor — so those rows
        // are always re-attempted, keeping the cursor monotonic.
        let behind = tickets
            .iter()
            .position(|ticket| (ticket.created_at, ticket.id.as_str()) > (created_at, id.as_str()))
            .unwrap_or(tickets.len());
        let skipped = tickets[..behind]
            .iter()
            .filter(|ticket| ticket.status == TicketStatus::Open)
            .count();
        if skipped > 0 {
            tracing::info!(skipped, "ticket recovery skipped behind-cursor rows");
        }
        if behind > 0 {
            let mut kept = Vec::with_capacity(tickets.len() - skipped);
            for (index, ticket) in tickets.into_iter().enumerate() {
                if index >= behind || ticket.status != TicketStatus::Open {
                    kept.push(ticket);
                }
            }
            tickets = kept;
        }
    }
    let deadline = Instant::now() + RECOVERY_ROWS_BUDGET;
    let mut failed = None;
    let total = tickets.len();
    for (index, ticket) in tickets.into_iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let dropped = total - index;
            tracing::warn!(dropped, "ticket recovery budget exhausted");
            break;
        }
        // Advance before await: even whole-scope cancellation skips past
        // this row next time. The durable row, not this cursor, remains truth.
        // Never move backwards: a re-attempted behind-cursor row must not
        // rewind past newer rows the cursor already passed.
        {
            let mut guard = cursor.lock().expect("ticket recovery cursor");
            let key = (ticket.created_at, ticket.id.clone());
            if guard.as_ref().is_none_or(|current| key > *current) {
                *guard = Some(key);
            }
        }
        let ticket_id = ticket.id.clone();
        let status = ticket.status.as_str();
        // The timeout arm reports no attempted action: the future never
        // finished selecting one. `None` likewise covers a transcript-probe
        // failure before any action was selected. The timeout wraps the
        // tuple future directly (`bounded` only wraps `Result` futures).
        let (result, attempted) = match tokio::time::timeout(
            remaining.min(RECOVERY_ITEM_TIMEOUT),
            action(ticket),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => (Err(Failure::Timeout), None),
        };
        if let Err(error) = result {
            failed = Some(error.class());
            match attempted {
                Some(action) => {
                    tracing::warn!(error_class = ?error.class(), action = %action, ticket_id = %ticket_id, status = %status, "ticket recovery deferred")
                }
                None => {
                    tracing::warn!(error_class = ?error.class(), ticket_id = %ticket_id, status = %status, "ticket recovery deferred")
                }
            }
        }
    }
    failed
}

async fn maintenance_loop(
    name: &'static str,
    action: JobAction,
    cadence: Duration,
    timeout: Duration,
    mut ready: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval_at(Instant::now() + cadence, cadence);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut completed_at = None;
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() { return; }
                continue;
            }
            changed = ready.changed() => {
                if changed.is_err() { return; }
                ready.borrow_and_update();
            }
            deadline = interval.tick() => {
                // Do not replay ticks that elapsed during a slow action.
                if completed_at.is_some_and(|end| deadline < end) { continue; }
            }
        }
        // Work is inline, not detached, so shutdown/drop cancels its future.
        let result = tokio::select! {
            biased;
            _ = crate::server::shutdown_requested(shutdown.clone()) => return,
            result = tokio::time::timeout(timeout, async { action().await }) => {
                result.unwrap_or(Err(ErrorClass::Timeout))
            }
        };
        completed_at = Some(Instant::now());
        if let Err(class) = result {
            tracing::warn!(job = name, error_class = ?class, "ticket maintenance deferred");
        }
    }
}

/// Destination keys served from the live settings snapshot. The ticket cooldown
/// rides the same snapshot through [`COOLDOWN_KEY`]; it stays out of this list
/// because `TicketConfig::from_env` treats the entries here as
/// required-for-warning while the cooldown is optional (boot default).
pub(crate) const LIVE_KEYS: [&str; 3] = [
    "DISCORD_TICKET_CATEGORY_ID",
    "DISCORD_TICKET_PANEL_CHANNEL_ID",
    "DISCORD_TICKET_STAFF_ROLE_ID",
];

/// The ticket cooldown key served from the live settings snapshot alongside
/// the destinations.
pub(crate) const COOLDOWN_KEY: &str = "TWO_TICKET_COOLDOWN_SECONDS";

/// Layer the live snapshot over the boot deployment (store-first, like the
/// raid/join-risk/containment runtimes): stored rows win, deleted rows fall
/// back to the boot value. Only the three destinations and the cooldown are
/// read.
pub(crate) fn layered_vars(
    deployment: &HashMap<String, String>,
    guild_id: &str,
    live: Option<&LiveSettings>,
) -> HashMap<String, String> {
    let mut vars = deployment.clone();
    if let Some(live) = live {
        vars.extend(
            live.env_snapshot(Some(guild_id))
                .into_iter()
                .filter(|(key, _)| LIVE_KEYS.contains(&key.as_str()) || key == COOLDOWN_KEY),
        );
    }
    vars
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TicketConfig {
    pub guild_id: String,
    pub category_id: String,
    pub panel_channel_id: String,
    pub staff_role_id: String,
    pub cooldown_seconds: u64,
}

impl TicketConfig {
    pub fn from_env(guild_id: u64) -> Option<Self> {
        let vars: HashMap<String, String> = std::env::vars().collect();
        let cooldown_seconds = match std::env::var("TWO_TICKET_COOLDOWN_SECONDS") {
            Ok(value) => match value.parse() {
                Ok(value) => value,
                Err(_) => {
                    tracing::warn!("invalid ticket cooldown; tickets disabled");
                    return None;
                }
            },
            Err(std::env::VarError::NotPresent) => COOLDOWN_SECONDS,
            Err(_) => return None,
        };
        let config = Self::from_vars(&vars, guild_id, cooldown_seconds);
        if config.is_none() && LIVE_KEYS.iter().all(|key| vars.contains_key(*key)) {
            tracing::warn!("invalid ticket configuration; tickets disabled");
        }
        config
    }

    /// Pure constructor over layered vars: destinations resolve, the staff
    /// role must not be the guild id, and category and panel must differ. A
    /// malformed stored row returns `None` so the caller keeps the last good
    /// config rather than disabling tickets. Silent by design: callers log.
    pub(crate) fn from_vars(
        vars: &HashMap<String, String>,
        guild_id: u64,
        cooldown_seconds: u64,
    ) -> Option<Self> {
        let setting = |key: &str| vars.get(key).cloned().filter(|v| valid_id(v).is_ok());
        let category_id = setting("DISCORD_TICKET_CATEGORY_ID")?;
        let panel_channel_id = setting("DISCORD_TICKET_PANEL_CHANNEL_ID")?;
        let staff_role_id = setting("DISCORD_TICKET_STAFF_ROLE_ID")?;
        if staff_role_id == guild_id.to_string() || category_id == panel_channel_id {
            return None;
        }
        Some(Self {
            guild_id: guild_id.to_string(),
            category_id,
            panel_channel_id,
            staff_role_id,
            cooldown_seconds,
        })
    }
}

pub(crate) struct TicketRuntime {
    store: TicketStore,
    executor: ActionExecutor,
    /// Current destinations and cooldown (boot, then live). Both move on
    /// refresh when the snapshot carries them.
    config: TaskMutex<TicketConfig>,
    /// Boot deployment values for the three destinations and the cooldown; the
    /// live snapshot layers over these on every refresh.
    deployment: HashMap<String, String>,
    /// Last effective input, including ops. Suppress repeated parsing/logging
    /// without conflating the guild revision with an effective publication.
    live_vars: TaskMutex<Option<HashMap<String, String>>>,
    bot_id: AtomicU64,
    // Ready, buttons and recovery share this lane. Purge has its own lane so
    // slow Discord I/O cannot postpone the privacy ceiling.
    lane: Mutex<()>,
    purge_lane: Mutex<()>,
    recovery_cursor: TaskMutex<Option<(i64, String)>>,
    /// Failed Open rows awaiting a later eligible pass. Forward progress
    /// never rewinds for these: the cursor keeps advancing past them, and a
    /// bounded retry phase re-attempts the due ones after each forward scan.
    /// Inserted before the row's work is awaited so scope cancellation and
    /// per-item timeouts retain eligibility; removed on success and on
    /// terminal or removed rows. Like the cursor, this set is in-memory: a
    /// restart re-attempts every row through the normal forward scan.
    open_retries: TaskMutex<BTreeMap<String, OpenRetryEntry>>,
    ready: watch::Sender<u64>,
    started: AtomicBool,
    stopping: AtomicBool,
    tasks: TaskMutex<JoinSet<()>>,
    /// Ticket component executions admitted per member; see
    /// [`crate::interaction_admission`]. Held inside the spawned handler
    /// until it settles, so the slot releases on success, failure,
    /// cancellation and shutdown.
    component_slots: UserSlots,
    /// Admitted ticket component executions currently in flight: the global
    /// half of admission, counted separately from `tasks` so generic ticket
    /// work sharing the scope (e.g. RESUMED readiness) never consumes
    /// component budget. Each permit holds a clone and releases on drop, so
    /// the count is exact without waiting for the next prune.
    component_in_flight: Arc<AtomicUsize>,
    /// Last sampled saturation `warn!`; the drops counter increments
    /// un-sampled on every refusal.
    last_component_drop_log: TaskMutex<Option<std::time::Instant>>,
}

pub(crate) enum Failure {
    Store(StoreError),
    Rest(DiscordError),
    InvalidEvidence,
    Timeout,
    LaneBusy,
    NoTicket,
    TicketClaimedBy(String),
    TicketNotOpen,
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<DiscordError> for Failure {
    fn from(error: DiscordError) -> Self {
        Self::Rest(error)
    }
}
impl Failure {
    pub fn reply(&self) -> String {
        match self {
            Self::Store(StoreError::Domain(error)) => error.to_string(),
            // Lane admission times out before any store reservation exists, so
            // no unfinished work was left for recovery to retry. Stay a
            // truthful retry-later refusal without claiming a recovery job.
            // This is deliberately distinct from `Timeout`: the cumulative
            // deadline wrapping the whole lane-held operation can expire after
            // `reserve`/`begin_close` stored a state that recovery resumes.
            Self::LaneBusy => {
                "Ticket action timed out waiting for another operation. Try again later.".into()
            }
            // Claim/close in a channel with no tracked ticket ends before any
            // operation: name the refusal specifically without promising
            // recovery or exposing storage details.
            Self::NoTicket => "This channel is not a ticket.".into(),
            // A lost claim race names the durable winner. The id is
            // Discord-owned, never a user-controlled name, and the shared
            // router suppresses the mention, so this cannot ping.
            Self::TicketClaimedBy(winner) => {
                format!("This ticket is already claimed by <@{winner}>.")
            }
            // Claiming a closing or otherwise non-open ticket is a lifecycle
            // refusal, never an ownership conflict or a recovery promise.
            Self::TicketNotOpen => "This ticket is not open.".into(),
            _ => {
                "Ticket action failed; recovery will retry unfinished work. Try again later.".into()
            }
        }
    }
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Store(_) => ErrorClass::Database,
            Self::Timeout | Self::LaneBusy | Self::Rest(DiscordError::Timeout) => {
                ErrorClass::Timeout
            }
            Self::Rest(_)
            | Self::InvalidEvidence
            | Self::NoTicket
            | Self::TicketClaimedBy(_)
            | Self::TicketNotOpen => ErrorClass::Rest,
        }
    }
    /// True only for SQL storage failures (pool, connection or query), never
    /// for domain refusals. Warn enrichment skips its decorating store
    /// lookup then, instead of spending a second pool acquisition on a sick
    /// store to decorate a failure line.
    pub fn is_storage_failure(&self) -> bool {
        matches!(self, Self::Store(StoreError::Database(_)))
    }
}

type Result<T> = std::result::Result<T, Failure>;

/// Maximum history data pages fetched per capture (100 messages each):
/// fetching stops at the cap even when the channel runs deeper, so a huge
/// channel cannot page Discord without bound. A full 10th page is followed
/// by one boundary probe (11 fetches at most): an empty probe proves the
/// history is exactly 1000 and completes without the marker, anything else
/// proves depth and completes with the shared truncation marker. The
/// capture still validates every stored page; histories under the cap are
/// byte-identical, and `message_count` covers stored messages only.
const MAX_HISTORY_PAGES: usize = 10;

/// An admitted ticket component execution. Both slots release on drop —
/// including when the handler is aborted — so admission is held exactly until
/// the handler settles.
pub(crate) struct TicketComponentPermit {
    _member: Option<UserSlot>,
    _components: Arc<AtomicUsize>,
}

impl Drop for TicketComponentPermit {
    fn drop(&mut self) {
        self._components.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Reap finished tasks; an abnormal end keeps its durable state for recovery.
fn prune_finished(tasks: &mut JoinSet<()>) {
    while let Some(result) = tasks.try_join_next() {
        if result.is_err() {
            tracing::warn!("ticket task stopped; durable state retained");
        }
    }
}

/// Back one retry entry off after a failed attempt: eligibility moves out by
/// the current step, then the step advances. Saturates instead of wrapping.
fn backoff_entry(entry: &mut OpenRetryEntry) {
    entry.eligible_at = Instant::now() + open_retry_backoff(entry.attempts);
    entry.attempts = entry.attempts.saturating_add(1);
}

/// Cancellation-safe retry bookkeeping for one Open row attempt. The entry is
/// inserted on creation (before any await), so a scope cancellation or a
/// per-item timeout that drops the future keeps the row eligible; `settle`
/// records the explicit outcome, and `Drop` retains only when the attempt
/// never settled. Non-Open rows never enter: their behind-cursor workflows
/// already re-attempt through the forward scan.
struct OpenRetryGuard<'a> {
    runtime: &'a TicketRuntime,
    ticket_id: String,
    preexisting: bool,
    settled: bool,
}

impl<'a> OpenRetryGuard<'a> {
    fn enter(runtime: &'a TicketRuntime, ticket: &Ticket) -> Option<Self> {
        if ticket.status != TicketStatus::Open {
            return None;
        }
        let mut retries = runtime.open_retries.lock().expect("ticket open retry set");
        let preexisting = retries.contains_key(&ticket.id);
        if !preexisting {
            if retries.len() >= OPEN_RETRY_RETAINED_CAP {
                // At capacity: the attempt proceeds once without eligibility.
                return None;
            }
            retries.insert(
                ticket.id.clone(),
                OpenRetryEntry {
                    eligible_at: Instant::now() + open_retry_backoff(0),
                    attempts: 0,
                },
            );
        }
        Some(Self {
            runtime,
            ticket_id: ticket.id.clone(),
            preexisting,
            settled: false,
        })
    }

    /// Record the attempt's explicit outcome: success removes the entry (a
    /// repaired retained row reports once; a first-pass success stays
    /// silent), failure backs the entry off with its reason code.
    fn settle(mut self, result: &Result<()>) {
        self.settled = true;
        let mut retries = self
            .runtime
            .open_retries
            .lock()
            .expect("ticket open retry set");
        match result {
            Ok(()) => {
                retries.remove(&self.ticket_id);
                if self.preexisting {
                    tracing::info!(
                        ticket_id = %self.ticket_id,
                        retryable = false,
                        reason = %"retry_succeeded",
                        outcome = %"repaired",
                        next_action = %"none",
                        "ticket open retry repaired"
                    );
                }
            }
            Err(error) => {
                let reason = open_retry_reason(error);
                if let Some(entry) = retries.get_mut(&self.ticket_id) {
                    backoff_entry(entry);
                }
                tracing::warn!(
                    ticket_id = %self.ticket_id,
                    retryable = true,
                    reason = %reason,
                    outcome = %"deferred",
                    next_action = %"await_retry",
                    "ticket open retry deferred"
                );
            }
        }
    }
}

impl Drop for OpenRetryGuard<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // The attempt was cancelled or timed out mid-flight: retain
        // eligibility with the same backoff step an explicit failure would
        // take. Never panic here: a poisoned set keeps the speculative entry
        // that `enter` inserted, which is itself a valid deferral.
        if let Ok(mut retries) = self.runtime.open_retries.lock() {
            if let Some(entry) = retries.get_mut(&self.ticket_id) {
                backoff_entry(entry);
            }
        }
        tracing::warn!(
            ticket_id = %self.ticket_id,
            retryable = true,
            reason = %"interrupted",
            outcome = %"deferred",
            next_action = %"await_retry",
            "ticket open retry deferred"
        );
    }
}

impl TicketRuntime {
    pub fn new(
        pool: PgPool,
        executor: ActionExecutor,
        config: TicketConfig,
        deployment: HashMap<String, String>,
    ) -> std::result::Result<Self, StoreError> {
        // The layered refresh needs a complete boot fallback, including
        // destinations already resolved in config but omitted from deployment.
        // Preserve explicit deployment destinations and the effective boot
        // cooldown so ops-only updates and deleted rows can rebuild config.
        // Keep this normalization separate from later live snapshot layering.
        let deployment = boot_deployment(&config, deployment);
        // Live settings are layered over these boot defaults at each refresh.
        Ok(Self {
            store: TicketStore::new(pool, config.guild_id.clone())?,
            executor,
            config: TaskMutex::new(config),
            live_vars: TaskMutex::new(None),
            deployment,
            bot_id: AtomicU64::new(0),
            lane: Mutex::new(()),
            purge_lane: Mutex::new(()),
            recovery_cursor: TaskMutex::new(None),
            open_retries: TaskMutex::new(BTreeMap::new()),
            ready: watch::channel(0).0,
            started: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            tasks: TaskMutex::new(JoinSet::new()),
            component_slots: UserSlots::default(),
            component_in_flight: Arc::new(AtomicUsize::new(0)),
            last_component_drop_log: TaskMutex::new(None),
        })
    }

    pub fn start(self: &Arc<Self>) -> Option<TicketSupervisor> {
        if self.started.swap(true, Ordering::AcqRel) {
            return None;
        }
        let (shutdown, receiver) = watch::channel(false);
        let recovery = Arc::clone(self);
        let purge = Arc::clone(self);
        let recovery_ready = self.ready.subscribe();
        let purge_ready = self.ready.subscribe();
        let task = tokio::spawn(async move {
            tokio::join!(
                maintenance_loop(
                    "ticket_recovery",
                    Arc::new(move || {
                        let runtime = Arc::clone(&recovery);
                        Box::pin(async move { runtime.recover().await })
                    }),
                    RECOVERY_CADENCE,
                    RECOVERY_TIMEOUT,
                    recovery_ready,
                    receiver.clone(),
                ),
                maintenance_loop(
                    "ticket_purge",
                    Arc::new(move || {
                        let runtime = Arc::clone(&purge);
                        Box::pin(async move { runtime.purge().await })
                    }),
                    PURGE_CADENCE,
                    PURGE_TIMEOUT,
                    purge_ready,
                    receiver,
                ),
            );
        });
        Some(TicketSupervisor {
            runtime: Arc::clone(self),
            shutdown,
            task,
        })
    }

    pub fn on_ready(&self, bot_id: u64) {
        self.set_bot_id(bot_id);
        self.ready
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().expect("ticket task scope");
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        prune_finished(&mut tasks);
        tasks.spawn(task);
    }

    /// Admit one ticket component execution and spawn its handler, or refuse
    /// synchronously without spawning when the runtime holds
    /// [`MAX_TICKET_COMPONENT_IN_FLIGHT`] executions or the member holds the
    /// per-member cap. The bound check, the member-slot claim and the spawn
    /// are atomic under the task scope, so concurrent dispatches cannot
    /// over-admit; refused work queues no waiter. The permit moves into the
    /// spawned handler and releases when it settles — on success, failure,
    /// cancellation and shutdown — while the global slot rides the permit's
    /// counter, so generic ticket tasks sharing the scope never consume
    /// component budget.
    pub fn try_spawn_component<Fut>(
        &self,
        user: Option<u64>,
        spawn: impl FnOnce(TicketComponentPermit) -> Fut,
    ) -> bool
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut tasks = self.tasks.lock().expect("ticket task scope");
        if self.stopping.load(Ordering::Acquire) {
            return false;
        }
        prune_finished(&mut tasks);
        if self.component_in_flight.load(Ordering::Acquire) >= MAX_TICKET_COMPONENT_IN_FLIGHT {
            return false;
        }
        let member = match user {
            Some(user) => match self.component_slots.acquire(user) {
                Some(slot) => Some(slot),
                None => return false,
            },
            None => None,
        };
        self.component_in_flight.fetch_add(1, Ordering::AcqRel);
        let permit = TicketComponentPermit {
            _member: member,
            _components: Arc::clone(&self.component_in_flight),
        };
        tasks.spawn(spawn(permit));
        true
    }

    /// Record one refused ticket component: the label-free saturation counter
    /// increments on every refusal and the log samples the first refusal per
    /// 60 s per runtime, so a burst is O(1) lines with N counter increments.
    /// Carries the interaction id only — no member or token labels.
    pub fn record_component_saturation(&self, interaction_id: u64) {
        two_bot_core::metrics::global().ticket_component_drop();
        let mut last = self
            .last_component_drop_log
            .lock()
            .expect("ticket saturation log");
        let now = std::time::Instant::now();
        let due = last.is_none_or(|at| {
            now.duration_since(at).as_secs() >= COMPONENT_SATURATION_LOG_WINDOW_SECS
        });
        if due {
            *last = Some(now);
            tracing::warn!(
                interaction_id = interaction_id.to_string(),
                "ticket component admission saturated (ticket_component_admission_saturated; sampled; see two_bot_ticket_component_drops_total)"
            );
        }
    }

    fn stop_tasks(&self) {
        // Live path (async `shutdown`): fail loud on a poisoned scope. The
        // lock acquisition panics before any mutation, so a poisoned live
        // call fences nothing, aborts nothing and counts nothing. `Drop`
        // must never call this; it calls `stop_tasks_from_drop` instead.
        let mut tasks = self.tasks.lock().expect("ticket task scope");
        self.stopping.store(true, Ordering::Release);
        tasks.abort_all();
    }

    /// Drop-only `stop_tasks`: panicking inside `drop` aborts the process
    /// when the drop runs during unwinding, so a poisoned scope is recovered
    /// with one counted `ticket_supervisor` increment instead. Called only
    /// from `TicketSupervisor::drop`; live callers use loud `stop_tasks`.
    fn stop_tasks_from_drop(&self) {
        let mut tasks =
            two_bot_core::metrics::recover_drop_poison(self.tasks.lock(), "ticket_supervisor");
        self.stopping.store(true, Ordering::Release);
        tasks.abort_all();
    }

    pub fn set_bot_id(&self, id: u64) {
        self.bot_id.store(id, Ordering::Release);
    }

    /// Re-resolve the effective destinations and cooldown, not just the DB
    /// revision: ops publications, clearing and expiry do not move that revision.
    /// Serialize the read/compare/apply so concurrent refreshes cannot overwrite
    /// a newer config with an earlier read. Malformed values keep the last good.
    pub(crate) fn refresh_config_with(&self, live: Option<&LiveSettings>) {
        let mut current = self.config.lock().expect("ticket config");
        let Ok(guild_id) = current.guild_id.parse::<u64>() else {
            return;
        };
        let vars = layered_vars(&self.deployment, &current.guild_id, live);
        let mut previous = self.live_vars.lock().expect("ticket effective vars");
        if previous.as_ref() == Some(&vars) {
            return;
        }
        *previous = Some(vars.clone());
        let cooldown_seconds = vars
            .get(COOLDOWN_KEY)
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(current.cooldown_seconds);
        match TicketConfig::from_vars(&vars, guild_id, cooldown_seconds) {
            Some(next) if next != *current => {
                tracing::info!("setting_changed: ticket config");
                *current = next;
            }
            Some(_) => {}
            None => {
                tracing::warn!("ticket destinations unusable; keeping the last good");
            }
        }
    }

    /// Config for this operation: refresh from the live snapshot, then clone
    /// the current config. Effective values include the independently
    /// published ops layer as well as guild rows.
    fn live_config(&self) -> TicketConfig {
        self.refresh_config_with(crate::settings_jobs::live().as_ref());
        self.config.lock().expect("ticket config").clone()
    }

    /// Destinations for tests without a poller.
    #[cfg(test)]
    pub(crate) fn config_for_test(&self) -> TicketConfig {
        self.config.lock().expect("ticket config").clone()
    }

    #[cfg(test)]
    pub(crate) fn readiness_for_test(&self) -> (u64, u64) {
        (self.bot_id.load(Ordering::Acquire), *self.ready.borrow())
    }

    fn bot_id(&self) -> Result<String> {
        let id = self.bot_id.load(Ordering::Acquire);
        if id == 0 {
            return Err(Failure::InvalidEvidence);
        }
        Ok(id.to_string())
    }

    pub fn authorize(
        &self,
        interaction: &Interaction,
        action: TicketAction,
    ) -> std::result::Result<(), TicketError> {
        let config = self.live_config();
        let guild = interaction.guild_id.map(|id| id.get().to_string());
        let roles: Vec<String> = interaction
            .member
            .as_ref()
            .map(|member| {
                member
                    .roles
                    .iter()
                    .map(|role| role.get().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let permissions = interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .map_or(0, |permissions| permissions.bits());
        authorize(
            guild.as_deref(),
            &config.guild_id,
            action,
            &roles,
            permissions,
            &config.staff_role_id,
        )
    }

    /// Resolve the ticket context for the `ticket action incomplete` warn:
    /// the channel-bound ticket for claim/close, if it still resolves. Open
    /// mints its reservation inside `execute`, so a failed open has no
    /// stable ticket to cite and resolves to `None`, as does a failure in a
    /// channel with no tracked ticket.
    pub(crate) async fn warn_ticket_context(
        &self,
        interaction: &Interaction,
        action: TicketAction,
    ) -> Option<(String, &'static str)> {
        match action {
            TicketAction::Open => None,
            TicketAction::Claim | TicketAction::Close => {
                let channel = interaction
                    .channel
                    .as_ref()
                    .map(|channel| channel.id.get().to_string())?;
                let ticket = self.store.by_channel(&channel).await.ok()??;
                Some((ticket.id, ticket.status.as_str()))
            }
        }
    }

    /// Called only after the shared router and successful ephemeral defer.
    pub async fn execute(&self, interaction: &Interaction, action: TicketAction) -> Result<String> {
        self.authorize(interaction, action)
            .map_err(StoreError::Domain)?;
        // Includes admission: an already-deferred interaction must not spend
        // its token lifetime queued behind somebody else's history capture.
        bounded(BUTTON_TIMEOUT, self.execute_in_lane(interaction, action)).await
    }

    async fn execute_in_lane(
        &self,
        interaction: &Interaction,
        action: TicketAction,
    ) -> Result<String> {
        let _guard = tokio::time::timeout(LANE_WAIT_TIMEOUT, self.lane.lock())
            .await
            .map_err(|_| Failure::LaneBusy)?;
        let user = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .ok_or(Failure::InvalidEvidence)?;
        let actor = user.id.get().to_string();
        match action {
            TicketAction::Open => self.open(&actor, &user.name).await,
            TicketAction::Claim | TicketAction::Close => {
                let channel = interaction
                    .channel
                    .as_ref()
                    .map(|channel| channel.id.get().to_string())
                    .ok_or(Failure::InvalidEvidence)?;
                let ticket = self
                    .store
                    .by_channel(&channel)
                    .await?
                    .ok_or(Failure::NoTicket)?;
                match action {
                    TicketAction::Claim => match self.store.claim(&ticket.id, &actor).await {
                        Ok(_) => {
                            let prior = ticket.status.as_str();
                            self.post_claim_marker(&channel, &actor).await;
                            emit_lifecycle("claim", "claimed", &ticket.id, prior, "staff_claim");
                            Ok("Ticket claimed.".into())
                        }
                        Err(StoreError::Domain(TicketError::AlreadyClaimed)) => {
                            Err(self.claim_refusal(&ticket.id).await)
                        }
                        Err(error) => Err(error.into()),
                    },
                    TicketAction::Close => {
                        let prior = ticket.status.as_str();
                        let truncated = self.close(&ticket).await?;
                        emit_lifecycle("close", "closed", &ticket.id, prior, "staff_close");
                        Ok(if truncated {
                            "Ticket closed; transcript was truncated, saved for up to 90 days."
                                .into()
                        } else {
                            "Ticket closed; transcript saved for up to 90 days.".into()
                        })
                    }
                    TicketAction::Open => unreachable!(),
                }
            }
        }
    }

    async fn open(&self, opener: &str, username: &str) -> Result<String> {
        let config = self.live_config();
        let bot_id = self.bot_id()?;
        let category = self
            .executor
            .fetch_ticket_channel(&config.category_id)
            .await?;
        match category {
            ChannelPresence::Present(doc) => self.validate_channel(&doc, &config.category_id, 4)?,
            ChannelPresence::Absent => return Err(Failure::InvalidEvidence),
        }
        let id = hex::encode(rand::random::<[u8; 16]>());
        let ticket = match self
            .store
            .reserve(&id, opener, now_millis_for_test(), config.cooldown_seconds)
            .await?
        {
            OpenResult::Created(ticket) => ticket,
            OpenResult::Refused(OpenDecision::Existing { channel_id }) => {
                return Ok(match channel_id {
                    Some(channel) => format!("You already have a ticket: <#{channel}>."),
                    None => "Your ticket is still being created or recovered.".into(),
                })
            }
            OpenResult::Refused(_) => {
                return Ok("Please wait before opening another ticket.".into())
            }
        };
        let channel = match self
            .executor
            .create_ticket_channel(&TicketChannelRequest {
                guild_id: &config.guild_id,
                category_id: &config.category_id,
                staff_role_id: &config.staff_role_id,
                bot_id: &bot_id,
                opener_id: opener,
                username,
                reservation_id: &ticket.id,
            })
            .await
        {
            Ok(channel) => channel,
            Err(error) => {
                // A single-attempt rejection proves no channel was created;
                // timeout/transport/malformed successes keep the reservation.
                if error.is_safe_pre_mutation() {
                    let prior = ticket.status.as_str();
                    if self.store.abandon_creating(&ticket.id).await? {
                        emit_lifecycle(
                            "abandon",
                            "abandoned",
                            &ticket.id,
                            prior,
                            "create_rejected",
                        );
                    } else {
                        emit_lifecycle(
                            "abandon",
                            "already_gone",
                            &ticket.id,
                            prior,
                            "already_gone",
                        );
                    }
                }
                return Err(error.into());
            }
        };
        // If this write fails, leave creating: recovery finds the exact topic.
        self.store.record_channel(&ticket.id, &channel).await?;
        self.store.activate(&ticket.id, &channel).await?;
        if let Err(error) = self
            .executor
            .post_ticket_message(&channel, TicketMessage::Controls { opener_id: opener })
            .await
        {
            self.store
                .queue_open_rollback(&ticket.id, now_millis_for_test())
                .await?;
            self.cleanup(&ticket.id, &channel).await?;
            return Err(error.into());
        }
        Ok(format!("Your ticket is ready: <#{channel}>."))
    }

    /// Best-effort visibility for a committed claim: the store row is truth,
    /// so a rejected post is counted once and never un-claims or retries.
    /// The remote response body is never logged.
    async fn post_claim_marker(&self, channel: &str, staff_id: &str) {
        if self
            .executor
            .post_ticket_message(channel, TicketMessage::Claimed { staff_id })
            .await
            .is_err()
        {
            let failed = 1;
            tracing::warn!(failed, "ticket claim marker failed");
        }
    }

    /// A lost claim names its reason from a fresh read: lifecycle state wins
    /// over ownership, and an unknown row keeps the domain refusal.
    async fn claim_refusal(&self, ticket_id: &str) -> Failure {
        match self.store.get(ticket_id).await {
            Ok(Some(current)) if current.status != TicketStatus::Open => Failure::TicketNotOpen,
            Ok(Some(current)) => match current.claimed_by {
                Some(winner) => Failure::TicketClaimedBy(winner),
                None => Failure::Store(StoreError::Domain(TicketError::AlreadyClaimed)),
            },
            _ => Failure::Store(StoreError::Domain(TicketError::AlreadyClaimed)),
        }
    }

    /// Runs the full close workflow. Returns whether the saved transcript was
    /// truncated, so the member-visible reply can say so.
    async fn close(&self, ticket: &Ticket) -> Result<bool> {
        // State first: a repeat Close on a non-Open ticket (or a ticket with
        // no channel) must fail with the state error before any Discord I/O,
        // preserving the pre-gate behavior (no GET, InvalidTransition copy).
        {
            let mut probe = ticket.clone();
            probe
                .begin_close(now_millis_for_test())
                .map_err(StoreError::Domain)
                .map_err(Failure::Store)?;
        }
        // Resolve identity first so a cold-RESUMED bot id (0) propagates as
        // InvalidEvidence (retry copy), not as a permission-loss refusal.
        self.bot_id()?;
        // Gate before any mutation: a hand-edited channel missing the bot's
        // explicit overwrite refuses with the ticket still Open and the
        // opener's writes untouched, instead of wedging in Closing until
        // recovery reopens. Discord silently returns [] without
        // READ_MESSAGE_HISTORY, so require the bot's explicit ticket
        // overwrite before treating [] as complete. The opener's overwrite is
        // validated next to the gate for the same reason: set_writes reads it
        // after begin_close, and recovery restores through the same lookup,
        // so a missing opener overwrite would wedge in Closing with no
        // self-heal.
        let channel = ticket
            .channel_id
            .as_deref()
            .ok_or(Failure::InvalidEvidence)?;
        let doc = self.channel_document(channel).await?;
        self.require_history_access(&doc)
            .map_err(|_| Failure::Store(StoreError::Domain(TicketError::MissingHistoryAccess)))?;
        member_overwrite(&doc, &ticket.opener_id)
            .map_err(|_| Failure::Store(StoreError::Domain(TicketError::MissingOpenerAccess)))?;
        let ticket = self
            .store
            .begin_close(&ticket.id, now_millis_for_test())
            .await?;
        let started_at = ticket.closing_started_at.ok_or(Failure::InvalidEvidence)?;
        let channel = ticket
            .channel_id
            .as_deref()
            .ok_or(Failure::InvalidEvidence)?;
        self.set_writes(&doc, channel, &ticket.opener_id, false)
            .await?;
        let (snapshot, truncated) = self.capture_history(channel).await?;
        self.store
            .save_transcript(&ticket.id, started_at, now_millis_for_test(), snapshot)
            .await?;
        // The store call above returns only after INSERT + cleanup state COMMIT.
        self.cleanup(&ticket.id, channel).await?;
        Ok(truncated)
    }

    async fn channel_document(&self, channel: &str) -> Result<Value> {
        match self.executor.fetch_ticket_channel(channel).await? {
            ChannelPresence::Present(doc) => {
                self.validate_channel(&doc, channel, 0)?;
                Ok(doc)
            }
            ChannelPresence::Absent => Err(Failure::InvalidEvidence),
        }
    }

    fn validate_channel(&self, doc: &Value, id: &str, kind: u64) -> Result<()> {
        let config = self.live_config();
        if doc["guild_id"].as_str() != Some(&config.guild_id)
            || doc["id"].as_str() != Some(id)
            || doc["type"].as_u64() != Some(kind)
        {
            return Err(Failure::InvalidEvidence);
        }
        Ok(())
    }

    fn verify_creation_channel(
        &self,
        doc: &Value,
        ticket: &Ticket,
        channel: &str,
        config: &TicketConfig,
    ) -> Result<()> {
        if ticket.guild_id != config.guild_id
            || ticket.channel_id.as_deref().is_some_and(|id| id != channel)
        {
            return Err(Failure::InvalidEvidence);
        }
        self.validate_channel(doc, channel, 0)?;
        TicketChannelRequest {
            guild_id: &config.guild_id,
            category_id: &config.category_id,
            staff_role_id: &config.staff_role_id,
            bot_id: &self.bot_id()?,
            opener_id: &ticket.opener_id,
            username: "", // Verification never constructs a channel name.
            reservation_id: &ticket.id,
        }
        .verify_receipt(doc)?;
        Ok(())
    }

    /// A listing or recorded id only selects a candidate. Fetch and prove it
    /// before recording it or advancing rollback state; never launder listing
    /// evidence into authority to delete a different/private-unproven channel.
    async fn rollback_creating(
        &self,
        ticket: &Ticket,
        channel: &str,
        config: &TicketConfig,
    ) -> Result<()> {
        let present = match self.executor.fetch_ticket_channel(channel).await? {
            ChannelPresence::Present(doc) => {
                self.verify_creation_channel(&doc, ticket, channel, config)?;
                true
            }
            ChannelPresence::Absent => false,
        };
        if !present && ticket.channel_id.is_none() {
            self.store.abandon_creating(&ticket.id).await?;
            return Ok(());
        }
        self.store.record_channel(&ticket.id, channel).await?;
        self.store
            .queue_open_rollback(&ticket.id, now_millis_for_test())
            .await?;
        if present {
            // Reuse the just-verified fetch: one listing + one fetch + one DELETE.
            self.executor.delete_ticket_channel(channel).await?;
        }
        self.store
            .finish_cleanup(&ticket.id, now_millis_for_test())
            .await?;
        Ok(())
    }

    fn require_history_access(&self, doc: &Value) -> Result<()> {
        let (allow, deny) = member_overwrite(doc, &self.bot_id()?)?;
        let required = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
        if allow & required != required || deny & required != 0 {
            return Err(Failure::InvalidEvidence);
        }
        Ok(())
    }

    async fn set_writes(
        &self,
        doc: &Value,
        channel: &str,
        opener: &str,
        enabled: bool,
    ) -> Result<()> {
        let (allow, deny) = member_overwrite(doc, opener)?;
        self.executor
            .set_ticket_opener_writes(channel, opener, allow, deny, enabled)
            .await?;
        Ok(())
    }

    pub(crate) async fn capture_history(
        &self,
        channel: &str,
    ) -> Result<(TranscriptSnapshot, bool)> {
        let mut before: Option<String> = None;
        let mut capture = history::HistoryCapture::default();
        let mut pages = 0;
        loop {
            let page = self
                .executor
                .fetch_channel_messages(channel, before.as_deref(), 100)
                .await?;
            pages += 1;
            if page.len() > 100 {
                return Err(Failure::InvalidEvidence);
            }
            let count = page.len();
            // Retain at most one page of IDs. Strictly decreasing cursors and
            // IDs prove global uniqueness without a history-sized seen set.
            let mut page = page
                .into_iter()
                .map(|message| {
                    let id = valid_id(message["id"].as_str().ok_or(Failure::InvalidEvidence)?)?;
                    Ok((id, message))
                })
                .collect::<Result<Vec<_>>>()?;
            page.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
            let mut floor = before.as_deref().map(valid_id).transpose()?;
            for (id, message) in page {
                if floor.is_some_and(|cursor| id >= cursor) {
                    return Err(Failure::InvalidEvidence);
                }
                floor = Some(id);
                capture
                    .push(id, &message)
                    .map_err(|_| Failure::InvalidEvidence)?;
            }
            if count < 100 {
                break;
            }
            if pages >= MAX_HISTORY_PAGES {
                // A full 10th page cannot prove the end (see
                // exact_full_page_requires_a_final_empty_page): fetch one
                // boundary probe past its floor. Empty proves exactly 1000
                // (complete, no marker); anything else proves depth, so cap
                // with the marker without storing the probe page.
                let probe_before = floor.ok_or(Failure::InvalidEvidence)?.to_string();
                let probe = self
                    .executor
                    .fetch_channel_messages(channel, Some(probe_before.as_str()), 100)
                    .await?;
                if probe.len() > 100 {
                    return Err(Failure::InvalidEvidence);
                }
                if !probe.is_empty() {
                    capture.mark_page_capped();
                }
                break;
            }
            before = Some(floor.ok_or(Failure::InvalidEvidence)?.to_string());
        }
        Ok(capture.finish())
    }

    async fn cleanup(&self, id: &str, channel: &str) -> Result<()> {
        let ticket = self.store.get(id).await?.ok_or(Failure::InvalidEvidence)?;
        if ticket.status != TicketStatus::CleanupPending
            || ticket.channel_id.as_deref() != Some(channel)
        {
            return Err(Failure::InvalidEvidence);
        }
        match self.executor.fetch_ticket_channel(channel).await? {
            ChannelPresence::Present(doc) => {
                self.validate_channel(&doc, channel, 0)?;
                // Open rollback has no close token. Re-prove creation on every
                // retry, but preserve captured-close cleanup (opener writes are
                // frozen there, and retention may already have purged the body).
                if ticket.closing_started_at.is_none() {
                    self.verify_creation_channel(&doc, &ticket, channel, &self.live_config())?;
                }
                self.executor.delete_ticket_channel(channel).await?;
            }
            ChannelPresence::Absent => {}
        }
        self.store.finish_cleanup(id, now_millis_for_test()).await?;
        Ok(())
    }

    pub async fn recover(&self) -> std::result::Result<(), ErrorClass> {
        let Ok(_guard) = self.lane.try_lock() else {
            return Ok(());
        };
        // A cold RESUMED lookup can fail once without another gateway event.
        // Periodic recovery retries only authenticated identity until it is known.
        if self.bot_id.load(Ordering::Acquire) == 0 {
            let bot_id = bounded(IDENTITY_TIMEOUT, async {
                self.executor
                    .current_bot_user_id()
                    .await
                    .map_err(Failure::from)
            })
            .await
            .map_err(|error| error.class())?;
            self.on_ready(bot_id);
        }
        let tickets = bounded(RECOVERY_ITEM_TIMEOUT, async {
            self.store.recoverable().await.map_err(Failure::from)
        })
        .await;
        // The retry phase shares this row envelope: captured here so it matches
        // the deadline the forward batch computes internally. `recovery_batch`
        // keeps its signature because cursor/budget seam tests call it
        // directly; retries never extend row work past the existing budget.
        let rows_deadline = Instant::now() + RECOVERY_ROWS_BUDGET;
        let mut failed = match tickets {
            Ok(tickets) => {
                recovery_batch(&self.recovery_cursor, tickets, |ticket| async move {
                    self.recover_ticket_traced(&ticket).await
                })
                .await
            }
            Err(error) => Some(error.class()),
        };
        // Due Open retries run after the forward scan, so new rows keep their
        // priority; the retry phase re-reads every row and sheds terminal or
        // removed entries without Discord I/O.
        if let Some(class) = self.retry_open_rows(rows_deadline).await {
            failed = Some(class);
        }
        // Rows consume at most 180s, leaving a separate bounded panel slot
        // inside the 240s supervisor budget, even with a permanent slow prefix.
        if let Err(error) = bounded(PANEL_TIMEOUT, self.ensure_panel()).await {
            failed = Some(error.class());
        }
        failed.map_or(Ok(()), Err)
    }

    /// Re-attempt due Open retries after the forward scan, inside the same row
    /// budget. Eligible entries are tried oldest-due first up to
    /// [`OPEN_RETRY_PER_PASS_CAP`]; each attempt re-reads its row, so
    /// terminal or removed rows shed their entry without Discord I/O, and the
    /// forward cursor is never touched: retries neither advance nor rewind
    /// progress. Successful unchanged Open rows are never retained, so they
    /// stay zero-work here exactly as in the forward scan.
    async fn retry_open_rows(&self, rows_deadline: Instant) -> Option<ErrorClass> {
        let due: Vec<String> = {
            let retries = self.open_retries.lock().expect("ticket open retry set");
            let now = Instant::now();
            let mut due: Vec<(&String, &OpenRetryEntry)> = retries
                .iter()
                .filter(|(_, entry)| entry.eligible_at <= now)
                .collect();
            due.sort_by(|a, b| (a.1.eligible_at, a.0).cmp(&(b.1.eligible_at, b.0)));
            due.into_iter()
                .take(OPEN_RETRY_PER_PASS_CAP)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut failed = None;
        let total = due.len();
        for (index, ticket_id) in due.into_iter().enumerate() {
            let remaining = rows_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let dropped = total - index;
                tracing::warn!(dropped, "ticket open retry budget exhausted");
                break;
            }
            // The re-read is bounded by the row's item timeout: an unbounded
            // pool wait here could otherwise push row work past the shared
            // envelope and starve the panel slot.
            let fresh = tokio::time::timeout(
                remaining.min(RECOVERY_ITEM_TIMEOUT),
                self.store.get(&ticket_id),
            )
            .await;
            let ticket = match fresh {
                Ok(Ok(Some(ticket))) if ticket.status == TicketStatus::Open => ticket,
                Ok(Ok(Some(_))) => {
                    self.shed_open_retry(&ticket_id, "status_changed");
                    continue;
                }
                Ok(Ok(None)) => {
                    self.shed_open_retry(&ticket_id, "missing_row");
                    continue;
                }
                Ok(Err(error)) => {
                    // A sick store defers without attempting: no Discord I/O.
                    let failure = Failure::from(error);
                    self.defer_open_retry_read(&ticket_id, &failure);
                    failed = Some(failure.class());
                    continue;
                }
                Err(_) => {
                    // A stalled re-read defers like a sick store.
                    let failure = Failure::Timeout;
                    self.defer_open_retry_read(&ticket_id, &failure);
                    failed = Some(failure.class());
                    continue;
                }
            };
            // Size the attempt from a fresh reading: the re-read above already
            // spent part of this row's share of the envelope.
            let remaining = rows_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let dropped = total - index;
                tracing::warn!(dropped, "ticket open retry budget exhausted");
                break;
            }
            // The timeout arm drops the attempt future, so the guard retains
            // the entry and reports `interrupted`; only the pass outcome is
            // recorded here. The guard reports explicit outcomes from inside
            // the attempt.
            match tokio::time::timeout(
                remaining.min(RECOVERY_ITEM_TIMEOUT),
                self.recover_ticket_traced(&ticket),
            )
            .await
            {
                Ok((result, _)) => {
                    if let Err(error) = result {
                        failed = Some(error.class());
                    }
                }
                Err(_) => {
                    failed = Some(ErrorClass::Timeout);
                }
            }
        }
        failed
    }

    /// Defer one retry entry after its re-read failed or stalled: back the
    /// entry off and report one deferred line. The row is not attempted, so
    /// no Discord I/O happens on a sick or slow store.
    fn defer_open_retry_read(&self, ticket_id: &str, failure: &Failure) {
        let reason = open_retry_reason(failure);
        if let Ok(mut retries) = self.open_retries.lock() {
            if let Some(entry) = retries.get_mut(ticket_id) {
                backoff_entry(entry);
            }
        }
        tracing::warn!(
            ticket_id = %ticket_id,
            retryable = true,
            reason = %reason,
            outcome = %"deferred",
            next_action = %"await_retry",
            "ticket open retry deferred"
        );
    }

    /// Drop one retry entry whose row can no longer be repaired: it left Open
    /// (`status_changed`) or it is gone (`missing_row`). Exactly one line per
    /// shed entry; the row's forward handling (if any) is unaffected.
    fn shed_open_retry(&self, ticket_id: &str, reason: &'static str) {
        self.open_retries
            .lock()
            .expect("ticket open retry set")
            .remove(ticket_id);
        tracing::info!(
            ticket_id = %ticket_id,
            retryable = false,
            reason = %reason,
            outcome = %"cancelled",
            next_action = %"none",
            "ticket open retry cancelled"
        );
    }

    // Test-only shim over the traced recovery: the ticket acceptance suites
    // drive recovery through the untraced API while production calls
    // `recover_ticket_traced` directly.
    #[cfg(test)]
    async fn recover_ticket(&self, ticket: &Ticket) -> Result<()> {
        self.recover_ticket_traced(ticket).await.0
    }

    /// Recover one row, reporting the selected recovery action alongside the
    /// outcome so the `ticket recovery deferred` warn can name the action
    /// that failed. The token is `None` when selection itself failed (the
    /// transcript probe) or selected no action.
    async fn recover_ticket_traced(&self, ticket: &Ticket) -> (Result<()>, Option<&'static str>) {
        // Entered before the probe: a probe failure, a later Discord
        // failure, and a mid-flight cancellation or timeout all retain Open
        // retry eligibility for a later eligible pass.
        let retry = OpenRetryGuard::enter(self, ticket);
        let saved = match self.store.transcript_exists(&ticket.id).await {
            Ok(saved) => saved,
            Err(error) => {
                let result: Result<()> = Err(error.into());
                if let Some(retry) = retry {
                    retry.settle(&result);
                }
                return (result, None);
            }
        };
        let action = recovery_action(ticket, now_millis_for_test(), saved);
        let attempted = recovery_action_token(&action);
        let result = self.run_recovery_action(ticket, action).await;
        if let Some(retry) = retry {
            retry.settle(&result);
        }
        (result, attempted)
    }

    async fn run_recovery_action(&self, ticket: &Ticket, action: RecoveryAction) -> Result<()> {
        let config = self.live_config();
        match action {
            RecoveryAction::None => {}
            RecoveryAction::ReattachOpenControls { channel_id } => {
                self.ensure_controls(ticket, &channel_id).await?
            }
            RecoveryAction::FindCreatingChannel { topic } => {
                let channels = self
                    .executor
                    .fetch_ticket_guild_channels(&config.guild_id)
                    .await?;
                let mut matches = Vec::new();
                for doc in channels {
                    // Validate the complete listing before inferring absence.
                    let id = doc["id"].as_str().ok_or(Failure::InvalidEvidence)?;
                    valid_id(id)?;
                    if doc["type"].as_u64().is_none() {
                        return Err(Failure::InvalidEvidence);
                    }
                    if doc
                        .get("guild_id")
                        .is_some_and(|guild| guild.as_str() != Some(&config.guild_id))
                    {
                        return Err(Failure::InvalidEvidence);
                    }
                    if doc["topic"].as_str() == Some(&topic) {
                        if doc["type"].as_u64() != Some(0) {
                            return Err(Failure::InvalidEvidence);
                        }
                        matches.push(id.to_owned());
                    }
                }
                match matches.as_slice() {
                    [] => {
                        let prior = ticket.status.as_str();
                        if self.store.abandon_creating(&ticket.id).await? {
                            emit_lifecycle(
                                "abandon",
                                "abandoned",
                                &ticket.id,
                                prior,
                                "orphan_not_found",
                            );
                        } else {
                            emit_lifecycle(
                                "abandon",
                                "already_gone",
                                &ticket.id,
                                prior,
                                "already_gone",
                            );
                        }
                    }
                    [channel] => self.rollback_creating(ticket, channel, &config).await?,
                    _ => return Err(Failure::InvalidEvidence),
                }
            }
            RecoveryAction::DeleteInterruptedCreate { channel_id } => {
                self.rollback_creating(ticket, &channel_id, &config).await?;
            }
            RecoveryAction::RetryCleanup { channel_id } => {
                self.cleanup(&ticket.id, &channel_id).await?
            }
            RecoveryAction::RecoverSavedClose { started_at } => {
                let ticket = self
                    .store
                    .recover_saved_close(&ticket.id, started_at)
                    .await?;
                self.cleanup(
                    &ticket.id,
                    ticket
                        .channel_id
                        .as_deref()
                        .ok_or(Failure::InvalidEvidence)?,
                )
                .await?;
            }
            RecoveryAction::RestoreOpenerThenReopen {
                channel_id,
                started_at,
            } => match self.executor.fetch_ticket_channel(&channel_id).await? {
                ChannelPresence::Absent => {
                    self.store
                        .abandon_unsaved_close(&ticket.id, started_at, now_millis_for_test())
                        .await?;
                }
                ChannelPresence::Present(doc) => {
                    self.validate_channel(&doc, &channel_id, 0)?;
                    self.set_writes(&doc, &channel_id, &ticket.opener_id, true)
                        .await?;
                    self.store
                        .reopen_interrupted(&ticket.id, started_at)
                        .await?;
                }
            },
        }
        Ok(())
    }

    async fn ensure_controls(&self, ticket: &Ticket, channel: &str) -> Result<()> {
        let config = self.live_config();
        let doc = match self.executor.fetch_ticket_channel(channel).await? {
            ChannelPresence::Present(doc) => {
                self.validate_channel(&doc, channel, 0)?;
                doc
            }
            ChannelPresence::Absent => {
                let prior = ticket.status.as_str();
                self.store
                    .retire_missing_open(&ticket.id, channel, now_millis_for_test())
                    .await?;
                emit_lifecycle("retire", "retired", &ticket.id, prior, "missing_channel");
                return Ok(());
            }
        };
        // Reattachment must not post controls into a reparented, retopiced or
        // de-privated channel: prove the creation receipt before history/post.
        self.verify_creation_channel(&doc, ticket, channel, &config)?;
        if !self
            .executor
            .ticket_history_readable(&config.guild_id, &self.bot_id()?, &doc)
            .await?
        {
            return Err(Failure::InvalidEvidence);
        }
        let messages = self
            .executor
            .fetch_channel_messages(channel, None, 50)
            .await?;
        let (panels, isolated) = panels_with_isolation(messages)?;
        let bot = self.bot_id()?;
        let attached = panels.iter().any(|message| {
            message.author_id == bot
                && message.custom_ids.iter().any(|id| id == TICKET_CLAIM_ID)
                && message.custom_ids.iter().any(|id| id == TICKET_CLOSE_ID)
        });
        if attached {
            return Ok(());
        }
        if isolated > 0 {
            // Uncertain evidence cannot prove absence: a malformed message
            // may hide the controls, so refuse instead of posting a possible
            // duplicate. A later eligible pass re-reads the full history.
            return Err(Failure::InvalidEvidence);
        }
        self.executor
            .post_ticket_message(
                channel,
                TicketMessage::Controls {
                    opener_id: &ticket.opener_id,
                },
            )
            .await?;
        Ok(())
    }

    async fn ensure_panel(&self) -> Result<()> {
        let config = self.live_config();
        let doc = self.channel_document(&config.panel_channel_id).await?;
        if !self
            .executor
            .ticket_history_readable(&config.guild_id, &self.bot_id()?, &doc)
            .await?
        {
            return Err(Failure::InvalidEvidence);
        }
        let messages = self
            .executor
            .fetch_channel_messages(&config.panel_channel_id, None, 50)
            .await?;
        let (panels, isolated) = panels_with_isolation(messages)?;
        if panel_needed(&self.bot_id()?, &panels) {
            // A malformed row may hide an existing panel. Preserve found-first
            // handling, but never treat uncertain history as proof of absence.
            if isolated > 0 {
                return Err(Failure::InvalidEvidence);
            }
            self.executor
                .post_ticket_message(&config.panel_channel_id, TicketMessage::Panel)
                .await?;
        }
        Ok(())
    }

    pub async fn purge(&self) -> std::result::Result<(), ErrorClass> {
        let Ok(_guard) = self.purge_lane.try_lock() else {
            return Ok(());
        };
        self.refresh_config_with(crate::settings_jobs::live().as_ref());
        match self.store.purge_expired(now_millis_for_test()).await {
            Ok(deleted) => {
                tracing::info!(
                    event = %"purge",
                    deleted,
                    cause = %"retention_expired",
                    "ticket_lifecycle"
                );
                metrics::global().ticket_lifecycle_n("purge", "deleted", deleted);
                Ok(())
            }
            Err(error) => {
                let outcome = purge_outcome(&error);
                tracing::info!(
                    event = %"purge",
                    deleted = 0,
                    cause = %outcome,
                    "ticket_lifecycle"
                );
                metrics::global().ticket_lifecycle("purge", outcome);
                Err(ErrorClass::Database)
            }
        }
    }
}

fn valid_id(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0 && id.to_string() == value)
        .ok_or(Failure::InvalidEvidence)
}

fn member_overwrite(doc: &Value, member: &str) -> Result<(u64, u64)> {
    let rows = doc["permission_overwrites"]
        .as_array()
        .ok_or(Failure::InvalidEvidence)?;
    let row = rows
        .iter()
        .find(|row| row["id"].as_str() == Some(member) && row["type"].as_u64() == Some(1))
        .ok_or(Failure::InvalidEvidence)?;
    let bits = |key| {
        row[key]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(Failure::InvalidEvidence)
    };
    Ok((bits("allow")?, bits("deny")?))
}

/// Discord allows at most 5 action rows of 5 buttons on one message, so a
/// legitimate panel message never carries more than 25 button custom ids.
/// Longer id lists are truncated to this bound; the message itself is kept.
const MAX_PANEL_CUSTOM_IDS: usize = 25;

/// Fixed process-wide count of isolated ticket panel rows: skipped malformed
/// messages plus over-bound truncations share this one counter. Log fields
/// echo only the count, never message content.
static PANEL_ISOLATED_ROWS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn panel_isolated_count_for_test() -> u64 {
    PANEL_ISOLATED_ROWS.load(Ordering::Relaxed)
}

/// Parse one channel-document message into a panel row. Returns `None` when
/// the message is malformed (bad author shape, bad components shape, or any
/// bad action row drops the whole message). Buttons without a string
/// `custom_id` are ignored, never fatal. The boolean is true when the
/// message was kept with its `custom_ids` truncated to
/// [`MAX_PANEL_CUSTOM_IDS`].
fn parse_panel_message(message: &Value) -> Option<(PanelMessage, bool)> {
    let author_id = message.get("author")?.get("id")?.as_str()?.to_owned();
    let mut custom_ids = Vec::new();
    let mut truncated = false;
    match message.get("components") {
        None => {}
        Some(value) => {
            let rows = value.as_array()?;
            for row in rows {
                let buttons = row.get("components")?.as_array()?;
                for button in buttons {
                    if let Some(id) = button.get("custom_id").and_then(Value::as_str) {
                        if custom_ids.len() < MAX_PANEL_CUSTOM_IDS {
                            custom_ids.push(id.to_owned());
                        } else {
                            truncated = true;
                        }
                    }
                }
            }
        }
    }
    Some((
        PanelMessage {
            author_id,
            custom_ids,
        },
        truncated,
    ))
}

#[cfg(test)]
fn panels(messages: Vec<Value>) -> Result<Vec<PanelMessage>> {
    panels_with_isolation(messages).map(|(panels, _)| panels)
}

/// Parse one history batch into panel rows, reporting how many messages were
/// isolated (skipped as malformed or truncated over the id bound). Callers
/// that would POST on absence must treat `isolated > 0` without a found
/// panel as uncertain evidence, never as proof of absence.
fn panels_with_isolation(messages: Vec<Value>) -> Result<(Vec<PanelMessage>, u64)> {
    let mut out = Vec::with_capacity(messages.len());
    let mut skipped = 0_u64;
    for message in &messages {
        match parse_panel_message(message) {
            Some((panel, truncated)) => {
                if truncated {
                    skipped += 1;
                }
                out.push(panel);
            }
            None => {
                skipped += 1;
            }
        }
    }
    if skipped > 0 {
        PANEL_ISOLATED_ROWS.fetch_add(skipped, Ordering::Relaxed);
        tracing::debug!(skipped, "ticket panel messages isolated");
    }
    Ok((out, skipped))
}

fn boot_deployment(
    config: &TicketConfig,
    mut deployment: HashMap<String, String>,
) -> HashMap<String, String> {
    for (key, value) in [
        ("DISCORD_TICKET_CATEGORY_ID", &config.category_id),
        ("DISCORD_TICKET_PANEL_CHANNEL_ID", &config.panel_channel_id),
        ("DISCORD_TICKET_STAFF_ROLE_ID", &config.staff_role_id),
    ] {
        deployment
            .entry(key.to_owned())
            .or_insert_with(|| value.clone());
    }
    deployment.insert(COOLDOWN_KEY.to_owned(), config.cooldown_seconds.to_string());
    deployment
}

#[cfg(test)]
#[path = "ticket_runtime_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ticket_drop_poison_acceptance_tests.rs"]
mod drop_poison_acceptance_tests;

#[cfg(test)]
#[path = "ticket_drop_poison_counter_acceptance_tests.rs"]
mod drop_poison_counter_acceptance_tests;

#[cfg(test)]
#[path = "ticket_timer_tests.rs"]
mod timer_tests;

#[cfg(test)]
#[path = "ticket_acceptance_tests.rs"]
mod acceptance_tests;

#[cfg(test)]
#[path = "ticket_revision_tests.rs"]
mod revision_tests;

#[cfg(test)]
#[path = "ticket_panels_isolation_tests.rs"]
mod panels_isolation_tests;

#[cfg(test)]
#[path = "ticket_no_work_acceptance_tests.rs"]
mod no_work_acceptance_tests;

#[cfg(test)]
#[path = "ticket_claim_close_truth_acceptance_tests.rs"]
mod claim_close_truth_acceptance_tests;

#[cfg(test)]
#[path = "ticket_failure_class_tests.rs"]
mod failure_class_tests;

#[cfg(test)]
#[path = "ticket_cursor_budget_acceptance_tests.rs"]
mod cursor_budget_acceptance_tests;

#[cfg(test)]
#[path = "ticket_cursor_budget_interaction_tests.rs"]
mod cursor_budget_interaction_tests;

#[cfg(test)]
#[path = "ticket_lifecycle_observability_acceptance_tests.rs"]
mod lifecycle_observability_acceptance_tests;

#[cfg(test)]
#[path = "ticket_lifecycle_review_guard_acceptance_tests.rs"]
mod review_guard_acceptance_tests;

#[cfg(test)]
#[path = "ticket_creation_receipt_acceptance_tests.rs"]
mod creation_receipt_acceptance_tests;

#[cfg(test)]
#[path = "ticket_destination_move_tests.rs"]
mod destination_move_tests;

#[cfg(test)]
#[path = "ticket_capture_cap_acceptance_tests.rs"]
mod capture_cap_acceptance_tests;

#[cfg(test)]
#[path = "ticket_capture_cap_boundary_tests.rs"]
mod capture_cap_boundary_tests;
