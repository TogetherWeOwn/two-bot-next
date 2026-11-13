//! Event Echoes attendance: the event-window voice-session log and its read.
//!
//! The logger half records one row per (member, channel, joined_at, left_at)
//! from `VOICE_STATE_UPDATE` transitions, but only while a mapped event window
//! is open for the guild. Outside event windows nothing is recorded — never an
//! always-on voice log. A window is `[starts_at - 15 min, ends_at + 15 min]`
//! per mapped event, resolved from shared accepted-instance lifecycle evidence.
//! Historical capture records never grant permission to capture. Sessions
//! overlapping two eligible windows tag both; close-time tags are revalidated
//! transactionally with capture-time privacy gates.
//!
//! The read half projects the §4 shape for the keyless
//! `event.attendance.read` verb from the stored rows: positive list only
//! (who was there, never absence), bots excluded from members, bots plus AFK
//! occupants excluded from the peak, below-threshold members included with
//! their minutes, and `final` false until the window plus grace has passed.
//!
//! Hook discipline mirrors `crate::event_voice_channels`: the guild actor
//! loop owns one [`AttendanceTransition`] at a time (ordering preserved),
//! with one total deadline and close-work bound per turn so a
//! slow store cannot monopolize room dispatch. Failures and timeouts are debug-logged
//! with the guild only — no member or channel ids on this warm path, per
//! docs/log-volume-guard.md — and never fail the actor loop. A missed open
//! stays missed (the documented join-before-window gap); unfinished closes
//! retain their original evidence across bounded maintenance turns. Room input
//! keeps flowing; skipped attendance frames require conservative recovery.
//! No open passes unfinished closes.
//!
//! Contract v2 §9 opt-out at capture: before opening a span the logger
//! consults `privacy_opt_outs` (`suppressed_in`) and
//! `guild_analytics_settings.voice_enabled`. A suppressed member, or a guild
//! with voice capture off, opens nothing — absent from the record, not
//! anonymised in it. Closes still finalize so no open row leaks; gate read
//! failures fail closed by dropping the open.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use time::OffsetDateTime;
use two_bot_core::Snowflake;
use two_bot_cutover::event_attendance::{
    AttendanceSession, AttendanceWindow, NewAttendanceSession,
};

/// Total attendance work deadline per actor turn, including every store await.
const REVALIDATE_TIMEOUT_MS: u64 = 1_000;

/// Per-process guild fences shared by gateway actors and signed reads. An
/// unwired guild refuses reads, including before its first actor has loaded.
#[derive(Clone, Default)]
pub struct AttendanceReadAuthority {
    guilds: Arc<Mutex<HashMap<Snowflake, AttendanceReadFence>>>,
}

impl AttendanceReadAuthority {
    /// Register an actor's fence before publishing that actor. Replacing an
    /// actor also invalidates tickets issued by its previous incarnation.
    pub fn register(&self, guild: Snowflake, fence: AttendanceReadFence) {
        let mut guilds = self
            .guilds
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = guilds.insert(guild, fence) {
            previous.invalidate();
        }
    }

    /// Acquire permission to read only after this actor's recovery completed.
    pub fn ticket(&self, guild: Snowflake) -> Option<AttendanceReadTicket> {
        let fence = self
            .guilds
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&guild)
            .cloned()?;
        fence.ticket()
    }
}

/// Loss metadata is identity/incarnation only, never retained voice frames.
pub(crate) const ATTENDANCE_LOSS_MEMBERS: usize = 32;

#[derive(Debug, Default)]
struct AttendanceFenceState {
    epoch: u64,
    recovery: u64,
    /// Most recent full-sweep incarnation, including after it completes.
    full_recovery: u64,
    ready: bool,
    observations: usize,
    loss_generation: u64,
    lost_members: Vec<(Snowflake, u64)>,
    saturated: bool,
}

impl AttendanceFenceState {
    fn capture_allowed(&self) -> bool {
        self.ready && self.lost_members.is_empty() && !self.saturated
    }
}

/// One guild's read/capture recovery epoch. Gateway interruption invalidates
/// it synchronously; actor recovery owns readiness, never the receiver.
#[derive(Debug, Clone, Default)]
pub struct AttendanceReadFence {
    inner: Arc<Mutex<AttendanceFenceState>>,
}

impl AttendanceReadFence {
    // Keep mutex acquisition/poison recovery shared across hot-path fence checks.
    #[inline(never)]
    fn state(&self) -> std::sync::MutexGuard<'_, AttendanceFenceState> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Revoke readiness and every outstanding read ticket, without a DB await.
    pub fn invalidate(&self) {
        let mut state = self.state();
        state.epoch += 1;
        state.recovery += 1;
        state.ready = false;
    }

    /// Start a new recovery incarnation. Keep its guard alive for the actor's
    /// lifetime; dropping it revokes readiness on normal exit or panic unwind.
    pub fn begin_recovery(&self) -> AttendanceRecoveryGuard {
        let mut state = self.state();
        state.epoch += 1;
        state.recovery += 1;
        state.ready = false;
        AttendanceRecoveryGuard {
            fence: self.clone(),
            recovery: state.recovery,
        }
    }

    /// Fence a gateway observation before enqueueing or changing live membership.
    /// Dropping it without acknowledgement records lost continuity, including
    /// capacity refusal and actor termination, without allocating another queue.
    pub(crate) fn observe(&self, member: Snowflake) -> AttendanceObservation {
        let mut state = self.state();
        state.epoch += 1;
        state.observations += 1;
        AttendanceObservation {
            fence: self.clone(),
            member,
            // No capture occurs during a full sweep. Its observations remain
            // read barriers, but their dropped frames cannot lose newer rows.
            loss_recovery: (state.ready || state.recovery != state.full_recovery)
                .then_some(state.recovery),
            acknowledged: false,
        }
    }

    /// Only an actual actor/gateway discontinuity may clear unknown loss.
    pub(crate) fn begin_full_recovery(&self) -> AttendanceRecoveryGuard {
        let mut state = self.state();
        state.epoch += 1;
        state.recovery += 1;
        state.ready = false;
        state.full_recovery = state.recovery;
        state.lost_members.clear();
        state.saturated = false;
        AttendanceRecoveryGuard {
            fence: self.clone(),
            recovery: state.recovery,
        }
    }

    pub(crate) fn capture_allowed(&self) -> bool {
        self.state().capture_allowed()
    }

    pub(crate) fn next_loss(&self) -> Option<(Snowflake, u64)> {
        self.state().lost_members.first().copied()
    }

    /// Compare incarnations after commit; an older page cannot erase newer loss.
    pub(crate) fn finish_loss(&self, loss: (Snowflake, u64)) -> bool {
        let mut state = self.state();
        if let Some(index) = state.lost_members.iter().position(|entry| *entry == loss) {
            state.lost_members.swap_remove(index);
            return true;
        }
        false
    }

    pub(crate) fn loss_is_current(&self, loss: (Snowflake, u64)) -> bool {
        self.state().lost_members.contains(&loss)
    }

    /// The transition itself holds readiness false; check only continuity at
    /// its final open boundary, including loss before its baseline was sampled.
    pub(crate) fn may_open(&self, loss_generation: u64) -> bool {
        let state = self.state();
        state.loss_generation == loss_generation
            && state.lost_members.is_empty()
            && !state.saturated
    }

    pub(crate) fn loss_generation(&self) -> u64 {
        self.state().loss_generation
    }

    fn ticket(&self) -> Option<AttendanceReadTicket> {
        let state = self.state();
        (state.capture_allowed() && state.observations == 0).then(|| AttendanceReadTicket {
            fence: self.clone(),
            epoch: state.epoch,
        })
    }
}

/// An unresolved ingress observation. Ownership follows the queued command,
/// then its retained close continuation; room dispatch never waits on this guard.
pub(crate) struct AttendanceObservation {
    fence: AttendanceReadFence,
    member: Snowflake,
    /// None for a frame already covered by the pending full sweep.
    loss_recovery: Option<u64>,
    acknowledged: bool,
}

impl AttendanceObservation {
    pub(crate) fn acknowledge(mut self) {
        self.acknowledged = true;
    }
}

impl Drop for AttendanceObservation {
    fn drop(&mut self) {
        let mut state = self.fence.state();
        state.observations -= 1;
        // A newer full sweep also covers observations queued before it began.
        // Never clear losses here: a post-sweep observation is still genuine,
        // even if an older covered guard is dropped after that newer loss.
        if !self.acknowledged
            && self
                .loss_recovery
                .is_some_and(|recovery| recovery >= state.full_recovery)
        {
            state.loss_generation += 1;
            state.epoch += 1;
            let incarnation = state.loss_generation;
            if let Some(entry) = state
                .lost_members
                .iter_mut()
                .find(|entry| entry.0 == self.member)
            {
                entry.1 = incarnation;
            } else if state.lost_members.len() < ATTENDANCE_LOSS_MEMBERS {
                state.lost_members.push((self.member, incarnation));
            } else {
                // Unknown loss stops capture/publication, never a guild sweep.
                state.saturated = true;
            }
        }
    }
}

/// Actor-owned completion authority for exactly one recovery incarnation.
pub struct AttendanceRecoveryGuard {
    fence: AttendanceReadFence,
    recovery: u64,
}

impl AttendanceRecoveryGuard {
    /// Called only after the final bounded page commits and gateway evidence
    /// is revalidated. A disconnect or replacement meanwhile refuses completion.
    pub fn complete(&self) -> bool {
        let mut state = self.fence.state();
        if state.recovery != self.recovery {
            return false;
        }
        state.ready = true;
        true
    }
}

impl Drop for AttendanceRecoveryGuard {
    fn drop(&mut self) {
        let mut state = self.fence.state();
        if state.recovery == self.recovery {
            state.epoch += 1;
            state.recovery += 1;
            state.ready = false;
        }
    }
}

/// A read must revalidate its original epoch immediately before returning,
/// even if a newer recovery has already finished by then.
pub struct AttendanceReadTicket {
    fence: AttendanceReadFence,
    epoch: u64,
}

impl AttendanceReadTicket {
    /// Whether the read still belongs to the recovered incarnation it began in.
    pub fn is_current(&self) -> bool {
        let state = self.fence.state();
        state.capture_allowed() && state.observations == 0 && state.epoch == self.epoch
    }
}

#[cfg(test)]
mod ingress_fence_tests {
    use super::*;

    #[test]
    fn member_loss_incarnations_are_bounded_and_saturation_requires_real_recovery() {
        let fence = AttendanceReadFence::default();
        let actor = fence.begin_full_recovery();
        assert!(actor.complete());
        let ticket = fence.ticket().unwrap();
        for member in 1..=32 {
            drop(fence.observe(member));
        }
        assert_eq!(
            fence.inner.lock().unwrap().lost_members.len(),
            ATTENDANCE_LOSS_MEMBERS
        );
        let old_loss = fence.next_loss().unwrap();
        drop(fence.observe(old_loss.0));
        assert!(
            !fence.finish_loss(old_loss),
            "newer loss survives old completion"
        );
        assert_eq!(fence.inner.lock().unwrap().lost_members.len(), 32);
        drop(fence.observe(33));
        for member in 34..=1000 {
            drop(fence.observe(member));
        }
        assert_eq!(fence.inner.lock().unwrap().lost_members.len(), 32);
        assert!(fence.inner.lock().unwrap().saturated);
        while let Some(loss) = fence.next_loss() {
            assert!(fence.finish_loss(loss));
        }
        assert!(actor.complete());
        assert!(!fence.capture_allowed(), "drain cannot clear saturation");
        assert!(fence.ticket().is_none());
        assert!(!ticket.is_current());
        let transition = fence.begin_recovery();
        assert!(transition.complete());
        assert!(
            !fence.capture_allowed(),
            "ordinary close cannot clear saturation"
        );
        fence.invalidate();
        let replacement = fence.begin_full_recovery();
        assert!(replacement.complete());
        assert!(fence.capture_allowed());
        assert!(!ticket.is_current());
    }

