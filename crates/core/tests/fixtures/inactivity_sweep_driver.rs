// Test-only SQLx boundary for inactivity_sweep_acceptance.rs. This is compiled
// beside the unchanged production sweep body, not a second sweep implementation.
// Unsupported queries panic instead of silently manufacturing a successful row.
extern crate self as sqlx;

use std::any::Any;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

static COMPARISONS: AtomicUsize = AtomicUsize::new(0);

// Shadow only Vec in the store module. All operations forward to std Vec;
// contains performs the same left-to-right equality search, counting its work.
// The production domain's outcome remains an ordinary Vec.
#[derive(Debug)]
pub struct CountingVec<T>(Vec<T>);
impl<T> CountingVec<T> {
    pub fn with_capacity(n: usize) -> Self {
        Self(Vec::with_capacity(n))
    }
    pub fn contains(&self, needle: &T) -> bool
    where
        T: PartialEq,
    {
        self.0.iter().any(|item| {
            COMPARISONS.fetch_add(1, Ordering::Relaxed);
            item == needle
        })
    }
}
impl<T> FromIterator<T> for CountingVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}
impl<T> IntoIterator for CountingVec<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
impl<T> Deref for CountingVec<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T> DerefMut for CountingVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Debug)]
pub struct Error;
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("offline database refusal")
    }
}
impl std::error::Error for Error {}
pub struct Postgres;
pub mod types {
    pub mod time {
        #[derive(Clone, Copy, Debug)]
        pub struct OffsetDateTime(pub i64);
        impl OffsetDateTime {
            pub fn unix_timestamp(&self) -> i64 {
                self.0.div_euclid(1000)
            }
            pub fn millisecond(&self) -> u16 {
                self.0.rem_euclid(1000) as u16
            }
        }
    }
}
use types::time::OffsetDateTime as Date;

