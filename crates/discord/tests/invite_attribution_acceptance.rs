//! Acceptance tests: invite-attribution tracker (snapshot-diff, non-retroactive).
//!
//! Failing-first spec for the parent invite-attribution build: the Builder
//! implements on the parent card until every test below passes. Change NO
//! expectations to make them pass; change the pipeline.
//!
//! Parent acceptance coverage, one observable per bullet:
//!
//! * (a) `INVITE_CREATE` seeds `{code, inviter, uses, max_uses, temporary}`:
//!   the staged seed write carries the cap and temporariness, and the next
//!   join measures growth against the seed.
//! * (b) A single-growth join credits the inviter end to end: `inviter_for` →
//!   `JoinInput.inviter_id` → funnel-row `metadata.inviterId` → buffered
//!   community-facts join write.
//! * (c) A no-movement join on a vanity guild files `vanity` (never an
//!   invite); the same silence on a guild without vanity (e.g. boosted but
//!   vanity-less) files `unknown`.
//! * (d) A join whose code was deleted before the check — failed REST read or
//!   successful read with the code missing — files `unknown` and never credits
//!   the stale inviter.
//! * (e) One join with two growing codes files the deterministic
//!   `ambiguous:a+b` (sorted; never order-dependent) with no inviter.
//! * (g) A pre-install join with no baseline files `unknown`
//!   (non-retroactive) and poisons nothing: later joins attribute normally.
//!
//! Log contract (RED on the current head — the pipeline emits no attribution
//! event today): every handled `MemberAdd` MUST emit exactly one INFO tracing
//! event carrying the filed `source` string, the joining member id, and the
//! credited inviter id when (and only when) a single code grew. Capture is a
//! single global subscriber whose writer fans out to a per-thread buffer, so
//! parallel tests cannot cross-talk (callsite interest is process-global) and
//! no wall clock is involved. Deterministic stamps only.
//!
//! Builder maintenance note: `snap()` below is the ONLY `InviteState` literal
//! in this file. When `max_uses`/`temporary` land on `InviteState`, extend
//! that one literal (assertion-neutral); every other construction spreads from
//! it. Sibling test files carry their own literals and need the same touch.
//!
//! No staging/prod data, no secrets, disposable seams only.

#[path = "support/deferred_capture_compat.rs"]
mod deferred_capture_compat;
use deferred_capture_compat::Healthy;

use std::str::FromStr;
use std::sync::Mutex;

use twilight_model::{
    gateway::{
        event::Event,
        payload::incoming::{InviteCreate, MemberAdd},
    },
    guild::{Member, MemberFlags},
    id::Id,
    user::User,
    util::Timestamp,
};
use two_bot_core::{
    analytics_family::AnalyticsCategory, gateway_funnel::InviteSnapshotWrite, EventType,
    FunnelEvent, FunnelStore, InviteState, MemStore, NoopLeveling, RecordOutcome, Snowflake,
    StoredRow,
};
use two_bot_discord::{DeferredCommunityFacts, NoClassification, Pipeline, ScriptedInvites};

const GUILD: u64 = 100_000_000_000_000_001;
const A: u64 = 900_000_000_000_001_111;
const B: u64 = 900_000_000_000_001_222;
const C: u64 = 900_000_000_000_001_333;
const D: u64 = 900_000_000_000_001_444;
const E: u64 = 900_000_000_000_001_555;
const CH_TEXT: u64 = 200_000_000_000_000_001;
const INVITER: u64 = 900_000_000_000_007_099;
const OTHER_INVITER: u64 = 900_000_000_000_007_098;

/// Scripted invite counter. Central literal: extend HERE when `max_uses` /
/// `temporary` land (see module docs); every test spreads from this helper.
fn snap(code: &str, uses: u64, inviter: Option<Snowflake>) -> InviteState {
    InviteState {
        code: code.to_owned(),
        uses,
        inviter_id: inviter,
        channel_id: Some(CH_TEXT),
        // Assertion-neutral (module docs): scripted counters carry no cap.
        max_uses: 0,
        temporary: false,
    }
}

fn stamp(s: &str) -> String {
    format!("2026-09-20T{s}.000Z")
}

fn ts(s: &str) -> Timestamp {
    Timestamp::from_str(&format!("2026-09-20T{s}.000+00:00")).expect("fixture stamp")
}

fn user(id: u64) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot: false,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "member".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

fn join_event(member_id: u64, joined: &str) -> Event {
    Event::MemberAdd(Box::new(MemberAdd {
        guild_id: Id::new(GUILD),
        member: Member {
            avatar: None,
            avatar_decoration_data: None,
            banner: None,
            communication_disabled_until: None,
            deaf: false,
            flags: MemberFlags::empty(),
            joined_at: Some(ts(joined)),
            mute: false,
            nick: None,
            pending: false,
            premium_since: None,
            roles: vec![],
            user: user(member_id),
        },
    }))
}