    #[test]
    fn full_recovery_covers_old_and_queued_loss_but_preserves_new_loss() {
        let fence = AttendanceReadFence::default();
        let actor = fence.begin_full_recovery();
        assert!(actor.complete());
        let old_ticket = fence.ticket().unwrap();
        let before_reset = fence.observe(1);
        fence.invalidate();
        let reset = fence.begin_full_recovery();
        let queued = fence.observe(2);
        drop(before_reset);
        for member in 3..=40 {
            drop(fence.observe(member));
        }
        assert!(fence.next_loss().is_none(), "full sweep covers these rows");
        assert!(!fence.capture_allowed());
        assert!(fence.ticket().is_none());
        assert!(reset.complete());
        assert!(fence.capture_allowed());
        assert!(
            fence.ticket().is_none(),
            "queued observation still fences reads"
        );
        drop(queued);
        assert!(fence.ticket().is_some());
        assert!(!old_ticket.is_current());

        let next_reset = fence.begin_full_recovery();
        let delayed = fence.observe(9);
        assert!(next_reset.complete());
        drop(fence.observe(9));
        let new_loss = fence.next_loss().unwrap();
        drop(delayed);
        assert_eq!(fence.next_loss(), Some(new_loss));
        assert!(next_reset.complete(), "completion cannot clear newer loss");
        assert!(!fence.capture_allowed());
        assert!(fence.finish_loss(new_loss));
        assert!(fence.ticket().is_some());
    }

    #[test]
    fn all_queued_observations_must_finish_without_reviving_old_tickets() {
        let authority = AttendanceReadAuthority::default();
        let fence = AttendanceReadFence::default();
        authority.register(7, fence.clone());
        let actor = fence.begin_recovery();
        assert!(actor.complete());
        let old = authority.ticket(7).unwrap();
        let join = fence.observe(42);
        let movement = fence.observe(42);
        assert!(!old.is_current());
        assert!(authority.ticket(7).is_none());
        assert!(
            actor.complete(),
            "observations do not cancel recovery ownership"
        );
        join.acknowledge();
        assert!(
            authority.ticket(7).is_none(),
            "second observation remains queued"
        );
        movement.acknowledge();
        assert!(authority.ticket(7).unwrap().is_current());
        assert!(!old.is_current());
        drop(fence.observe(42));
        assert!(
            actor.complete(),
            "member loss does not cancel actor ownership"
        );
        assert!(
            authority.ticket(7).is_none(),
            "member quarantine fences reads"
        );
        let recovered = fence.begin_full_recovery();
        assert!(recovered.complete());
        assert!(authority.ticket(7).unwrap().is_current());
        drop(actor);
        assert!(
            authority.ticket(7).is_some(),
            "old guard cannot revoke replacement"
        );
    }
}

/// Store surface the session logger needs. Narrower than
/// `voice_rooms::RoomPersistence`, which forwards every store through a
/// blanket impl (see `voice_rooms.rs`). Bounds match the `RoomPersistence`
/// attendance defaults. Public: [`record_transition`] is public, and the
/// binary-only read half needs the projection items visible through
/// `two_bot::event_attendance`.
pub trait AttendanceStore: Send + Sync {
    type Error: std::fmt::Display;
    /// A second SQL sanitizer can refuse metadata changed after the page read.
    fn unsupported_stored_tags(_error: &Self::Error) -> bool {
        false
    }
    fn attendance_windows(
        &self,
        guild: Snowflake,
        member: Snowflake,
        previous: Option<Snowflake>,
        observed_at: OffsetDateTime,
    ) -> impl Future<Output = Result<Vec<AttendanceWindow>, Self::Error>> + Send;
    fn open_attendance_sessions(
        &self,
        guild: Snowflake,
        member: Snowflake,
    ) -> impl Future<Output = Result<Vec<AttendanceSession>, Self::Error>> + Send;
    fn insert_attendance_session(
        &self,
        new: NewAttendanceSession,
    ) -> impl Future<Output = Result<i64, Self::Error>> + Send;
    fn close_attendance_session(
        &self,
        id: i64,
        left_at: OffsetDateTime,
        channel_name: Option<String>,
        event_keys: &[String],
    ) -> impl Future<Output = Result<u64, Self::Error>> + Send;
    fn attendance_sessions_for_event_key(
        &self,
        guild: Snowflake,
        event_key: &str,
    ) -> impl Future<Output = Result<Vec<AttendanceSession>, Self::Error>> + Send;
    /// Whether the pair holds a guild-scoped opt-out suppression row. Checked
    /// before opening a span; a suppressed member opens nothing.
    fn is_attendance_suppressed(
        &self,
        guild: Snowflake,
        member: Snowflake,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    /// Whether guild-wide voice capture is enabled. Checked before opening
    /// a span; `voice_enabled = off` opens nothing.
    fn is_attendance_voice_enabled(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}

/// Recording grace on each side of `[starts_at, ends_at]`, in millis.
pub(crate) const ATTENDANCE_GRACE_MS: i64 = 15 * 60 * 1_000;
/// Minutes of in-window voice that mark an attendee. Travels in the payload;
/// members below it stay listed with their minutes. Public: the read
/// projection (used by the binary) sends it on the wire.
pub const ATTENDANCE_THRESHOLD_MINUTES: i64 = 10;
/// Relayed-answer budget for the serialized `result` object, mirroring
/// `voice_snapshot`: the Worker refuses bodies over its 64 KiB bound, so the
/// 512-byte reserve covers the `{"ok","result","request_id"}` envelope.
const MAX_ATTENDANCE_RESULT_BYTES: usize = 64 * 1024 - 512;

/// One mapped event window, in epoch millis. Public: the read projection
/// takes it, and the binary builds it from stored windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendanceWindowMs {
    pub event_key: String,
    pub starts_ms: i64,
    pub ends_ms: i64,
}

/// One still-open session row the logger must finalize. The channel is
/// `None` when the stored id is corrupt (TEXT snowflake convention): the row
/// still finalizes, but the name refresh skips it and the stored name holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenRow {
    pub id: i64,
    pub channel_id: Option<Snowflake>,
    pub joined_ms: i64,
    pub event_keys: Vec<String>,
}

/// One row to finalize: the stamp and the widened tag union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosePlan {
    pub id: i64,
    pub left_ms: i64,
    pub tag_union: Vec<String>,
}

/// One row to open, tagged with the windows containing the join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenPlan {
    pub tags: Vec<String>,
}

/// The logger's decision for one transition. Empty (no closes, no open)
/// means no write: outside windows with no open state, nothing is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TransitionPlan {
    pub closes: Vec<ClosePlan>,
    pub open: Option<OpenPlan>,
}

fn window_open_ms(starts_ms: i64, ends_ms: i64) -> (i64, i64) {
    (
        starts_ms.saturating_sub(ATTENDANCE_GRACE_MS),
        ends_ms.saturating_add(ATTENDANCE_GRACE_MS),
    )
}

fn overlaps_ms(starts_ms: i64, ends_ms: i64, from_ms: i64, to_ms: i64) -> bool {
    let (open, close) = window_open_ms(starts_ms, ends_ms);
    open <= to_ms && from_ms <= close
}

/// Windows overlapping `[from_ms, to_ms]`, keys sorted and deduplicated.
#[inline(never)]
fn overlapping_keys(windows: &[AttendanceWindowMs], from_ms: i64, to_ms: i64) -> Vec<String> {
    let mut keys: Vec<String> = windows
        .iter()
        .filter(|window| overlaps_ms(window.starts_ms, window.ends_ms, from_ms, to_ms))
        .map(|window| window.event_key.clone())
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

// A join is a zero-width overlap. Share the bounded tag collector with closes,
// without an intermediate window-reference vector.
fn plan_open(
    windows: &[AttendanceWindowMs],
    previous: Option<Snowflake>,
    current: Option<Snowflake>,
    now_ms: i64,
) -> Option<OpenPlan> {
    if previous == current || current.is_none() {
        return None;
    }
    let tags = overlapping_keys(windows, now_ms, now_ms);
    (!tags.is_empty()).then_some(OpenPlan { tags })
}

/// Decide the writes for one voice transition. Pure over the stored windows,
/// the member's open rows and the transition, so the tagging rules pin
/// without a database:
///
/// - same-channel frames plan nothing (the caller filters them already);
/// - a row matching the observed previous channel finalizes at observation
///   time, preserving continuous moves and leaves even after grace;
/// - every foreign row closes at its own `joined_at` (zero length, original
///   tags only). Neither a shared event nor a fresh join proves continuity
///   after a missed close;
/// - the tag union widens to every window overlapping `[joined_at, left_at]`,
///   which tags campers that never transition mid-event when they finally
///   leave during a later window;
/// - opens happen only while a window is open, tagged with exactly the
///   windows containing the join; a leave with no open row writes nothing.
pub(crate) fn plan_transition(
    windows: &[AttendanceWindowMs],
    open_rows: &[OpenRow],
    previous: Option<Snowflake>,
    current: Option<Snowflake>,
    now_ms: i64,
) -> TransitionPlan {
    if previous == current {
        return TransitionPlan::default();
    }
    let mut closes = Vec::new();
    for row in open_rows {
        // Only the observed previous channel proves continuity. A shared event
        // cannot distinguish a continuous move from a fresh rejoin after a lost
        // leave. Foreign rows (including on leaves) conservatively lose credit.
        let continuous = row.channel_id.is_some() && row.channel_id == previous;
        let stale = !continuous;
        let left_ms = if stale { row.joined_ms } else { now_ms };
        let mut tag_union = row.event_keys.clone();
        if continuous {
            for key in overlapping_keys(windows, row.joined_ms, left_ms) {
                if !tag_union.contains(&key) {
                    tag_union.push(key);
                }
            }
        }
        tag_union.sort();
        closes.push(ClosePlan {
            id: row.id,
            left_ms,
            tag_union,
        });
    }
    let open = plan_open(windows, previous, current, now_ms);
    TransitionPlan { closes, open }
}

/// Whether the attendance logger writes. Resolved once per process from the
/// same env-only flag that gates the read verb: a flip restarts the
/// container, so no per-transition env scan on the voice hot path.
pub(crate) fn logger_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("TWO_INTERNAL_ALLOW_ATTENDANCE_READ").is_ok_and(|value| value == "1")
    })
}

/// One voice-channel transition (join, leave or move) for the session logger.
/// Bundles the actor loop's frame — `is_bot` and `afk_channel` arrive from
/// the dispatch (the cache view of the transition), the channel names are
/// the actor's latest live view (refreshed at close so renames converge),
/// and `enabled` is the env-only flag that also gates the read verb — so
/// [`record_transition`] stays under the arity lint without a checker waiver.
/// Public: the guild actor loop builds this per transition.
#[derive(Debug, Clone)]
pub struct VoiceTransition {
    pub guild: Snowflake,
    pub member: Snowflake,
    pub previous: Option<Snowflake>,
    pub current: Option<Snowflake>,
    pub is_bot: bool,
    pub afk_channel: Option<Snowflake>,
    pub previous_name: Option<String>,
    pub current_name: Option<String>,
    /// Gateway observation time, never the actor's later processing time.
    pub observed_at: OffsetDateTime,
    pub enabled: bool,
}

/// Maximum close attempts per actor turn, independent of statement latency.
const CLOSES_PER_TURN: usize = 8;

/// Bounded finalization continuation owned by the guild actor. It retains the
/// original observation, names, candidate windows and any uncertain close plan.
/// No new open is attempted until every old row is finalized. The actor keeps
/// consuming room input while maintenance drains pages; skipped attendance
/// observations require conservative recovery instead of another unbounded queue.
pub struct AttendanceTransition {
    transition: VoiceTransition,
    windows: Option<Vec<AttendanceWindowMs>>,
    closes: VecDeque<(ClosePlan, Option<String>)>,
    rows_exhausted: bool,
    allow_open: bool,
    complete: bool,
}

impl AttendanceTransition {
    /// Retain original transition evidence before the first cancellable await.
    pub fn new(mut transition: VoiceTransition) -> Self {
        // Discord names are bounded, but alternate callers must not retain
        // unbounded text in the continuation. Unavailable metadata preserves
        // the stored name rather than replacing it with a truncated value.
        if transition
            .previous_name
            .as_ref()
            .is_some_and(|name| name.len() > 4096)
        {
            transition.previous_name = None;
        }
        let allow_open = !transition
            .current_name
            .as_ref()
            .is_some_and(|name| name.len() > 4096);
        if !allow_open {
            transition.current_name = None;
        }
        let complete = !transition.enabled || transition.previous == transition.current;
        Self {
            transition,
            windows: None,
            closes: VecDeque::new(),
            rows_exhausted: false,
            allow_open,
            complete,
        }
    }