#[derive(Clone)]
struct Member {
    guild: String,
    member: String,
    seen: Option<Date>,
    flagged: Option<Date>,
    bot: bool,
    left: Option<Date>,
    suppressed: bool,
    stale: bool,
}
#[derive(Default, Clone, Debug)]
struct Stats {
    queries: usize,
    max_read: usize,
    begin: usize,
    inserts: usize,
    updates: usize,
}
#[derive(Default)]
pub struct State {
    members: Vec<Member>,
    events: HashSet<String>,
    stats: Stats,
    cutoffs: Vec<String>,
}
pub struct Pool<DB> {
    state: Arc<Mutex<State>>,
    marker: PhantomData<DB>,
}
impl Pool<Postgres> {
    async fn begin(&self) -> Result<Transaction, Error> {
        self.state.lock().expect("state").stats.begin += 1;
        Ok(Transaction(Connection(self.state.clone())))
    }
}
pub struct Connection(Arc<Mutex<State>>);
pub struct Transaction(Connection);
impl Deref for Transaction {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for Transaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Transaction {
    async fn commit(self) -> Result<(), Error> {
        Ok(())
    }
    async fn rollback(self) -> Result<(), Error> {
        Ok(())
    }
}
pub trait Executor {
    type Database;
    fn state(&self) -> Arc<Mutex<State>>;
}
impl Executor for &Pool<Postgres> {
    type Database = Postgres;
    fn state(&self) -> Arc<Mutex<State>> {
        self.state.clone()
    }
}
impl Executor for &mut Connection {
    type Database = Postgres;
    fn state(&self) -> Arc<Mutex<State>> {
        self.0.clone()
    }
}

pub struct Query<DB, O> {
    sql: String,
    binds: Vec<String>,
    marker: PhantomData<(DB, O)>,
}
pub fn query(sql: &str) -> Query<Postgres, ()> {
    query_as(sql)
}
pub fn query_as<O>(sql: &str) -> Query<Postgres, O> {
    Query {
        sql: sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase(),
        binds: Vec::new(),
        marker: PhantomData,
    }
}
pub fn query_scalar<DB, O>(sql: &str) -> Query<DB, O> {
    Query {
        sql: sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase(),
        binds: Vec::new(),
        marker: PhantomData,
    }
}
fn cast<T: 'static>(value: impl Any) -> T {
    *(Box::new(value) as Box<dyn Any>)
        .downcast::<T>()
        .expect("query result tuple matches SQLx contract")
}
impl<DB, O: 'static> Query<DB, O> {
    pub fn bind(mut self, value: impl ToString) -> Self {
        self.binds.push(value.to_string());
        self
    }
    fn number(&self, token: &str) -> usize {
        let token = token.trim_end_matches(';');
        if let Some(index) = token.strip_prefix('$') {
            self.binds[index.parse::<usize>().expect("bind number") - 1]
                .parse()
                .expect("numeric limit/offset")
        } else {
            token.parse().expect("literal limit/offset")
        }
    }
    async fn fetch_all(
        self,
        executor: impl Executor<Database = DB>,
    ) -> Result<CountingVec<O>, Error> {
        assert!(
            self.sql.starts_with("select guild_id, member_id,")
                && self.sql.contains("from members"),
            "unsupported read: {}",
            self.sql
        );
        let shared = executor.state();
        let mut state = shared.lock().expect("state");
        state.stats.queries += 1;
        let cutoff = funnel::parse_iso_millis(&self.binds[0]).expect("ISO cutoff");
        state.cutoffs.push(self.binds[0].clone());
        let limit = self
            .sql
            .split_once(" limit ")
            .map(|(_, rest)| self.number(rest.split_whitespace().next().expect("limit")));
        let offset = self.sql.split_once(" offset ").map_or(0, |(_, rest)| {
            self.number(rest.split_whitespace().next().expect("offset"))
        });
        let rows: Vec<_> = state
            .members
            .iter()
            .filter(|m| {
                !m.bot
                    && m.left.is_none()
                    && m.seen.is_some_and(|seen| seen.0 < cutoff)
                    && m.flagged.is_none_or(|flagged| flagged.0 < cutoff)
            })
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .map(|m| cast::<O>((m.guild.clone(), m.member.clone(), m.seen, m.flagged, m.bot)))
            .collect();
        state.stats.max_read = state.stats.max_read.max(rows.len());
        Ok(rows.into_iter().collect())
    }
    async fn fetch_optional(
        self,
        executor: impl Executor<Database = DB>,
    ) -> Result<Option<O>, Error> {
        let shared = executor.state();
        let mut state = shared.lock().expect("state");
        state.stats.queries += 1;
        if self.sql.starts_with("select coalesce(") && self.sql.contains("for update") {
            let found = state
                .members
                .iter()
                .find(|m| m.guild == self.binds[0] && m.member == self.binds[1]);
            return Ok(found.map(|m| {
                cast((
                    if m.stale {
                        Some(Date(9_999_999_999_999))
                    } else {
                        m.seen
                    },
                    m.flagged,
                    m.bot,
                    m.left,
                ))
            }));
        }
        if self.sql.starts_with("insert into events ") {
            let inserted = state.events.insert(self.binds[5].clone());
            if inserted {
                state.stats.inserts += 1;
            }
            return Ok(inserted.then(|| cast((1_i64,))));
        }
        panic!("unsupported optional query: {}", self.sql);
    }
    async fn fetch_one(self, executor: impl Executor<Database = DB>) -> Result<O, Error> {
        assert!(
            self.sql
                .starts_with("select count(*) from events where idempotency_key = $1"),
            "unsupported scalar: {}",
            self.sql
        );
        let shared = executor.state();
        let mut state = shared.lock().expect("state");
        state.stats.queries += 1;
        Ok(cast(i64::from(state.events.contains(&self.binds[0]))))
    }
    async fn execute(self, executor: impl Executor<Database = DB>) -> Result<(), Error> {
        let shared = executor.state();
        let mut state = shared.lock().expect("state");
        state.stats.queries += 1;
        if self
            .sql
            .starts_with("lock table events, members, privacy_opt_outs ")
        {
            return Ok(());
        }
        if self
            .sql
            .starts_with("update members set inactive_flagged_at = $1::timestamptz")
        {
            let stamp = funnel::parse_iso_millis(&self.binds[0]).expect("ISO flag");
            let row = state
                .members
                .iter_mut()
                .find(|m| m.guild == self.binds[1] && m.member == self.binds[2])
                .expect("updated member");
            row.flagged = Some(Date(stamp));
            state.stats.updates += 1;
            return Ok(());
        }
        panic!("unsupported write: {}", self.sql);
    }
}
mod privacy {
    pub async fn suppressed_in(
        tx: &mut crate::Transaction,
        guild: &str,
        member: &str,
    ) -> Result<bool, crate::Error> {
        let state = tx.0 .0.lock().expect("state");
        Ok(state
            .members
            .iter()
            .find(|m| m.guild == guild && m.member == member)
            .expect("suppression member")
            .suppressed)
    }
}