fn invite_create(code: &str, max_uses: u64, temporary: bool, inviter: Option<u64>) -> Event {
    Event::InviteCreate(Box::new(InviteCreate {
        channel_id: Id::new(CH_TEXT),
        code: code.to_owned(),
        created_at: ts("12:00:00"),
        guild_id: Id::new(GUILD),
        inviter: inviter.map(user),
        max_age: 0,
        max_uses,
        target_user_type: None,
        target_user: None,
        temporary,
        uses: 0,
    }))
}

/// `FunnelStore` that delegates persistence to `MemStore` and records every
/// staged invite-snapshot write for assertions.
#[derive(Debug, Default)]
struct RecordingStore {
    inner: MemStore,
    snapshots: Mutex<Vec<StagedSnapshot>>,
}

#[derive(Debug)]
struct StagedSnapshot {
    replace_all: bool,
    states_debug: String,
}

impl RecordingStore {
    fn rows(&self) -> Vec<StoredRow> {
        self.inner.rows()
    }

    fn staged(&self) -> Vec<(bool, String)> {
        self.snapshots
            .lock()
            .expect("snapshot lock")
            .iter()
            .map(|s| (s.replace_all, s.states_debug.clone()))
            .collect()
    }
}

impl FunnelStore for RecordingStore {
    fn mark_bot(&self, guild_id: Snowflake, member_id: Snowflake) {
        self.inner.mark_bot(guild_id, member_id);
    }

    fn record(&self, event: FunnelEvent) -> RecordOutcome {
        self.inner.record(event)
    }