    /// Whether the transition needs another bounded maintenance turn.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Resume under one deadline covering all reads, gates and writes. A close
    /// advances only on a successful response; cancellation retries the exact
    /// same id/boundary/tags, including after an uncertain commit.
    pub async fn turn(&mut self, store: &impl AttendanceStore) {
        self.turn_if(store, || true).await;
    }

    /// Revalidate live actor/gateway authority immediately before an open.
    /// Closes retain their evidence even when that authority has been revoked.
    pub(crate) async fn turn_if(
        &mut self,
        store: &impl AttendanceStore,
        may_open: impl Fn() -> bool + Send,
    ) {
        if self.complete {
            return;
        }
        // Keep the multi-stage store future out of the actor's timeout/select
        // state machine. One allocation per bounded turn; cancellation still
        // drops the future while retained close evidence stays in `self`.
        let advance: std::pin::Pin<Box<dyn Future<Output = Result<(), ()>> + Send + '_>> =
            Box::pin(self.advance(store, may_open));
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS),
            advance,
        )
        .await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::debug!(
                guild = self.transition.guild.to_string(),
                "attendance transition deferred"
            );
        }
    }

    // Keep synchronous planning and its temporary collections outside the
    // generic async state machine, including its cancellation/drop paths.
    #[inline(never)]
    fn retain_windows(&mut self, windows: Vec<AttendanceWindow>) {
        use two_bot_cutover::event_attendance::ATTENDANCE_MAX_WINDOWS;
        // Alternate stores must obey the same bounds as the SQL adapter.
        // An oversized candidate set grants no new tags or opens; existing
        // tags and the observed close boundary are still finalized.
        let bounded = windows.len() <= ATTENDANCE_MAX_WINDOWS
            && windows.iter().all(|w| w.event_key.len() <= 128);
        self.allow_open &= bounded;
        self.windows = Some(if bounded {
            windows
                .into_iter()
                .map(|w| AttendanceWindowMs {
                    event_key: w.event_key,
                    starts_ms: millis_of(w.starts_at),
                    ends_ms: millis_of(w.ends_at),
                })
                .collect()
        } else {
            Vec::new()
        });
    }

    #[inline(never)]
    fn retain_close_page(&mut self, rows: Vec<AttendanceSession>) -> Result<(), ()> {
        use two_bot_cutover::event_attendance::{
            ATTENDANCE_MAX_TAG_BYTES, ATTENDANCE_MAX_WINDOWS, ATTENDANCE_RECOVERY_PAGE_SIZE,
        };
        let t = &self.transition;
        if rows.len() > ATTENDANCE_RECOVERY_PAGE_SIZE {
            return Err(());
        }
        let bounded: Vec<bool> = rows
            .iter()
            .map(|r| {
                r.event_keys.len() <= ATTENDANCE_MAX_WINDOWS
                    && r.event_keys.iter().map(String::len).sum::<usize>()
                        <= ATTENDANCE_MAX_TAG_BYTES
                    && r.channel_name.len() <= 4096
                    && r.channel_id.len() <= 20
                    && r.member_id.len() <= 20
            })
            .collect();
        self.allow_open &= bounded.iter().all(|bounded| *bounded);
        self.rows_exhausted = rows.len() < ATTENDANCE_RECOVERY_PAGE_SIZE;
        let open_rows: Vec<_> = rows
            .iter()
            .zip(&bounded)
            .map(|(r, bounded)| OpenRow {
                id: r.id,
                channel_id: (r.channel_id.len() <= 20)
                    .then(|| r.channel_id.parse().ok())
                    .flatten(),
                joined_ms: millis_of(r.joined_at),
                event_keys: if *bounded {
                    r.event_keys.clone()
                } else {
                    Vec::new()
                },
            })
            .collect();
        let plan = plan_transition(
            self.windows.as_deref().unwrap_or_default(),
            &open_rows,
            t.previous,
            t.current,
            millis_of(t.observed_at),
        );
        self.closes = plan
            .closes
            .into_iter()
            .zip(open_rows)
            .zip(bounded)
            .map(|((mut close, row), bounded)| {
                if !bounded {
                    // Empty expansion means finalize by id, preserving the
                    // original metadata server-side instead of sentinel text.
                    close.tag_union.clear();
                    return (close, None);
                }
                let name = (row.channel_id.is_some() && row.channel_id == t.previous)
                    .then(|| t.previous_name.clone())
                    .flatten();
                (close, name)
            })
            .collect();
        Ok(())
    }

    async fn advance<S: AttendanceStore>(
        &mut self,
        store: &S,
        may_open: impl Fn() -> bool,
    ) -> Result<(), ()> {
        if self.windows.is_none() {
            let windows = store
                .attendance_windows(
                    self.transition.guild,
                    self.transition.member,
                    self.transition.previous,
                    self.transition.observed_at,
                )
                .await
                .map_err(|_| ())?;
            self.retain_windows(windows);
        }
        if self.closes.is_empty() && !self.rows_exhausted {
            let rows = store
                .open_attendance_sessions(self.transition.guild, self.transition.member)
                .await
                .map_err(|_| ())?;
            self.retain_close_page(rows)?;
        }
        let t = &self.transition;
        for _ in 0..CLOSES_PER_TURN {
            let Some((close, name)) = self.closes.front_mut() else {
                break;
            };
            if let Err(error) = store
                .close_attendance_session(
                    close.id,
                    datetime_of(close.left_ms),
                    name.clone(),
                    &close.tag_union,
                )
                .await
            {
                if S::unsupported_stored_tags(&error) {
                    // The row changed shape since planning. Retain the observed
                    // boundary, but retry by id without expansion or new capture.
                    close.tag_union.clear();
                    *name = None;
                    self.allow_open = false;
                }
                return Err(());
            }
            self.closes.pop_front();
        }
        if !self.closes.is_empty() || !self.rows_exhausted {
            return Ok(());
        }
        let age = OffsetDateTime::now_utc() - t.observed_at;
        let tags = plan_transition(
            self.windows.as_deref().unwrap_or_default(),
            &[],
            t.previous,
            t.current,
            millis_of(t.observed_at),
        )
        .open
        .map(|open| open.tags)
        .unwrap_or_default();
        if !self.allow_open
            || t.current.is_none()
            || tags.is_empty()
            || age < time::Duration::ZERO
            || age > time::Duration::minutes(1)
        {
            self.complete = true;
            return Ok(());
        }
        // The write boundary repeats these checks under the actual privacy and
        // lifecycle locks. An unavailable gate drops the open, never a close.
        // Mark complete before awaiting gates/insert: a timeout here must not
        // turn a missed open into a retry or duplicate an uncertain insert.
        self.complete = true;
        let voice = store
            .is_attendance_voice_enabled(t.guild)
            .await
            .unwrap_or(false);
        let suppressed = if voice {
            store
                .is_attendance_suppressed(t.guild, t.member)
                .await
                .unwrap_or(true)
        } else {
            true
        };
        let age = OffsetDateTime::now_utc() - t.observed_at;
        if !voice
            || suppressed
            || !may_open()
            || age < time::Duration::ZERO
            || age > time::Duration::minutes(1)
        {
            return Ok(());
        }
        if let Some(channel) = t.current {
            // An uncertain insert is never retried: missed opens stay missed.
            let _ = store
                .insert_attendance_session(NewAttendanceSession {
                    guild_id: t.guild.to_string(),
                    member_id: t.member.to_string(),
                    channel_id: channel.to_string(),
                    channel_name: t.current_name.clone().unwrap_or_default(),
                    is_bot: t.is_bot,
                    afk: t.afk_channel == Some(channel),
                    joined_at: t.observed_at,
                    event_keys: tags,
                })
                .await;
        }
        Ok(())
    }
}

/// Start one transition and return its continuation, even on a timeout. The
/// caller must retain unfinished work and resume it before consuming later
/// attendance frames. Flag-off and same-channel frames touch no store.
pub async fn record_transition(
    store: &impl AttendanceStore,
    transition: VoiceTransition,
) -> AttendanceTransition {
    let mut work = AttendanceTransition::new(transition);
    work.turn(store).await;
    work
}

/// Epoch millis of an instant. Public: the binary-only read verb builds its
/// projection rows through `two_bot::event_attendance`.
pub fn millis_of(instant: OffsetDateTime) -> i64 {
    instant.unix_timestamp().saturating_mul(1_000) + i64::from(instant.millisecond())
}

/// Instant of epoch millis, clamped to the epoch on overflow. Public: the
/// binary-only read verb renders window bounds through
/// `two_bot::event_attendance`.
pub fn datetime_of(millis: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

// ---------------------------------------------------------------------------
// Read projection (§4 shape)
// ---------------------------------------------------------------------------

/// One stored session row for the read projection, in epoch millis.
/// Public: the binary builds these from stored rows for the projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMs {
    pub member_id: Snowflake,
    pub channel_id: Snowflake,
    pub channel_name: String,
    pub name_observed_ns: i128,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_ms: i64,
    pub left_ms: Option<i64>,
}

/// Why the attendance read refuses. Both map to existing refusal codes only.
/// Public: the binary matches on it to pick the refusal code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttendanceRefusal {
    /// No mapped window, or a mapped window with no tagged sessions: the
    /// read answers `not_found` (404, not retryable) by construction, which
    /// is also the pre-enable answer since windows only persist after the
    /// logger ships.
    NoRecord,
    /// The projection blew the relayed-answer budget. The receiver answers
    /// `internal` (500, retryable): a later read may be smaller.
    OverBudget,
}

// A failed close followed by a successful open can leave overlapping rows.
// Union per member, not per row: one member cannot earn two minutes or occupy
// two places in the peak for the same minute. Keep AFK eligibility separate.
fn union_intervals(mut intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    intervals.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (from, to) in intervals {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    merged
}

#[derive(serde::Serialize)]
struct AttendanceMember {
    discord_id: String,
    minutes: i64,
    sessions: u64,
    first_joined_at: String,
}

#[derive(serde::Serialize)]
struct AttendanceRoom<'a> {
    channel_id: String,
    name_at_close: &'a str,
}

#[derive(serde::Serialize)]
struct AttendanceResultWindow {
    starts_at: String,
    ends_at: String,
}

#[derive(serde::Serialize)]
struct AttendanceResult<'a> {
    v: u8,
    event_key: &'a str,
    window: AttendanceResultWindow,
    threshold_minutes: i64,
    peak_concurrent: i64,
    rooms: Vec<AttendanceRoom<'a>>,
    members: Vec<AttendanceMember>,
    r#final: bool,
}