// Every fake future is ready. A Pending future is an unsupported fixture, not
// permission to open an executor, socket or timer.
fn run<F: std::future::Future>(future: F) -> F::Output {
    struct Ready;
    impl std::task::Wake for Ready {
        fn wake(self: Arc<Self>) {}
    }
    let waker = std::task::Waker::from(Arc::new(Ready));
    let mut context = std::task::Context::from_waker(&waker);
    match std::pin::pin!(future).as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("offline boundary unexpectedly waited"),
    }
}
const NOW: &str = "2026-09-07T00:00:00.000Z";
const READ_CEILING: usize = 4096;
fn seeded(count: usize) -> Pool<Postgres> {
    let seen = Date(1_782_864_000_000); // 2026-07-01 UTC, independently fixed.
    let members = (0..count)
        .map(|i| Member {
            guild: format!("fixture-guild-{}", i % 2),
            member: format!("fixture-member-{}", i / 2),
            seen: Some(seen),
            flagged: None,
            bot: false,
            left: None,
            suppressed: false,
            stale: false,
        })
        .collect();
    Pool {
        state: Arc::new(Mutex::new(State {
            members,
            ..State::default()
        })),
        marker: PhantomData,
    }
}

#[test]
fn invalid_now_is_an_error_before_any_query_or_write() {
    let mut failures = Vec::new();
    for now in [
        "",
        "not-a-time",
        "2026-09-07",
        "2026-13-07T00:00:00Z",
        "2026-09-07T25:00:00Z",
        "2026-09-07T00:00:00+1é1",
    ] {
        for count in [0, 3] {
            let pool = seeded(count);
            let result = run(subject::run_sweep(&pool, now, 14));
            let state = pool.state.lock().expect("state");
            let classification = format!("{result:?}").to_lowercase();
            if result.is_ok()
                || !classification.contains("invalid")
                || state.stats.queries != 0
                || state.stats.begin != 0
                || state.stats.inserts != 0
                || state.stats.updates != 0
                || !state.events.is_empty()
            {
                failures.push(format!(
                    "now={now:?} seeds={count}: {result:?}, {:?}",
                    state.stats
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "invalid timestamps did not fail closed:\n{}",
        failures.join("\n")
    );
}

#[test]
fn valid_iso_preserves_cutoff_stamp_and_idempotent_outcome() {
    let pool = seeded(2);
    // Same member id in two guilds must remain two distinct committed pairs.
    let first = run(subject::run_sweep(&pool, NOW, 14)).expect("valid sweep");
    assert_eq!(first.flagged.len(), 2);
    assert_eq!(first.flagged[0].guild_id, "fixture-guild-0");
    assert_eq!(first.flagged[1].guild_id, "fixture-guild-1");
    for member in &first.flagged {
        assert_eq!(member.member_id, "fixture-member-0");
        assert_eq!(member.occurred_at, NOW);
        assert_eq!(member.threshold_days, 14);
    }
    {
        let state = pool.state.lock().expect("state");
        assert_eq!(state.cutoffs[0], "2026-08-24T00:00:00.000Z");
        assert_eq!(state.stats.inserts, 2);
        assert_eq!(state.stats.updates, 2);
    }
    let second = run(subject::run_sweep(&pool, NOW, 14)).expect("repeat sweep");
    assert!(second.flagged.is_empty());
    assert_eq!(pool.state.lock().expect("state").events.len(), 2);
}

#[test]
fn quiet_member_reads_are_bounded_and_never_silently_drop_the_tail() {
    let count = READ_CEILING * 2 + 1;
    let pool = seeded(count);
    let result = run(subject::run_sweep(&pool, NOW, 14));
    let state = pool.state.lock().expect("state");
    assert!(
        state.stats.max_read <= READ_CEILING,
        "one quiet-member read returned {} rows; ceiling is {READ_CEILING}, seed={count}",
        state.stats.max_read
    );
    let outcome = result.expect("bounded sweep must page or explicitly report truncation");
    let explicit_truncation = format!("{outcome:?}").contains("truncated: true");
    assert!(
        outcome.flagged.len() == count || explicit_truncation,
        "{} of {count} members reported with no explicit truncation",
        outcome.flagged.len()
    );
    assert_eq!(outcome.flagged.len(), state.stats.inserts);
}

#[test]
fn outcome_membership_does_not_rescan_written_prefixes_at_scale() {
    let mut failures = Vec::new();
    for count in [128, 512, 2048] {
        let pool = seeded(count);
        COMPARISONS.store(0, Ordering::Relaxed);
        let outcome = run(subject::run_sweep(&pool, NOW, 14)).expect("scaling sweep");
        let comparisons = COMPARISONS.load(Ordering::Relaxed);
        let actual: HashSet<_> = outcome
            .flagged
            .iter()
            .map(|m| (m.guild_id.clone(), m.member_id.clone()))
            .collect();
        let state = pool.state.lock().expect("state");
        let expected: HashSet<_> = state
            .members
            .iter()
            .map(|m| (m.guild.clone(), m.member.clone()))
            .collect();
        assert_eq!(
            actual, expected,
            "filter must preserve every committed pair"
        );
        assert_eq!(outcome.flagged.len(), count);
        assert_eq!(state.stats.inserts, count);
        eprintln!(
            "scaling seeds={count} membership_comparisons={comparisons} budget={}",
            8 * count
        );
        if comparisons > 8 * count {
            failures.push(format!("{comparisons} comparisons for {count} outcomes"));
        }
    }
    assert!(
        failures.is_empty(),
        "quadratic written-prefix rescans: {}",
        failures.join(", ")
    );
}

#[test]
fn stale_suppressed_and_existing_events_keep_their_outcome_semantics() {
    let pool = seeded(4);
    {
        let mut state = pool.state.lock().expect("state");
        state.members[0].stale = true;
        state.members[1].suppressed = true;
        // This event exists but its projection is lagging: report it without
        // inventing another event or updating its projection.
        state.events.insert(format!(
            "fixture-guild-0:fixture-member-1:member_inactive:{NOW}"
        ));
    }
    let result = run(subject::run_sweep(&pool, NOW, 14)).expect("mixed sweep");
    let actual: Vec<_> = result
        .flagged
        .iter()
        .map(|m| (m.guild_id.as_str(), m.member_id.as_str()))
        .collect();
    assert_eq!(
        actual,
        [
            ("fixture-guild-0", "fixture-member-1"),
            ("fixture-guild-1", "fixture-member-1")
        ]
    );
    let state = pool.state.lock().expect("state");
    assert_eq!(state.stats.inserts, 1);
    assert_eq!(state.stats.updates, 1);
    assert_eq!(state.events.len(), 2);
}