    fn touch_activity(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
        family: AnalyticsCategory,
    ) {
        self.inner.touch_activity(guild_id, member_id, at, family);
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

    fn stage_invite_snapshot(&self, snapshot: InviteSnapshotWrite) {
        self.snapshots
            .lock()
            .expect("snapshot lock")
            .push(StagedSnapshot {
                replace_all: snapshot.replace_all,
                states_debug: format!("{:?}", snapshot.states),
            });
    }
}

type AttrPipeline = Pipeline<
    RecordingStore,
    NoopLeveling,
    DeferredCommunityFacts,
    ScriptedInvites,
    NoClassification,
>;

fn pipeline() -> (AttrPipeline, DeferredCommunityFacts) {
    let buffer = DeferredCommunityFacts::new();
    let pipeline = Pipeline::new(
        RecordingStore::default(),
        Some(NoopLeveling),
        Some(buffer.clone()),
        ScriptedInvites::new(),
        NoClassification,
    );
    (pipeline, buffer)
}

thread_local! {
    static CAPTURE_BUF: std::cell::RefCell<Option<Vec<u8>>> =
        const { std::cell::RefCell::new(None) };
}

/// `io::Write` that appends to the calling thread's capture buffer, when one
/// is armed by `capture_logs`. Events on threads without a buffer are dropped.
struct ThreadLocalSink;

impl std::io::Write for ThreadLocalSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE_BUF.with(|cell| {
            if let Some(vec) = cell.borrow_mut().as_mut() {
                vec.extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install the single process-global INFO subscriber once. Callsite interest
/// and max-level hints are process-global, so a per-test scoped subscriber
/// intermittently misses events under parallel runs; one global subscriber
/// keeps interest enabled while the thread-local writer keeps tests isolated.
fn ensure_global_subscriber() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(|| ThreadLocalSink)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Run `run` under the shared global INFO subscriber and return the lines
/// logged on this thread. `Pipeline::handle` is synchronous, so the
/// attribution event is written on the test's own thread.
fn capture_logs(run: impl FnOnce()) -> Vec<String> {
    ensure_global_subscriber();
    CAPTURE_BUF.with(|cell| *cell.borrow_mut() = Some(Vec::new()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    let bytes = CAPTURE_BUF.with(|cell| cell.borrow_mut().take().unwrap_or_default());
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The single attribution log line for one join: names the filed source and
/// the joining member.
fn attribution_line<'a>(logs: &'a [String], source: &str, member_id: u64) -> &'a String {
    let member = member_id.to_string();
    let hits: Vec<&String> = logs
        .iter()
        .filter(|line| line.contains(source) && line.contains(member.as_str()))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "exactly one attribution log per join (source={source} member={member}): {logs:?}"
    );
    hits[0]
}

fn join_row(pipeline: &AttrPipeline, member_id: u64) -> StoredRow {
    pipeline
        .handlers()
        .store()
        .rows()
        .into_iter()
        .find(|r| r.event_type == EventType::MemberJoin && r.member_id == Some(member_id))
        .expect("member_join row")
}

/// (a) `INVITE_CREATE` seeds code/inviter/uses AND the cap/temporariness, as a
/// single-code upsert; the next snapshot read measures growth against it.
#[test]
fn invite_create_seed_carries_max_uses_and_temporary() {
    let (pipeline, _buffer) = pipeline();
    pipeline.handle_at(
        &invite_create("fresh01", 4213, true, Some(INVITER)),
        &stamp("12:00:00"),
    );

    let staged = pipeline.handlers().store().staged();
    assert_eq!(
        staged.len(),
        1,
        "one seed write, no full snapshot: {staged:?}"
    );
    assert!(!staged[0].0, "InviteCreate is an upsert, not a replacement");
    let debug = &staged[0].1;
    assert!(debug.contains("fresh01"), "seed carries the code: {debug}");
    // RED: the seed drops the cap and temporariness today.
    assert!(debug.contains("max_uses"), "seed carries max_uses: {debug}");
    assert!(
        debug.contains("4213"),
        "seed carries the max_uses value: {debug}"
    );
    assert!(
        debug.contains("temporary"),
        "seed carries temporary: {debug}"
    );

    // The seeded baseline still measures growth of 1, not of the whole counter.
    pipeline
        .invite_source()
        .push(GUILD, vec![snap("fresh01", 1, Some(INVITER))]);
    pipeline.handle_at(&join_event(A, "12:00:00"), &stamp("12:00:00"));
    assert_eq!(join_row(&pipeline, A).source, "invite:fresh01");
}

/// (b) Single-growth join credits the inviter end to end: funnel row source +
/// `metadata.inviterId` + buffered community-facts join write + log.
#[test]
fn single_growth_join_credits_inviter_end_to_end() {
    let (pipeline, buffer) = pipeline();
    pipeline
        .invite_source()
        .push(GUILD, vec![snap("twodev01", 5, Some(INVITER))]);
    pipeline.handle_at(&join_event(A, "12:00:00"), &stamp("12:00:00"));

    pipeline
        .invite_source()
        .push(GUILD, vec![snap("twodev01", 6, Some(INVITER))]);
    let logs = capture_logs(|| {
        pipeline.handle_at(&join_event(B, "12:30:00"), &stamp("12:30:00"));
    });

    let row = join_row(&pipeline, B);
    assert_eq!(row.source, "invite:twodev01");
    assert_eq!(
        row.metadata,
        Some(serde_json::json!({ "inviterId": INVITER.to_string() })),
        "funnel row persists the credited inviter"
    );
    let joins = buffer.take_joins().healthy();
    let fact = joins
        .iter()
        .find(|w| w.member_id == B)
        .expect("buffered join fact");
    assert_eq!(fact.inviter_id, Some(INVITER));
    // RED: no attribution log is emitted today.
    let line = attribution_line(&logs, "invite:twodev01", B);
    assert!(
        line.contains(&INVITER.to_string()),
        "log carries the credited inviter: {line}"
    );
}

/// (c) No-movement join on a vanity guild files `vanity` — never an invite,
/// never an inviter — and a guild without vanity (boosted or plain) files
/// `unknown` for the same silence.
#[test]
fn no_movement_join_files_vanity_or_unknown_never_an_invite() {
    let (vanity_pipeline, buffer) = pipeline();
    vanity_pipeline.set_guild_vanity(GUILD, true);
    vanity_pipeline.invite_source().push(GUILD, vec![]);
    let logs = capture_logs(|| {
        vanity_pipeline.handle_at(&join_event(A, "12:00:00"), &stamp("12:00:00"));
    });
    let row = join_row(&vanity_pipeline, A);
    assert_eq!(row.source, "vanity");
    assert_eq!(row.metadata, None, "vanity credits no inviter");
    assert_eq!(buffer.take_joins().healthy()[0].inviter_id, None);
    // RED: no per-edge log today.
    let line = attribution_line(&logs, "vanity", A);
    assert!(
        !line.contains(&INVITER.to_string()),
        "vanity log must not name an inviter: {line}"
    );

    let (plain, _) = pipeline();
    plain.invite_source().push(GUILD, vec![]);
    let logs = capture_logs(|| {
        plain.handle_at(&join_event(B, "12:30:00"), &stamp("12:30:00"));
    });
    let row = join_row(&plain, B);
    assert_eq!(row.source, "unknown");
    assert_eq!(row.metadata, None);
    attribution_line(&logs, "unknown", B);
}

/// (d) Deleted-before-check: a failed REST read and a successful read with the
/// code missing both file `unknown` and never credit the stale inviter.
#[test]
fn deleted_before_check_files_unknown_never_stale_inviter() {
    let (pipeline, buffer) = pipeline();
    pipeline
        .invite_source()
        .push(GUILD, vec![snap("gone01", 5, Some(INVITER))]);
    pipeline.handle_at(&join_event(A, "12:00:00"), &stamp("12:00:00"));

    // Failed read: no snapshot queued → baseline kept, join still records.
    let logs = capture_logs(|| {
        pipeline.handle_at(&join_event(B, "12:30:00"), &stamp("12:30:00"));
    });
    let row = join_row(&pipeline, B);
    assert_eq!(row.source, "unknown");
    assert_eq!(row.metadata, None, "failed read credits no stale inviter");
    attribution_line(&logs, "unknown", B);

    // Successful read with the code missing: pruned, not credited.
    pipeline.invite_source().push(GUILD, vec![]);
    let logs = capture_logs(|| {
        pipeline.handle_at(&join_event(C, "13:00:00"), &stamp("13:00:00"));
    });
    let row = join_row(&pipeline, C);
    assert_eq!(row.source, "unknown");
    assert_eq!(row.metadata, None, "deleted code credits no stale inviter");
    attribution_line(&logs, "unknown", C);

    assert!(
        buffer
            .take_joins()
            .healthy()
            .iter()
            .all(|w| w.inviter_id.is_none()),
        "no join after the delete carries the stale inviter"
    );
}

/// (e) One join with two growing codes files the deterministic
/// `ambiguous:a+b` (sorted — never read-order dependent) with no inviter.
#[test]
fn simultaneous_double_increment_files_sorted_ambiguous_without_inviter() {
    let (pipeline, buffer) = pipeline();
    pipeline.invite_source().push(
        GUILD,
        vec![
            snap("a", 5, Some(INVITER)),
            snap("b", 5, Some(OTHER_INVITER)),
        ],
    );
    pipeline.handle_at(&join_event(D, "12:00:00"), &stamp("12:00:00"));

    // Deliberately reverse-alphabetical read order: the filed string must not
    // follow it.
    pipeline.invite_source().push(
        GUILD,
        vec![
            snap("b", 6, Some(OTHER_INVITER)),
            snap("a", 6, Some(INVITER)),
        ],
    );
    let logs = capture_logs(|| {
        pipeline.handle_at(&join_event(E, "12:30:00"), &stamp("12:30:00"));
    });
    let row = join_row(&pipeline, E);
    // RED: today the string follows read order ("ambiguous:b+a").
    assert_eq!(row.source, "ambiguous:a+b");
    assert_eq!(row.metadata, None, "ambiguous credits no inviter");
    let joins = buffer.take_joins().healthy();
    assert_eq!(
        joins
            .iter()
            .find(|w| w.member_id == E)
            .expect("fact")
            .inviter_id,
        None
    );
    // RED: no per-edge log today.
    let line = attribution_line(&logs, "ambiguous", E);
    assert!(
        !line.contains(&INVITER.to_string()) && !line.contains(&OTHER_INVITER.to_string()),
        "ambiguous log must not name an inviter: {line}"
    );
}

/// (g) Pre-install join with no baseline files `unknown` (non-retroactive):
/// the bot must not back-attribute from counters it never witnessed, and the
/// empty start must not poison later attribution.
#[test]
fn pre_install_join_without_baseline_files_unknown() {
    let (pipeline, buffer) = pipeline();
    // No snapshot ever queued: the read fails, the join still records.
    let logs = capture_logs(|| {
        pipeline.handle_at(&join_event(A, "12:00:00"), &stamp("12:00:00"));
    });
    let row = join_row(&pipeline, A);
    assert_eq!(row.source, "unknown");
    assert_eq!(row.metadata, None);
    assert_eq!(buffer.take_joins().healthy()[0].inviter_id, None);
    // RED: no per-edge log today.
    attribution_line(&logs, "unknown", A);

    // Later joins attribute normally: the empty start poisoned nothing.
    pipeline
        .invite_source()
        .push(GUILD, vec![snap("twodev01", 5, Some(INVITER))]);
    pipeline.handle_at(&join_event(B, "12:30:00"), &stamp("12:30:00"));
    assert_eq!(join_row(&pipeline, B).source, "unknown");
    pipeline
        .invite_source()
        .push(GUILD, vec![snap("twodev01", 6, Some(INVITER))]);
    pipeline.handle_at(&join_event(C, "13:00:00"), &stamp("13:00:00"));
    assert_eq!(join_row(&pipeline, C).source, "invite:twodev01");
}