/// Project the §4 attendance shape over stored sessions. Pure over plain
/// rows so the threshold/peak/final math pins without a database.
///
/// Rules: bots leave `members` and `rooms` out entirely; AFK sessions keep
/// their member rows but leave `peak_concurrent` out; below-threshold
/// members stay listed with their minutes; rooms carry the latest name the
/// logger saw (`name_at_close`) for channels with at least one human
/// session; still-open rows clamp at `min(now, window close)`; `final` is
/// true once the window plus grace has passed. Public: the binary-only
/// `event.attendance.read` verb projects through it.
pub fn project_attendance(
    event_key: &str,
    window: &AttendanceWindowMs,
    sessions: &[SessionMs],
    now_ms: i64,
) -> Result<Value, AttendanceRefusal> {
    if sessions.len() > two_bot_core::internal_action_store::MAX_ATTENDANCE_INPUT_SESSIONS
        || sessions
            .iter()
            .try_fold(0usize, |total, row| {
                total.checked_add(row.channel_name.len())
            })
            .is_none_or(|bytes| {
                bytes > two_bot_core::internal_action_store::MAX_ATTENDANCE_INPUT_NAME_BYTES
            })
    {
        return Err(AttendanceRefusal::OverBudget);
    }
    let (open_ms, close_ms) = window_open_ms(window.starts_ms, window.ends_ms);
    // Group bounded human observations by unsigned member id and input position.
    // Reuse the same tuple ordering and storage for room observations below.
    let mut observations: Vec<(Snowflake, usize)> = sessions
        .iter()
        .enumerate()
        .filter(|(_, row)| !row.is_bot)
        .map(|(index, row)| (row.member_id, index))
        .collect();
    if observations.is_empty() {
        return Err(AttendanceRefusal::NoRecord);
    }
    observations.sort_unstable();
    // Sweep events for the peak: (instant, delta), ends before starts.
    let mut sweep: Vec<(i64, i64)> = Vec::new();
    let wire_members: Vec<AttendanceMember> = observations
        .chunk_by(|a, b| a.0 == b.0)
        .map(|rows| {
            let mut intervals = Vec::new();
            let mut non_afk_intervals = Vec::new();
            let mut first_joined_ms = i64::MAX;
            for (_, index) in rows {
                let row = &sessions[*index];
                first_joined_ms = first_joined_ms.min(row.joined_ms);
                let end_ms = row.left_ms.unwrap_or(now_ms.min(close_ms));
                let from_ms = row.joined_ms.max(open_ms);
                let to_ms = end_ms.min(close_ms);
                if from_ms < to_ms {
                    intervals.push((from_ms, to_ms));
                    if !row.afk {
                        non_afk_intervals.push((from_ms, to_ms));
                    }
                }
            }
            let duration_ms: i64 = union_intervals(intervals)
                .into_iter()
                .map(|(from, to)| to - from)
                .sum();
            for (from, to) in union_intervals(non_afk_intervals) {
                sweep.push((from, 1));
                sweep.push((to, -1));
            }
            AttendanceMember {
                discord_id: rows[0].0.to_string(),
                minutes: duration_ms / 60_000,
                sessions: rows.len() as u64,
                first_joined_at: two_bot_core::funnel::format_iso_millis(first_joined_ms),
            }
        })
        .collect();
    sweep.sort_unstable();
    let mut peak_concurrent = 0i64;
    let mut running = 0i64;
    for (_, delta) in sweep {
        running += delta;
        peak_concurrent = peak_concurrent.max(running);
    }
    for (id, index) in &mut observations {
        *id = sessions[*index].channel_id;
    }
    observations.sort_unstable();
    let wire_rooms: Vec<AttendanceRoom<'_>> = observations
        .chunk_by(|a, b| a.0 == b.0)
        .map(|rows| {
            let mut latest = &sessions[rows[0].1];
            for (_, index) in &rows[1..] {
                let row = &sessions[*index];
                // Equal stamps keep the last input observation.
                if row.name_observed_ns >= latest.name_observed_ns {
                    latest = row;
                }
            }
            AttendanceRoom {
                channel_id: latest.channel_id.to_string(),
                name_at_close: &latest.channel_name,
            }
        })
        .collect();
    let value = serde_json::to_value(AttendanceResult {
        v: 1,
        event_key,
        window: AttendanceResultWindow {
            starts_at: two_bot_core::funnel::format_iso_millis(window.starts_ms),
            ends_at: two_bot_core::funnel::format_iso_millis(window.ends_ms),
        },
        threshold_minutes: ATTENDANCE_THRESHOLD_MINUTES,
        peak_concurrent,
        rooms: wire_rooms,
        members: wire_members,
        r#final: now_ms >= close_ms,
    })
    .map_err(|_| AttendanceRefusal::OverBudget)?;
    let bytes = serde_json::to_vec(&value).map_err(|_| AttendanceRefusal::OverBudget)?;
    if bytes.len() > MAX_ATTENDANCE_RESULT_BYTES {
        return Err(AttendanceRefusal::OverBudget);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interval_union_compacts_nested_touching_and_disjoint_spans() {
        assert!(union_intervals(Vec::new()).is_empty());
        assert_eq!(union_intervals(vec![(10, 20)]), vec![(10, 20)]);
        assert_eq!(
            union_intervals(vec![(50, 60), (20, 30), (10, 20), (12, 15), (10, 20)]),
            vec![(10, 30), (50, 60)]
        );
        assert_eq!(
            union_intervals(vec![(5, 10), (1, 100), (10, 50), (100, 101)]),
            vec![(1, 101)]
        );
    }

    const MIN: i64 = 60_000;

    #[test]
    fn room_name_follows_observation_order_not_join_or_credit_order() {
        let event = window("event", 0, 60 * MIN);
        // A closes after B despite joining first. Its credit was separately
        // stopped before B's join; neither voice stamp orders the new name.
        let mut a = session(1, 7, "Renamed", false, false, 0, Some(10 * MIN));
        a.name_observed_ns = i128::from(40 * MIN) * 1_000_000;
        let b = session(2, 7, "Old", false, false, 20 * MIN, Some(25 * MIN));
        for rows in [vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]] {
            let result = project_attendance("event", &event, &rows, 80 * MIN).unwrap();
            assert_eq!(result["rooms"][0]["name_at_close"], "Renamed");
        }
        // An open row's join is a real later observation too. Missing metadata
        // at close retains the older persisted observation, tested in the store.
        let open = session(3, 7, "Newest", false, false, 45 * MIN, None);
        let result = project_attendance("event", &event, &[a, b, open], 50 * MIN).unwrap();
        assert_eq!(result["rooms"][0]["name_at_close"], "Newest");
    }

    #[test]
    fn room_observation_ties_keep_last_human_name_in_channel_order() {
        let event = window("event", 0, 60 * MIN);
        let first = session(1, 9, "First", false, false, 0, Some(MIN));
        let mut last = session(2, 9, "Last", false, false, 0, Some(MIN));
        last.name_observed_ns = first.name_observed_ns;
        let low = session(3, 7, "Lower", false, false, 0, Some(MIN));
        let bot_rename = session(4, 9, "Bot name", true, false, MIN, Some(2 * MIN));
        let bot_only = session(5, 5, "Bot room", true, false, MIN, Some(2 * MIN));
        let result = project_attendance(
            "event",
            &event,
            &[
                first.clone(),
                low.clone(),
                last.clone(),
                bot_rename,
                bot_only,
            ],
            80 * MIN,
        )
        .unwrap();
        assert_eq!(
            result["rooms"],
            json!([
                {"channel_id": "7", "name_at_close": "Lower"},
                {"channel_id": "9", "name_at_close": "Last"},
            ])
        );
        let reversed = project_attendance("event", &event, &[last, low, first], 80 * MIN).unwrap();
        assert_eq!(reversed["rooms"][1]["name_at_close"], "First");
    }

    #[test]
    fn rooms_keep_unsigned_channel_order_and_latest_names_across_interleaved_rows() {
        let event = window("event", 0, 60 * MIN);
        let mut latest = session(1, u64::MAX, "Latest", false, false, 0, Some(MIN));
        latest.name_observed_ns = i128::MAX;
        let rows = [
            session(2, 10, "Ten", false, false, 0, Some(MIN)),
            latest,
            session(3, 2, "Two", false, true, 0, Some(MIN)),
            session(4, u64::MAX, "Older", false, false, 0, Some(MIN)),
            session(5, 10, "Bot rename", true, false, 2 * MIN, Some(3 * MIN)),
        ];
        let result = project_attendance("event", &event, &rows, 80 * MIN).unwrap();
        assert_eq!(
            result["rooms"],
            json!([
                {"channel_id": "2", "name_at_close": "Two"},
                {"channel_id": "10", "name_at_close": "Ten"},
                {"channel_id": u64::MAX.to_string(), "name_at_close": "Latest"},
            ])
        );
    }

    #[test]
    fn interleaved_members_keep_unsigned_order_counts_and_independent_interval_unions() {
        let event = window("event", 0, 60 * MIN);
        let rows = [
            session(u64::MAX, 7, "Lobby", false, false, 5 * MIN, Some(15 * MIN)),
            session(2, 7, "Lobby", false, false, 30 * MIN, Some(30 * MIN)),
            session(u64::MAX, 7, "Lobby", false, true, 0, Some(10 * MIN)),
            session(10, 7, "Lobby", false, false, 0, Some(20 * MIN)),
            session(u64::MAX, 7, "Lobby", false, false, 10 * MIN, Some(20 * MIN)),
            session(1, 7, "Lobby", true, false, 0, Some(20 * MIN)),
        ];
        let result = project_attendance("event", &event, &rows, 80 * MIN).unwrap();
        assert_eq!(
            result["members"],
            json!([
                {"discord_id": "2", "minutes": 0, "sessions": 1,
                 "first_joined_at": two_bot_core::funnel::format_iso_millis(30 * MIN)},
                {"discord_id": "10", "minutes": 20, "sessions": 1,
                 "first_joined_at": two_bot_core::funnel::format_iso_millis(0)},
                {"discord_id": u64::MAX.to_string(), "minutes": 20, "sessions": 3,
                 "first_joined_at": two_bot_core::funnel::format_iso_millis(0)},
            ])
        );
        assert_eq!(result["peak_concurrent"], 2);
        let mut reversed = rows.to_vec();
        reversed.reverse();
        let reversed = project_attendance("event", &event, &reversed, 80 * MIN).unwrap();
        assert_eq!(result["members"], reversed["members"]);
        assert_eq!(result["peak_concurrent"], reversed["peak_concurrent"]);
    }

    #[test]
    fn raw_history_budget_refuses_before_aggregation_even_when_output_would_be_tiny() {
        let row = session(1, 7, "Lobby", false, false, 0, Some(MIN));
        let limit = two_bot_core::internal_action_store::MAX_ATTENDANCE_INPUT_SESSIONS;
        let mut rows = vec![row; limit];
        let event = window("event", 0, MIN);
        let at_limit = project_attendance("event", &event, &rows, 20 * MIN).unwrap();
        assert_eq!(at_limit["members"].as_array().unwrap().len(), 1);
        assert_eq!(at_limit["members"][0]["sessions"], limit);
        rows.push(rows[0].clone());
        assert_eq!(
            project_attendance("event", &event, &rows, 20 * MIN),
            Err(AttendanceRefusal::OverBudget)
        );
    }

    #[test]
    fn partial_minutes_accumulate_across_sessions_before_rounding() {
        let rows = [
            session(1, 7, "Lobby", false, false, 0, Some(30_000)),
            session(1, 7, "Lobby", false, false, MIN, Some(MIN + 30_000)),
        ];
        let result =
            project_attendance("event", &window("event", 0, 2 * MIN), &rows, 3 * MIN).unwrap();
        assert_eq!(result["members"][0]["minutes"], 1);
        assert_eq!(result["members"][0]["sessions"], 2);
        assert_eq!(result["peak_concurrent"], 1);
    }

    #[test]
    fn overlapping_sessions_for_one_member_deduplicate_duration_and_peak() {
        // A failed move-close can leave both rows open until the eventual leave.
        let rows = [
            session(1, 7, "Lobby", false, false, 0, Some(20 * MIN)),
            session(1, 9, "Stage", false, false, 10 * MIN, Some(20 * MIN)),
        ];
        let result = project_attendance("event", &window("event", 0, 30 * MIN), &rows, 60 * MIN)
            .expect("fits the budget");
        assert_eq!(result["members"][0]["minutes"], 20);
        assert_eq!(result["members"][0]["sessions"], 2);
        assert_eq!(result["peak_concurrent"], 1);
        assert_eq!(result["rooms"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn non_afk_interval_union_does_not_bridge_afk_only_time() {
        let rows = [
            session(1, 7, "Lobby", false, false, 0, Some(5 * MIN)),
            session(1, 9, "AFK", false, true, 3 * MIN, Some(18 * MIN)),
            session(1, 7, "Lobby", false, false, 15 * MIN, Some(20 * MIN)),
            session(2, 7, "Lobby", false, false, 10 * MIN, Some(15 * MIN)),
        ];
        let result = project_attendance("event", &window("event", 0, 30 * MIN), &rows, 60 * MIN)
            .expect("fits the budget");
        assert_eq!(result["members"][0]["minutes"], 20);
        assert_eq!(result["members"][0]["sessions"], 3);
        assert_eq!(result["members"][1]["minutes"], 5);
        assert_eq!(
            result["peak_concurrent"], 1,
            "AFK time never fills a peak gap"
        );
    }

    fn window(event_key: &str, starts_ms: i64, ends_ms: i64) -> AttendanceWindowMs {
        AttendanceWindowMs {
            event_key: event_key.to_owned(),
            starts_ms,
            ends_ms,
        }
    }

    fn row(id: i64, channel: Snowflake, joined_ms: i64, keys: &[&str]) -> OpenRow {
        OpenRow {
            id,
            channel_id: Some(channel),
            joined_ms,
            event_keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    fn session(
        member: Snowflake,
        channel: Snowflake,
        name: &str,
        is_bot: bool,
        afk: bool,
        joined_ms: i64,
        left_ms: Option<i64>,
    ) -> SessionMs {
        SessionMs {
            member_id: member,
            channel_id: channel,
            channel_name: name.to_owned(),
            name_observed_ns: i128::from(left_ms.unwrap_or(joined_ms)) * 1_000_000,
            is_bot,
            afk,
            joined_ms,
            left_ms,
        }
    }

    #[test]
    fn same_channel_frames_plan_nothing() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let plan = plan_transition(&windows, &[], Some(7), Some(7), 30 * MIN);
        assert_eq!(plan, TransitionPlan::default());
    }

    #[test]
    fn outside_windows_with_no_state_plans_nothing() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        // Before the early grace: no open rows, no open window, no write.
        let plan = plan_transition(&windows, &[], None, Some(7), -16 * MIN);
        assert_eq!(plan, TransitionPlan::default());
        let plan = plan_transition(&windows, &[], Some(7), None, 200 * MIN);
        assert_eq!(plan, TransitionPlan::default());
    }

    #[test]
    fn join_inside_window_opens_tagged_with_open_windows() {
        let windows = vec![
            window("early-show", 0, 60 * MIN),
            window("late-show", 30 * MIN, 120 * MIN),
        ];
        // 45 min sits inside both windows (with grace): dual-tag.
        let plan = plan_transition(&windows, &[], None, Some(7), 45 * MIN);
        assert!(plan.closes.is_empty());
        assert_eq!(
            plan.open,
            Some(OpenPlan {
                tags: vec!["early-show".to_owned(), "late-show".to_owned()],
            })
        );
    }

    #[test]
    fn move_closes_previous_and_opens_current() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let open = vec![row(1, 7, 10 * MIN, &["game-night"])];
        let plan = plan_transition(&windows, &open, Some(7), Some(9), 20 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 20 * MIN,
                tag_union: vec!["game-night".to_owned()],
            }]
        );
        assert_eq!(
            plan.open,
            Some(OpenPlan {
                tags: vec!["game-night".to_owned()],
            })
        );
    }

    #[test]
    fn close_after_window_still_finalizes_without_opening() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let open = vec![row(1, 7, 10 * MIN, &["game-night"])];
        // Leave lands after the late grace: the row still closes (left stamps
        // now, inside the window's own span so its tags hold), but with no
        // current channel nothing opens.
        let plan = plan_transition(&windows, &open, Some(7), None, 200 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 200 * MIN,
                tag_union: vec!["game-night".to_owned()],
            }]
        );
        assert_eq!(plan.open, None);
    }

    #[test]
    fn stale_row_from_a_closed_window_closes_zero_length() {
        let windows = vec![
            window("old-show", 0, 60 * MIN),
            window("new-show", 1_000 * MIN, 1_060 * MIN),
        ];
        // The missed close from the old show finalizes during the new show:
        // the row closes at its own join (zero length, original tags only),
        // so it can never inflate the new show's minutes.
        let open = vec![row(1, 7, 10 * MIN, &["old-show"])];
        // A fresh join has no previous channel to prove continuity with the row.
        let plan = plan_transition(&windows, &open, None, Some(9), 1_010 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 10 * MIN,
                tag_union: vec!["old-show".to_owned()],
            }]
        );
        assert_eq!(
            plan.open,
            Some(OpenPlan {
                tags: vec!["new-show".to_owned()],
            })
        );
    }

    #[test]
    fn continuous_move_into_a_later_window_preserves_earned_attendance() {
        // A is closed, so the store now returns only B. The observed previous
        // channel still proves that the member was continuously in the old room.
        let windows = vec![window("b", 60 * MIN, 120 * MIN)];
        let open = vec![row(1, 7, 0, &["a"])];
        let plan = plan_transition(&windows, &open, Some(7), Some(9), 80 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 80 * MIN,
                tag_union: vec!["a".to_owned(), "b".to_owned()],
            }]
        );
        assert_eq!(
            plan.open,
            Some(OpenPlan {
                tags: vec!["b".to_owned()]
            })
        );
        // A stale row in a different room has no such continuity evidence.
        let foreign = vec![row(1, 3, 0, &["a"])];
        let stale = plan_transition(&windows, &foreign, Some(7), Some(9), 80 * MIN);
        assert_eq!(stale.closes[0].left_ms, 0);
        assert_eq!(stale.closes[0].tag_union, vec!["a".to_owned()]);
    }

    #[test]
    fn fresh_same_event_rejoin_and_foreign_leave_do_not_bridge_failed_closes() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let open = vec![row(1, 7, 10 * MIN, &["game-night"])];
        for (previous, current) in [(None, Some(7)), (Some(9), None), (None, Some(9))] {
            let plan = plan_transition(&windows, &open, previous, current, 40 * MIN);
            assert_eq!(plan.closes[0].left_ms, 10 * MIN);
            assert_eq!(plan.closes[0].tag_union, ["game-night"]);
        }
        let continuous = plan_transition(&windows, &open, Some(7), Some(9), 40 * MIN);
        assert_eq!(continuous.closes[0].left_ms, 40 * MIN);
    }

    #[test]
    fn close_widens_tags_to_later_overlapping_windows() {
        let windows = vec![
            window("early-show", 0, 60 * MIN),
            window("late-show", 30 * MIN, 120 * MIN),
        ];
        // Joined during the early show only, leaves during the late show: the
        // closed row tags both, since the session overlapped both windows.
        let open = vec![row(1, 7, 10 * MIN, &["early-show"])];
        let plan = plan_transition(&windows, &open, Some(7), None, 90 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 90 * MIN,
                tag_union: vec!["early-show".to_owned(), "late-show".to_owned()],
            }]
        );
        assert_eq!(plan.open, None);
    }

    #[test]
    fn leave_with_no_open_row_plans_nothing() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let plan = plan_transition(&windows, &[], Some(7), None, 200 * MIN);
        assert_eq!(plan, TransitionPlan::default());
    }

    #[test]
    fn full_shape_counts_peak_and_final() {
        // Window [60, 120] min; grace opens at 45, closes at 135.
        let event = window("game-night", 60 * MIN, 120 * MIN);
        let sessions = vec![
            // 70 minutes in the lobby, two sessions (move out and back).
            session(11, 7, "lobby", false, false, 50 * MIN, Some(90 * MIN)),
            session(11, 7, "lobby", false, false, 95 * MIN, Some(130 * MIN)),
            // 5 minutes only: stays listed, below threshold.
            session(12, 7, "lobby", false, false, 100 * MIN, Some(105 * MIN)),
            // AFK the whole event: listed as a member, out of the peak.
            session(13, 9, "afk-room", false, true, 50 * MIN, Some(130 * MIN)),
            // Bot: out of members, rooms and peak entirely.
            session(99, 7, "lobby", true, false, 50 * MIN, Some(130 * MIN)),
        ];
        let value = project_attendance("game-night", &event, &sessions, 100 * MIN)
            .expect("fits the budget");
        assert_eq!(value["v"], json!(1));
        assert_eq!(value["event_key"], json!("game-night"));
        assert_eq!(value["threshold_minutes"], json!(10));
        assert_eq!(value["final"], json!(false));
        // Lobby overlap: [50,90] and [95,130] clamp to [45,135] → 75 min
        // over 2 sessions, first seen at 50.
        let members = value["members"].as_array().expect("members array");
        assert_eq!(members.len(), 3);
        assert_eq!(members[0]["discord_id"], json!("11"));
        assert_eq!(members[0]["minutes"], json!(75));
        assert_eq!(members[0]["sessions"], json!(2));
        assert!(members[1]["minutes"].as_i64().expect("minutes") < 10);
        // Peak: members 11 and 12 overlap 100-105; the AFK occupant and the
        // bot never count.
        assert_eq!(value["peak_concurrent"], json!(2));
        let rooms = value["rooms"].as_array().expect("rooms array");
        assert_eq!(rooms.len(), 2);
        assert_eq!(
            value["window"]["starts_at"],
            json!("1970-01-01T01:00:00.000Z")
        );
        assert_eq!(
            value["window"]["ends_at"],
            json!("1970-01-01T02:00:00.000Z")
        );
        assert_eq!(
            value,
            json!({
                "v": 1,
                "event_key": "game-night",
                "window": {
                    "starts_at": "1970-01-01T01:00:00.000Z",
                    "ends_at": "1970-01-01T02:00:00.000Z",
                },
                "threshold_minutes": 10,
                "peak_concurrent": 2,
                "rooms": [
                    {"channel_id": "7", "name_at_close": "lobby"},
                    {"channel_id": "9", "name_at_close": "afk-room"},
                ],
                "members": [
                    {"discord_id": "11", "minutes": 75, "sessions": 2,
                     "first_joined_at": "1970-01-01T00:50:00.000Z"},
                    {"discord_id": "12", "minutes": 5, "sessions": 1,
                     "first_joined_at": "1970-01-01T01:40:00.000Z"},
                    {"discord_id": "13", "minutes": 80, "sessions": 1,
                     "first_joined_at": "1970-01-01T00:50:00.000Z"},
                ],
                "final": false,
            })
        );
    }

    #[test]
    fn open_rows_clamp_at_now_before_close_and_close_after() {
        let event = window("game-night", 60 * MIN, 120 * MIN);
        let sessions = vec![session(11, 7, "lobby", false, false, 100 * MIN, None)];
        // Mid-window: open row clamps at now.
        let value = project_attendance("game-night", &event, &sessions, 110 * MIN)
            .expect("fits the budget");
        assert_eq!(value["final"], json!(false));
        assert_eq!(value["members"][0]["minutes"], json!(10));
        // After the late grace: open row clamps at the window close, final.
        let value = project_attendance("game-night", &event, &sessions, 200 * MIN)
            .expect("fits the budget");
        assert_eq!(value["final"], json!(true));
        assert_eq!(value["members"][0]["minutes"], json!(35));
    }

    #[test]
    fn no_human_sessions_is_no_record() {
        let event = window("game-night", 60 * MIN, 120 * MIN);
        let sessions = vec![session(
            99,
            7,
            "lobby",
            true,
            false,
            70 * MIN,
            Some(80 * MIN),
        )];
        assert_eq!(
            project_attendance("game-night", &event, &sessions, 200 * MIN),
            Err(AttendanceRefusal::NoRecord)
        );
        assert_eq!(
            project_attendance("game-night", &event, &[], 200 * MIN),
            Err(AttendanceRefusal::NoRecord)
        );
    }

    #[test]
    fn members_sort_by_discord_id_and_rooms_by_channel() {
        let event = window("game-night", 0, 60 * MIN);
        let sessions = vec![
            session(30, 9, "b-room", false, false, 10 * MIN, Some(20 * MIN)),
            session(20, 7, "a-room", false, false, 10 * MIN, Some(20 * MIN)),
        ];
        let value = project_attendance("game-night", &event, &sessions, 200 * MIN)
            .expect("fits the budget");
        let members = value["members"].as_array().expect("members array");
        assert_eq!(members[0]["discord_id"], json!("20"));
        assert_eq!(members[1]["discord_id"], json!("30"));
        let rooms = value["rooms"].as_array().expect("rooms array");
        assert_eq!(rooms[0]["channel_id"], json!("7"));
        assert_eq!(rooms[1]["channel_id"], json!("9"));
    }

    #[test]
    fn result_budget_counts_serialized_utf8_and_reserves_the_envelope() {
        let event = window("event", 0, MIN);
        let mut rows = vec![session(1, 7, "", false, false, 0, Some(MIN))];
        let empty = project_attendance("event", &event, &rows, 20 * MIN).unwrap();
        let spare = MAX_ATTENDANCE_RESULT_BYTES - serde_json::to_vec(&empty).unwrap().len();
        rows[0].channel_name = "x".repeat(spare);
        let exact = project_attendance("event", &event, &rows, 20 * MIN).unwrap();
        assert_eq!(
            serde_json::to_vec(&exact).unwrap().len(),
            MAX_ATTENDANCE_RESULT_BYTES
        );
        rows[0].channel_name.push('x');
        assert_eq!(
            project_attendance("event", &event, &rows, 20 * MIN),
            Err(AttendanceRefusal::OverBudget)
        );
        // Escaping counts too, and the parsed value preserves the original text.
        let escaped = "\"\\\n\u{0000}界";
        rows[0].channel_name = escaped.to_owned();
        let value = project_attendance("event", &event, &rows, 20 * MIN).unwrap();
        assert_eq!(value["rooms"][0]["name_at_close"], json!(escaped));
        rows[0].channel_name = "\"".repeat(spare / 2);
        rows[0].channel_name.push_str(&"x".repeat(spare % 2));
        let exact = project_attendance("event", &event, &rows, 20 * MIN).unwrap();
        assert_eq!(
            serde_json::to_vec(&exact).unwrap().len(),
            MAX_ATTENDANCE_RESULT_BYTES
        );
        rows[0].channel_name.push('x');
        assert_eq!(
            project_attendance("event", &event, &rows, 20 * MIN),
            Err(AttendanceRefusal::OverBudget)
        );
        // A character count would accept this, but UTF-8 bytes exceed the cap.
        rows[0].channel_name = "界".repeat(spare / 2);
        assert!(rows[0].channel_name.chars().count() < spare);
        assert_eq!(
            project_attendance("event", &event, &rows, 20 * MIN),
            Err(AttendanceRefusal::OverBudget)
        );
    }

    #[test]
    fn grace_close_is_the_exact_finality_boundary() {
        let event = window("event", 0, MIN);
        let rows = vec![session(1, 7, "Lobby", false, false, 0, None)];
        let close = MIN + ATTENDANCE_GRACE_MS;
        let before = project_attendance("event", &event, &rows, close - 1).unwrap();
        let at = project_attendance("event", &event, &rows, close).unwrap();
        assert_eq!(before["final"], false);
        assert_eq!(at["final"], true);
        assert_eq!(at["members"][0]["minutes"], 16);
    }

    #[test]
    fn touching_sessions_do_not_double_the_peak_and_latest_room_name_wins() {
        let event = window("event", 0, 10 * MIN);
        let rows = [
            session(2, 7, "Renamed", false, false, 5 * MIN, Some(10 * MIN)),
            session(1, 7, "Original", false, false, 0, Some(5 * MIN)),
        ];
        let result = project_attendance("event", &event, &rows, 30 * MIN).unwrap();
        assert_eq!(result["peak_concurrent"], 1);
        assert_eq!(result["rooms"][0]["name_at_close"], "Renamed");
        assert_eq!(result["members"][0]["minutes"], 5);
        assert_eq!(result["members"][1]["minutes"], 5);
    }

    /// Fake store for the capture-gate tests: one open window, an optional
    /// seeded open row, and counters for opens and closes.
    struct GateStore {
        suppressed: bool,
        voice_enabled: bool,
        fail_gate: bool,
        seed_open: bool,
        opens: std::sync::Mutex<u64>,
        closes: std::sync::Mutex<u64>,
    }

    impl GateStore {
        fn open(suppressed: bool, voice_enabled: bool) -> Self {
            Self {
                suppressed,
                voice_enabled,
                fail_gate: false,
                seed_open: false,
                opens: std::sync::Mutex::new(0),
                closes: std::sync::Mutex::new(0),
            }
        }
    }

    impl AttendanceStore for GateStore {
        type Error = std::convert::Infallible;

        async fn attendance_windows(
            &self,
            _guild: Snowflake,
            _member: Snowflake,
            _previous: Option<Snowflake>,
            _observed_at: OffsetDateTime,
        ) -> Result<Vec<AttendanceWindow>, Self::Error> {
            let now = OffsetDateTime::now_utc();
            Ok(vec![AttendanceWindow {
                event_key: "game-night".to_owned(),
                starts_at: now - time::Duration::hours(1),
                ends_at: now + time::Duration::hours(1),
            }])
        }

        async fn open_attendance_sessions(
            &self,
            _guild: Snowflake,
            _member: Snowflake,
        ) -> Result<Vec<AttendanceSession>, Self::Error> {
            if !self.seed_open {
                return Ok(Vec::new());
            }
            Ok(vec![AttendanceSession {
                id: 1,
                member_id: "11".to_owned(),
                channel_id: "7".to_owned(),
                channel_name: "lobby".to_owned(),
                name_observed_at: OffsetDateTime::now_utc() - time::Duration::minutes(5),
                is_bot: false,
                afk: false,
                joined_at: OffsetDateTime::now_utc() - time::Duration::minutes(5),
                left_at: None,
                event_keys: vec!["game-night".to_owned()],
            }])
        }

        async fn insert_attendance_session(
            &self,
            _new: NewAttendanceSession,
        ) -> Result<i64, Self::Error> {
            *self.opens.lock().expect("opens lock") += 1;
            Ok(1)
        }

        async fn close_attendance_session(
            &self,
            _id: i64,
            _left_at: OffsetDateTime,
            _channel_name: Option<String>,
            _event_keys: &[String],
        ) -> Result<u64, Self::Error> {
            *self.closes.lock().expect("closes lock") += 1;
            Ok(1)
        }

        async fn attendance_sessions_for_event_key(
            &self,
            _guild: Snowflake,
            _event_key: &str,
        ) -> Result<Vec<AttendanceSession>, Self::Error> {
            Ok(Vec::new())
        }

        async fn is_attendance_suppressed(
            &self,
            _guild: Snowflake,
            _member: Snowflake,
        ) -> Result<bool, Self::Error> {
            if self.fail_gate {
                // `Infallible` cannot fail: model the failure as suppressed so
                // the test pins the fail-closed direction without a new error
                // type. The timeout arm is covered by the same branch.
                return Ok(true);
            }
            Ok(self.suppressed)
        }

        async fn is_attendance_voice_enabled(
            &self,
            _guild: Snowflake,
        ) -> Result<bool, Self::Error> {
            if self.fail_gate {
                return Ok(false);
            }
            Ok(self.voice_enabled)
        }
    }

    fn join_transition() -> VoiceTransition {
        VoiceTransition {
            guild: 1,
            member: 11,
            previous: None,
            current: Some(7),
            is_bot: false,
            afk_channel: None,
            previous_name: None,
            current_name: Some("lobby".to_owned()),
            observed_at: OffsetDateTime::now_utc(),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn postgres_logger_roundtrip_uses_room_persistence_and_capture_privacy() {
        use two_bot_core::{
            internal_action_store::InternalActionStore,
            scheduled_events::{EventStatus, ScheduledEvent},
            website_store::upsert_event,
        };
        use two_bot_cutover::voice_rooms::PgRoomStore;
        use two_bot_testsupport::TestDatabase;

        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            assert!(
                std::env::var("GITHUB_ACTIONS").is_err(),
                "CI requires the test database"
            );
            eprintln!("SKIP attendance logger DB roundtrip: TWO_TEST_DATABASE_URL is unset");
            return;
        };
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
        let pool = db.pool();
        let store = PgRoomStore::new(pool.clone());
        let guild: Snowflake = 10000000000000001;
        let member: Snowflake = 10000000000000002;
        let guild_id = guild.to_string();
        let member_id = member.to_string();
        let now = OffsetDateTime::now_utc();
        let stamp = |at| two_bot_core::funnel::format_iso_millis(millis_of(at));
        let observed = ScheduledEvent {
            id: "20000000000000001".to_owned(),
            name: "Game night".to_owned(),
            starts_at: stamp(now - time::Duration::minutes(10)),
            ends_at: Some(stamp(now + time::Duration::minutes(10))),
            channel_id: None,
            description: None,
            status: EventStatus::Active,
        };
        upsert_event(
            pool,
            &guild_id,
            &stamp(now - time::Duration::minutes(1)),
            &observed,
        )
        .await
        .unwrap();
        InternalActionStore::new(pool.clone())
            .put_event_key(&guild_id, "game-night", &observed.id)
            .await
            .unwrap();
        let mut transition = join_transition();
        transition.guild = guild;
        transition.member = member;
        transition.current = Some(7);
        transition.current_name = Some("Lobby".to_owned());
        record_transition(&store, transition.clone()).await;
        let rows = AttendanceStore::open_attendance_sessions(&store, guild, member)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel_id, "7");
        assert_eq!(rows[0].event_keys, ["game-night"]);

        transition.previous = Some(7);
        transition.previous_name = Some("Renamed lobby".to_owned());
        transition.current = Some(8);
        transition.current_name = Some("AFK".to_owned());
        transition.afk_channel = Some(8);
        record_transition(&store, transition.clone()).await;
        let rows = AttendanceStore::attendance_sessions_for_event_key(&store, guild, "game-night")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let closed = rows.iter().find(|row| row.channel_id == "7").unwrap();
        assert!(closed.left_at.is_some());
        assert_eq!(closed.channel_name, "Renamed lobby");
        assert!(rows.iter().find(|row| row.channel_id == "8").unwrap().afk);
        let open = AttendanceStore::open_attendance_sessions(&store, guild, member)
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].channel_id, "8");
        assert!(open[0].left_at.is_none());
        assert!(open[0].afk);

        two_bot_cutover::privacy::opt_out_member(
            &two_bot_cutover::CutoverDb::from_pool(pool.clone()),
            &guild_id,
            &member_id,
            "operator",
        )
        .await
        .unwrap();
        transition.previous = Some(8);
        transition.current = Some(9);
        record_transition(&store, transition).await;
        assert!(
            AttendanceStore::open_attendance_sessions(&store, guild, member)
                .await
                .unwrap()
                .is_empty()
        );
        let rows = AttendanceStore::attendance_sessions_for_event_key(&store, guild, "game-night")
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "suppression closes existing rows but opens no new row"
        );
        assert!(rows.iter().all(|row| row.left_at.is_some()));
        assert!(AttendanceStore::attendance_sessions_for_event_key(
            &store,
            guild + 1,
            "game-night"
        )
        .await
        .unwrap()
        .is_empty());
        let recovery_member = member + 1;
        record_transition(
            &store,
            VoiceTransition {
                guild,
                member: recovery_member,
                ..join_transition()
            },
        )
        .await;
        let page =
            crate::voice_rooms::RoomPersistence::attendance_recovery_page(&store, guild, None, 0)
                .await
                .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].member_id, recovery_member.to_string());
        assert_eq!(page[0].channel_id, "7");
        assert_eq!(
            crate::voice_rooms::RoomPersistence::attendance_recovery_page(
                &store,
                guild,
                Some(recovery_member),
                0,
            )
            .await
            .unwrap(),
            page,
        );
        assert!(
            crate::voice_rooms::RoomPersistence::attendance_recovery_page(
                &store,
                guild,
                Some(member),
                0,
            )
            .await
            .unwrap()
            .is_empty(),
            "a scoped adapter must not return the neighboring member's row",
        );
        let ids = [page[0].id];
        assert_eq!(
            crate::voice_rooms::RoomPersistence::recover_attendance_sessions(
                &store,
                guild + 1,
                &ids
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            crate::voice_rooms::RoomPersistence::recover_attendance_sessions(&store, guild, &ids)
                .await
                .unwrap(),
            1
        );
        assert!(
            crate::voice_rooms::RoomPersistence::attendance_recovery_page(&store, guild, None, 0)
                .await
                .unwrap()
                .is_empty()
        );
        let (joined_at, left_at, channel_name, event_keys): (
            OffsetDateTime,
            OffsetDateTime,
            String,
            Vec<String>,
        ) = sqlx::query_as(
            "SELECT joined_at, left_at, channel_name, event_keys FROM event_voice_sessions WHERE id = $1",
        )
        .bind(ids[0])
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(left_at, joined_at);
        assert_eq!(channel_name, "lobby");
        assert_eq!(event_keys, ["game-night"]);
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn open_records_when_gates_pass() {
        let store = GateStore::open(false, true);
        record_transition(&store, join_transition()).await;
        assert_eq!(*store.opens.lock().expect("opens lock"), 1);
    }

    #[tokio::test]
    async fn suppressed_member_opens_nothing() {
        let store = GateStore::open(true, true);
        record_transition(&store, join_transition()).await;
        assert_eq!(*store.opens.lock().expect("opens lock"), 0);
    }

    #[tokio::test]
    async fn voice_disabled_guild_opens_nothing() {
        let store = GateStore::open(false, false);
        record_transition(&store, join_transition()).await;
        assert_eq!(*store.opens.lock().expect("opens lock"), 0);
    }

    #[tokio::test]
    async fn gate_failure_fails_closed() {
        let mut store = GateStore::open(false, true);
        store.fail_gate = true;
        record_transition(&store, join_transition()).await;
        assert_eq!(*store.opens.lock().expect("opens lock"), 0);
    }

    #[tokio::test]
    async fn suppressed_leave_still_finalizes_open_row() {
        // Contract v2 §9: the gate drops opens, never closes — a suppressed
        // member's seeded row finalizes on leave with no new open.
        let mut store = GateStore::open(true, true);
        store.seed_open = true;
        record_transition(
            &store,
            VoiceTransition {
                previous: Some(7),
                current: None,
                current_name: None,
                ..join_transition()
            },
        )
        .await;
        assert_eq!(*store.opens.lock().expect("opens lock"), 0);
        assert_eq!(*store.closes.lock().expect("closes lock"), 1);
    }

    /// How one `FaultStore` statement behaves.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        Pass,
        Fail,
        /// Never resolves, so the statement bound must fire.
        Hang,
        /// A statement consumes most of the shared deadline.
        Delay,
    }

    /// What the logger wrote through a `FaultStore`.
    #[derive(Default)]
    struct Writes {
        calls: Vec<&'static str>,
        closes: Vec<(i64, OffsetDateTime, String, Vec<String>)>,
        inserts: Vec<NewAttendanceSession>,
        closed: std::collections::HashSet<i64>,
    }

    /// Fake store with a per-statement fault, configurable windows and open
    /// rows, and a record of every call and write.
    struct FaultStore {
        windows: Vec<AttendanceWindow>,
        open_rows: Vec<AttendanceSession>,
        windows_fault: Fault,
        open_fault: Fault,
        voice_fault: Fault,
        suppress_fault: Fault,
        close_fault: Fault,
        insert_fault: Fault,
        writes: std::sync::Mutex<Writes>,
    }

    impl FaultStore {
        /// One window spanning now, no open rows, every statement passing.
        fn new() -> Self {
            let now = OffsetDateTime::now_utc();
            Self {
                windows: vec![AttendanceWindow {
                    event_key: "game-night".to_owned(),
                    starts_at: now - time::Duration::hours(1),
                    ends_at: now + time::Duration::hours(1),
                }],
                open_rows: Vec::new(),
                windows_fault: Fault::Pass,
                open_fault: Fault::Pass,
                voice_fault: Fault::Pass,
                suppress_fault: Fault::Pass,
                close_fault: Fault::Pass,
                insert_fault: Fault::Pass,
                writes: std::sync::Mutex::new(Writes::default()),
            }
        }

        fn with_open_row(mut self, channel_id: &str, name: &str) -> Self {
            self.open_rows.push(AttendanceSession {
                id: 5,
                member_id: "11".to_owned(),
                channel_id: channel_id.to_owned(),
                channel_name: name.to_owned(),
                name_observed_at: OffsetDateTime::now_utc() - time::Duration::minutes(5),
                is_bot: false,
                afk: false,
                joined_at: OffsetDateTime::now_utc() - time::Duration::minutes(5),
                left_at: None,
                event_keys: vec!["game-night".to_owned()],
            });
            self
        }

        fn note(&self, call: &'static str) {
            self.writes.lock().expect("writes lock").calls.push(call);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.writes.lock().expect("writes lock").calls.clone()
        }

        async fn apply<T>(fault: Fault, value: T) -> Result<T, String> {
            match fault {
                Fault::Pass => Ok(value),
                Fault::Fail => Err("store down".to_owned()),
                Fault::Hang => std::future::pending().await,
                Fault::Delay => {
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    Ok(value)
                }
            }
        }
    }

    impl AttendanceStore for FaultStore {
        type Error = String;

        async fn attendance_windows(
            &self,
            _guild: Snowflake,
            _member: Snowflake,
            _previous: Option<Snowflake>,
            _observed_at: OffsetDateTime,
        ) -> Result<Vec<AttendanceWindow>, Self::Error> {
            self.note("windows");
            Self::apply(self.windows_fault, self.windows.clone()).await
        }

        async fn open_attendance_sessions(
            &self,
            _guild: Snowflake,
            _member: Snowflake,
        ) -> Result<Vec<AttendanceSession>, Self::Error> {
            self.note("open_rows");
            let rows = {
                let writes = self.writes.lock().expect("writes lock");
                self.open_rows
                    .iter()
                    .filter(|row| !writes.closed.contains(&row.id))
                    .take(two_bot_cutover::event_attendance::ATTENDANCE_RECOVERY_PAGE_SIZE)
                    .cloned()
                    .collect()
            };
            Self::apply(self.open_fault, rows).await
        }

        async fn insert_attendance_session(
            &self,
            new: NewAttendanceSession,
        ) -> Result<i64, Self::Error> {
            self.note("insert");
            self.writes.lock().expect("writes lock").inserts.push(new);
            Self::apply(self.insert_fault, 9).await
        }

        async fn close_attendance_session(
            &self,
            id: i64,
            left_at: OffsetDateTime,
            channel_name: Option<String>,
            event_keys: &[String],
        ) -> Result<u64, Self::Error> {
            self.note("close");
            self.writes.lock().expect("writes lock").closes.push((
                id,
                left_at,
                channel_name.unwrap_or_else(|| {
                    self.open_rows
                        .iter()
                        .find(|row| row.id == id)
                        .map(|row| row.channel_name.clone())
                        .unwrap_or_default()
                }),
                event_keys.to_vec(),
            ));
            let result = Self::apply(self.close_fault, 1).await;
            if result.is_ok() {
                self.writes.lock().expect("writes lock").closed.insert(id);
            }
            result
        }

        async fn attendance_sessions_for_event_key(
            &self,
            _guild: Snowflake,
            _event_key: &str,
        ) -> Result<Vec<AttendanceSession>, Self::Error> {
            Ok(Vec::new())
        }

        async fn is_attendance_suppressed(
            &self,
            _guild: Snowflake,
            _member: Snowflake,
        ) -> Result<bool, Self::Error> {
            self.note("suppressed");
            Self::apply(self.suppress_fault, false).await
        }

        async fn is_attendance_voice_enabled(
            &self,
            _guild: Snowflake,
        ) -> Result<bool, Self::Error> {
            self.note("voice");
            Self::apply(self.voice_fault, true).await
        }
    }

    fn move_transition() -> VoiceTransition {
        VoiceTransition {
            previous: Some(7),
            current: Some(9),
            previous_name: Some("lobby-renamed".to_owned()),
            current_name: Some("stage".to_owned()),
            ..join_transition()
        }
    }

    #[tokio::test]
    async fn flag_off_touches_no_store() {
        let store = FaultStore::new();
        record_transition(
            &store,
            VoiceTransition {
                enabled: false,
                ..join_transition()
            },
        )
        .await;
        assert!(store.calls().is_empty());
    }

    #[tokio::test]
    async fn same_channel_frame_touches_no_store() {
        let store = FaultStore::new();
        record_transition(
            &store,
            VoiceTransition {
                previous: Some(7),
                current: Some(7),
                ..join_transition()
            },
        )
        .await;
        assert!(store.calls().is_empty());
        // A leave from nowhere to nowhere is the same noop.
        record_transition(
            &store,
            VoiceTransition {
                previous: None,
                current: None,
                ..join_transition()
            },
        )
        .await;
        assert!(store.calls().is_empty());
    }

    #[tokio::test]
    async fn windows_read_error_skips_transition() {
        let mut store = FaultStore::new();
        store.windows_fault = Fault::Fail;
        record_transition(&store, join_transition()).await;
        assert_eq!(store.calls(), vec!["windows"]);
    }

    #[tokio::test(start_paused = true)]
    async fn windows_read_timeout_skips_transition() {
        let mut store = FaultStore::new();
        store.windows_fault = Fault::Hang;
        record_transition(&store, join_transition()).await;
        assert_eq!(store.calls(), vec!["windows"]);
    }

    #[tokio::test]
    async fn no_live_windows_still_checks_for_rows_to_finalize() {
        let mut store = FaultStore::new();
        store.windows.clear();
        record_transition(&store, join_transition()).await;
        assert_eq!(store.calls(), vec!["windows", "open_rows"]);
        let writes = store.writes.lock().expect("writes lock");
        assert!(writes.inserts.is_empty());
        assert!(writes.closes.is_empty());
    }

    #[tokio::test]
    async fn revocation_leaves_no_live_window_but_still_finalizes() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.windows.clear();
        record_transition(
            &store,
            VoiceTransition {
                previous: Some(7),
                current: None,
                ..join_transition()
            },
        )
        .await;
        assert_eq!(store.calls(), vec!["windows", "open_rows", "close"]);
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes.len(), 1);
        assert!(writes.inserts.is_empty());
    }

    #[tokio::test]
    async fn open_rows_read_error_skips_transition() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.open_fault = Fault::Fail;
        record_transition(&store, move_transition()).await;
        assert_eq!(store.calls(), vec!["windows", "open_rows"]);
    }

    #[tokio::test(start_paused = true)]
    async fn open_rows_read_timeout_skips_transition() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.open_fault = Fault::Hang;
        record_transition(&store, move_transition()).await;
        assert_eq!(store.calls(), vec!["windows", "open_rows"]);
    }

    #[tokio::test]
    async fn outside_every_window_with_no_open_row_writes_nothing() {
        let mut store = FaultStore::new();
        let now = OffsetDateTime::now_utc();
        store.windows = vec![AttendanceWindow {
            event_key: "future-show".to_owned(),
            starts_at: now + time::Duration::days(2),
            ends_at: now + time::Duration::days(2) + time::Duration::hours(1),
        }];
        record_transition(&store, join_transition()).await;
        // No open planned, so the gates are never consulted either.
        assert_eq!(store.calls(), vec!["windows", "open_rows"]);
    }

    #[tokio::test]
    async fn outside_window_leave_closes_open_row_without_gates() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        let now = OffsetDateTime::now_utc();
        store.windows = vec![AttendanceWindow {
            event_key: "game-night".to_owned(),
            starts_at: now - time::Duration::days(2),
            ends_at: now - time::Duration::days(2) + time::Duration::hours(1),
        }];
        record_transition(
            &store,
            VoiceTransition {
                previous: Some(7),
                current: None,
                previous_name: Some("lobby".to_owned()),
                current_name: None,
                ..join_transition()
            },
        )
        .await;
        assert_eq!(store.calls(), vec!["windows", "open_rows", "close"]);
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes[0].0, 5);
        assert_eq!(writes.closes[0].3, vec!["game-night".to_owned()]);
        assert!(writes.inserts.is_empty());
    }

    #[tokio::test]
    async fn voice_gate_error_drops_open_and_skips_suppression_read() {
        let mut store = FaultStore::new();
        store.voice_fault = Fault::Fail;
        record_transition(&store, join_transition()).await;
        assert_eq!(store.calls(), vec!["windows", "open_rows", "voice"]);
    }

    #[tokio::test(start_paused = true)]
    async fn voice_gate_timeout_drops_open() {
        let mut store = FaultStore::new();
        store.voice_fault = Fault::Hang;
        record_transition(&store, join_transition()).await;
        assert_eq!(store.calls(), vec!["windows", "open_rows", "voice"]);
    }

    #[tokio::test]
    async fn suppression_error_drops_open() {
        let mut store = FaultStore::new();
        store.suppress_fault = Fault::Fail;
        record_transition(&store, join_transition()).await;
        assert_eq!(
            store.calls(),
            vec!["windows", "open_rows", "voice", "suppressed"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn suppression_timeout_drops_open() {
        let mut store = FaultStore::new();
        store.suppress_fault = Fault::Hang;
        record_transition(&store, join_transition()).await;
        assert_eq!(
            store.calls(),
            vec!["windows", "open_rows", "voice", "suppressed"]
        );
    }

    #[tokio::test]
    async fn gate_failure_on_move_still_closes_previous_row() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.voice_fault = Fault::Fail;
        record_transition(&store, move_transition()).await;
        // The open is dropped fail-closed, but the old row is finalized.
        assert_eq!(
            store.calls(),
            vec!["windows", "open_rows", "close", "voice"]
        );
    }

    #[tokio::test]
    async fn delayed_processing_preserves_observed_join_and_leave_boundaries() {
        let store = FaultStore::new().with_open_row("7", "lobby");
        let observed_at = OffsetDateTime::now_utc() - time::Duration::seconds(20);
        record_transition(
            &store,
            VoiceTransition {
                observed_at,
                ..move_transition()
            },
        )
        .await;
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(millis_of(writes.closes[0].1), millis_of(observed_at));
        assert_eq!(writes.inserts[0].joined_at, observed_at);
    }

    #[tokio::test]
    async fn excessively_late_or_future_frames_finalize_but_never_backfill_opens() {
        for offset in [-120, 30] {
            let store = FaultStore::new().with_open_row("7", "lobby");
            let observed_at = OffsetDateTime::now_utc() + time::Duration::seconds(offset);
            record_transition(
                &store,
                VoiceTransition {
                    observed_at,
                    ..move_transition()
                },
            )
            .await;
            let writes = store.writes.lock().expect("writes lock");
            assert!(writes.inserts.is_empty());
            assert_eq!(writes.closes.len(), 1);
            assert_eq!(millis_of(writes.closes[0].1), millis_of(observed_at));
        }
    }

    #[tokio::test]
    async fn join_inserts_tagged_row_with_afk_and_bot_flags() {
        let store = FaultStore::new();
        record_transition(
            &store,
            VoiceTransition {
                is_bot: true,
                afk_channel: Some(7),
                ..join_transition()
            },
        )
        .await;
        assert_eq!(
            store.calls(),
            vec!["windows", "open_rows", "voice", "suppressed", "insert"]
        );
        let writes = store.writes.lock().expect("writes lock");
        let new = &writes.inserts[0];
        assert_eq!(new.guild_id, "1");
        assert_eq!(new.member_id, "11");
        assert_eq!(new.channel_id, "7");
        assert_eq!(new.channel_name, "lobby");
        assert!(new.is_bot);
        assert!(new.afk);
        assert_eq!(new.event_keys, vec!["game-night".to_owned()]);
    }

    #[tokio::test]
    async fn join_to_non_afk_channel_clears_afk_flag() {
        let store = FaultStore::new();
        record_transition(
            &store,
            VoiceTransition {
                afk_channel: Some(99),
                current_name: None,
                ..join_transition()
            },
        )
        .await;
        let writes = store.writes.lock().expect("writes lock");
        assert!(!writes.inserts[0].afk);
        // A missing live name stores an empty one rather than failing.
        assert_eq!(writes.inserts[0].channel_name, "");
    }

    #[tokio::test]
    async fn move_closes_with_live_previous_name_then_inserts() {
        let store = FaultStore::new().with_open_row("7", "lobby");
        record_transition(&store, move_transition()).await;
        assert_eq!(
            store.calls(),
            vec![
                "windows",
                "open_rows",
                "close",
                "voice",
                "suppressed",
                "insert"
            ]
        );
        let writes = store.writes.lock().expect("writes lock");
        // Row channel equals the previous channel: the renamed live name wins.
        assert_eq!(writes.closes[0].2, "lobby-renamed");
        assert_eq!(writes.inserts[0].channel_id, "9");
        assert_eq!(writes.inserts[0].channel_name, "stage");
    }

    #[tokio::test]
    async fn close_keeps_known_name_when_matching_channel_metadata_is_missing() {
        let store = FaultStore::new().with_open_row("7", "known-room");
        record_transition(
            &store,
            VoiceTransition {
                previous_name: None,
                ..move_transition()
            },
        )
        .await;
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes.len(), 1);
        assert_eq!(writes.closes[0].2, "known-room");
    }

    #[tokio::test]
    async fn close_keeps_stored_name_when_row_channel_is_not_previous() {
        // The row sits in channel 3 while the frame says the member left 7
        // (a missed close earlier): the live name must not overwrite it.
        let store = FaultStore::new().with_open_row("3", "old-room");
        record_transition(&store, move_transition()).await;
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes[0].2, "old-room");
    }

    #[tokio::test]
    async fn close_keeps_stored_name_when_row_channel_id_is_corrupt() {
        let store = FaultStore::new().with_open_row("not-a-snowflake", "old-room");
        record_transition(&store, move_transition()).await;
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes.len(), 1);
        assert_eq!(writes.closes[0].2, "old-room");
    }

    #[tokio::test]
    async fn close_error_blocks_open_and_retries_original_evidence() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.close_fault = Fault::Fail;
        let mut pending = record_transition(&store, move_transition()).await;
        assert!(!pending.is_complete());
        {
            let writes = store.writes.lock().expect("writes lock");
            assert_eq!(writes.closes.len(), 1);
            assert!(writes.inserts.is_empty());
        }
        store.close_fault = Fault::Pass;
        pending.turn(&store).await;
        assert!(pending.is_complete());
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes.len(), 2);
        assert_eq!(writes.closes[0], writes.closes[1]);
        assert_eq!(writes.inserts.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn close_timeout_blocks_open_with_one_total_deadline() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.close_fault = Fault::Hang;
        let start = tokio::time::Instant::now();
        let mut pending = record_transition(&store, move_transition()).await;
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS)
        );
        assert!(!pending.is_complete());
        {
            let writes = store.writes.lock().expect("writes lock");
            assert_eq!(writes.closes.len(), 1);
            assert!(writes.inserts.is_empty());
        }
        store.close_fault = Fault::Pass;
        pending.turn(&store).await;
        assert!(pending.is_complete());
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes[0], writes.closes[1]);
        assert_eq!(writes.inserts.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_slow_reads_share_the_same_deadline() {
        let mut store = FaultStore::new();
        store.windows_fault = Fault::Delay;
        store.open_fault = Fault::Delay;
        let start = tokio::time::Instant::now();
        let mut pending = record_transition(&store, join_transition()).await;
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS)
        );
        assert!(!pending.is_complete());
        assert_eq!(store.calls(), ["windows", "open_rows"]);
        assert!(store.writes.lock().unwrap().inserts.is_empty());
        store.open_fault = Fault::Pass;
        pending.turn(&store).await;
        assert!(pending.is_complete());
        assert_eq!(
            store
                .calls()
                .iter()
                .filter(|call| **call == "windows")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn many_unfinished_rows_drain_in_bounded_turns_before_open() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        let template = store.open_rows[0].clone();
        store.open_rows = (1..=100)
            .map(|id| AttendanceSession {
                id,
                ..template.clone()
            })
            .collect();
        let observed_at = OffsetDateTime::now_utc();
        let mut pending = record_transition(
            &store,
            VoiceTransition {
                observed_at,
                ..move_transition()
            },
        )
        .await;
        assert_eq!(store.writes.lock().unwrap().closes.len(), CLOSES_PER_TURN);
        assert!(!pending.is_complete());
        let mut turns = 1;
        while !pending.is_complete() {
            assert!(turns < 20, "bounded pages must eventually drain");
            let before = store.writes.lock().unwrap().closes.len();
            assert!(store.writes.lock().unwrap().inserts.is_empty());
            pending.turn(&store).await;
            assert!(store.writes.lock().unwrap().closes.len() - before <= CLOSES_PER_TURN);
            turns += 1;
        }
        let writes = store.writes.lock().unwrap();
        assert_eq!(writes.closes.len(), 100);
        assert_eq!(writes.closed.len(), 100);
        assert!(writes
            .closes
            .iter()
            .all(
                |(_, at, name, tags)| millis_of(*at) == millis_of(observed_at)
                    && name == "lobby-renamed"
                    && tags == &["game-night"]
            ));
        assert_eq!(writes.inserts.len(), 1);
    }

    #[tokio::test]
    async fn oversized_candidates_finalize_existing_tags_without_open() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.windows = vec![store.windows[0].clone(); 129];
        let pending = record_transition(&store, move_transition()).await;
        assert!(pending.is_complete());
        let writes = store.writes.lock().unwrap();
        assert_eq!(writes.closes.len(), 1);
        assert_eq!(writes.closes[0].3, ["game-night"]);
        assert!(writes.inserts.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn many_slow_closes_share_deadline_and_retry_only_unfinished_row() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        let template = store.open_rows[0].clone();
        store.open_rows = (1..=100)
            .map(|id| AttendanceSession {
                id,
                ..template.clone()
            })
            .collect();
        store.close_fault = Fault::Delay;
        let started = tokio::time::Instant::now();
        let mut pending = record_transition(&store, move_transition()).await;
        assert_eq!(
            started.elapsed(),
            std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS)
        );
        assert!(!pending.is_complete());
        {
            let writes = store.writes.lock().unwrap();
            assert_eq!(writes.closed.len(), 1);
            assert_eq!(writes.closes.len(), 2);
            assert_eq!(writes.closes[1].0, 2);
            assert!(writes.inserts.is_empty());
        }
        store.close_fault = Fault::Pass;
        pending.turn(&store).await;
        let writes = store.writes.lock().unwrap();
        assert_eq!(writes.closes.len(), 2 + CLOSES_PER_TURN);
        assert_eq!(writes.closes[1], writes.closes[2]);
        assert_eq!(writes.closes.iter().filter(|close| close.0 == 1).count(), 1);
        assert!(writes.inserts.is_empty());
    }

    #[tokio::test]
    async fn oversize_live_metadata_never_retains_unbounded_text_or_opens() {
        let store = FaultStore::new().with_open_row("7", "known-name");
        let pending = record_transition(
            &store,
            VoiceTransition {
                previous_name: Some("p".repeat(4097)),
                current_name: Some("c".repeat(4097)),
                ..move_transition()
            },
        )
        .await;
        assert!(pending.is_complete());
        let writes = store.writes.lock().unwrap();
        assert_eq!(writes.closes.len(), 1);
        assert_eq!(writes.closes[0].2, "known-name");
        assert!(writes.inserts.is_empty());
    }

    #[tokio::test]
    async fn oversized_stored_row_keeps_original_evidence_and_never_opens() {
        let mut store = FaultStore::new().with_open_row("7", "known-name");
        let template = store.open_rows[0].clone();
        store.open_rows = (1..=40)
            .map(|id| AttendanceSession {
                id,
                ..template.clone()
            })
            .collect();
        store.open_rows[0].event_keys = vec!["k".repeat(16385)];
        store.open_rows[1].channel_id = "x".repeat(21);
        store.open_rows[2].channel_name = "n".repeat(4097);
        let observed_at = OffsetDateTime::now_utc();
        let mut pending = record_transition(
            &store,
            VoiceTransition {
                observed_at,
                ..move_transition()
            },
        )
        .await;
        for _ in 0..8 {
            if pending.is_complete() {
                break;
            }
            let before = store.writes.lock().unwrap().closes.len();
            pending.turn(&store).await;
            assert!(store.writes.lock().unwrap().closes.len() - before <= CLOSES_PER_TURN);
            assert!(store.writes.lock().unwrap().inserts.is_empty());
        }
        assert!(
            pending.is_complete(),
            "malformed rows cannot permanently park the inbox"
        );
        let writes = store.writes.lock().unwrap();
        assert_eq!(writes.closed.len(), 40);
        assert_eq!(writes.closes.len(), 40);
        assert_eq!(millis_of(writes.closes[0].1), millis_of(observed_at));
        assert_eq!(millis_of(writes.closes[1].1), millis_of(template.joined_at));
        assert!(writes.closes[..3].iter().all(|close| close.3.is_empty()));
        assert_eq!(writes.closes[0].2, "known-name");
        assert_eq!(writes.closes[2].2.len(), 4097);
        assert!(
            writes.inserts.is_empty(),
            "later well-formed pages cannot re-enable opens"
        );
    }

    #[tokio::test]
    async fn revoked_gateway_at_open_boundary_keeps_close_but_drops_open() {
        let store = FaultStore::new().with_open_row("7", "lobby");
        let mut pending = AttendanceTransition::new(move_transition());
        pending.turn_if(&store, || false).await;
        assert!(pending.is_complete());
        let writes = store.writes.lock().unwrap();
        assert_eq!(writes.closes.len(), 1);
        assert!(writes.inserts.is_empty());
    }

    #[tokio::test]
    async fn insert_error_returns_after_close_is_recorded() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.insert_fault = Fault::Fail;
        record_transition(&store, move_transition()).await;
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes.len(), 1);
        assert_eq!(writes.inserts.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn insert_timeout_returns_after_close_is_recorded() {
        let mut store = FaultStore::new().with_open_row("7", "lobby");
        store.insert_fault = Fault::Hang;
        record_transition(&store, move_transition()).await;
        let writes = store.writes.lock().expect("writes lock");
        assert_eq!(writes.closes.len(), 1);
        assert_eq!(writes.inserts.len(), 1);
    }
}
