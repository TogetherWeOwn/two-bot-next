// Test-only SQLx boundary for sweep_budget_acceptance.rs. This is compiled
// beside the production sweep body plus the Builder's budgeted pass, not a
// second sweep implementation. Unsupported queries panic instead of silently
// manufacturing a successful row.
//
// Builder contract (see sweep_budget_acceptance.rs): the budgeted pass keeps
// every SQL statement below byte-identical (matched after case/whitespace
// normalisation) and adds no new external-crate imports. If either must
// change, renegotiate on the card — never edit this file to loosen it.
extern crate self as sqlx;

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

// Shadow only Vec in the subject module. All operations forward to std Vec.
#[derive(Debug)]
pub struct SubjectVec<T>(Vec<T>);
impl<T> SubjectVec<T> {
    pub fn with_capacity(n: usize) -> Self {
        Self(Vec::with_capacity(n))
    }
}
impl<T> FromIterator<T> for SubjectVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}
impl<T> IntoIterator for SubjectVec<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
impl<T> Deref for SubjectVec<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T> DerefMut for SubjectVec<T> {
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
}
#[derive(Default, Clone, Debug)]
struct Stats {
    selects: usize,
    inserts: usize,
    updates: usize,
}
#[derive(Default)]
pub struct State {
    members: Vec<Member>,
    events: HashSet<String>,
    insert_attempts: HashMap<String, usize>,
    stats: Stats,
}

const SELECT_MEMBERS: &str = "select guild_id, member_id, coalesce(last_active_at, joined_at), inactive_flagged_at, is_bot from members where left_at is null and not is_bot and coalesce(last_active_at, joined_at) < $1::timestamptz and (inactive_flagged_at is null or inactive_flagged_at < $1::timestamptz)";
const PRIVACY_EXCLUSION: &str = " and not exists ( select 1 from privacy_opt_outs as opt where opt.guild_id = members.guild_id and opt.member_id = members.member_id )";
const KEYSET: &str = " and (not $3::boolean or (guild_id, member_id) > ($4, $5))";
const KEY_ORDER: &str = " order by guild_id, member_id";
const SELECT_FRESH: &str = "select coalesce(last_active_at, joined_at), inactive_flagged_at, is_bot, left_at from members where guild_id = $1 and member_id = $2 for update";

struct ReadPlan {
    cursor: Option<(String, String)>,
    limit: usize,
}
pub struct Pool<DB> {
    state: Arc<Mutex<State>>,
    marker: PhantomData<DB>,
}
impl Pool<Postgres> {
    async fn begin(&self) -> Result<Transaction, Error> {
        Ok(Transaction(Connection(self.state.clone())))
    }
    fn selects(&self) -> usize {
        self.state.lock().expect("state").stats.selects
    }
    fn insert_attempts(&self) -> HashMap<String, usize> {
        self.state.lock().expect("state").insert_attempts.clone()
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
    fn read_plan(&self) -> ReadPlan {
        // The paged select keeps its exact shape: unknown predicates, wrong
        // placeholders, OFFSET and partial/reversed ordering all fail closed.
        assert!(
            self.sql.contains(PRIVACY_EXCLUSION),
            "suppression exclusion left the sweep select: {}",
            self.sql
        );
        assert!(
            self.sql.contains(KEYSET),
            "keyset cursor left the sweep select: {}",
            self.sql
        );
        assert!(
            self.sql.contains(KEY_ORDER),
            "keyset order left the sweep select: {}",
            self.sql
        );
        let bare = self
            .sql
            .replacen(PRIVACY_EXCLUSION, "", 1)
            .replacen(KEYSET, "", 1)
            .replacen(KEY_ORDER, "", 1);
        assert_eq!(
            bare,
            format!("{SELECT_MEMBERS} limit $2"),
            "unsupported read"
        );
        assert_eq!(self.binds.len(), 5, "read bind count");
        let cursor = match self.binds[2].as_str() {
            "false" => {
                assert!(
                    self.binds[3].is_empty() && self.binds[4].is_empty(),
                    "first-page cursor must be empty"
                );
                None
            }
            "true" => {
                assert!(
                    !self.binds[3].is_empty() && !self.binds[4].is_empty(),
                    "active cursor must contain both keys"
                );
                Some((self.binds[3].clone(), self.binds[4].clone()))
            }
            _ => panic!("cursor-enabled bind must be boolean"),
        };
        let limit = self.binds[1].parse().expect("numeric page limit");
        assert!(limit > 0, "page limit must be positive");
        ReadPlan { cursor, limit }
    }
    async fn fetch_all(
        self,
        executor: impl Executor<Database = DB>,
    ) -> Result<SubjectVec<O>, Error> {
        let plan = self.read_plan();
        let cutoff = funnel::parse_iso_millis(&self.binds[0]).expect("ISO cutoff");
        let shared = executor.state();
        let mut state = shared.lock().expect("state");
        state.stats.selects += 1;
        let mut rows: Vec<(String, String, Option<Date>, Option<Date>, bool)> = state
            .members
            .iter()
            .filter(|m| m.left.is_none())
            .filter(|m| !m.bot)
            .filter(|m| m.seen.is_some_and(|seen| seen.0 < cutoff))
            .filter(|m| m.flagged.is_none_or(|flagged| flagged.0 < cutoff))
            .filter(|m| !m.suppressed)
            .filter(|m| {
                plan.cursor.as_ref().is_none_or(|(guild, member)| {
                    (m.guild.clone(), m.member.clone()) > (guild.clone(), member.clone())
                })
            })
            .map(|m| {
                (
                    m.guild.clone(),
                    m.member.clone(),
                    m.seen,
                    m.flagged,
                    m.bot,
                )
            })
            .collect();
        rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        rows.truncate(plan.limit);
        Ok(cast(SubjectVec(rows)))
    }
    async fn fetch_optional(
        self,
        executor: impl Executor<Database = DB>,
    ) -> Result<Option<O>, Error> {
        let shared = executor.state();
        let mut state = shared.lock().expect("state");
        if self.sql == SELECT_FRESH {
            let row = state
                .members
                .iter()
                .find(|m| m.guild == self.binds[0] && m.member == self.binds[1])
                .map(|m| (m.seen, m.flagged, m.bot, m.left));
            return Ok(cast(row));
        }
        if self.sql.starts_with(
            "insert into events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)",
        ) {
            let key = self.binds[5].clone();
            *state.insert_attempts.entry(key.clone()).or_default() += 1;
            if state.events.insert(key) {
                state.stats.inserts += 1;
                return Ok(cast(Some((0i64,))));
            }
            return Ok(cast(None::<(i64,)>));
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
        let state = shared.lock().expect("state");
        Ok(cast(i64::from(state.events.contains(&self.binds[0]))))
    }
    async fn execute(self, executor: impl Executor<Database = DB>) -> Result<(), Error> {
        let shared = executor.state();
        let mut state = shared.lock().expect("state");
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
const DAYS: u64 = 14;
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
fn one_page_budget() -> subject::SweepBudget {
    subject::SweepBudget {
        max_pages: 1,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(3600),
    }
}
fn ample_budget() -> subject::SweepBudget {
    subject::SweepBudget {
        max_pages: 10_000,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(3600),
    }
}
fn assert_each_inserted_once(pool: &Pool<Postgres>, members: usize) {
    let attempts = pool.insert_attempts();
    assert_eq!(
        attempts.len(),
        members,
        "every member attempted exactly one event write"
    );
    assert!(
        attempts.values().all(|count| *count == 1),
        "no member re-attempted across resume: {attempts:?}"
    );
}

#[test]
fn budget_limits_select_pages_and_resume_completes() {
    let pool = seeded(9_000);
    let (first, cursor) = run(subject::run_sweep_budgeted(
        &pool,
        NOW,
        DAYS,
        one_page_budget(),
        None,
    ))
    .expect("budgeted pass");
    assert_eq!(pool.selects(), 1, "one page budget reads one page");
    assert_eq!(first.flagged.len(), 4_096, "first page flags its rows");
    let mut resume = Some(cursor.expect("more work remains after one page of three"));
    let mut union = first.flagged;
    let mut passes = 1;
    while let Some(cursor) = resume {
        passes += 1;
        assert!(passes <= 6, "resume did not terminate");
        let (out, next) = run(subject::run_sweep_budgeted(
            &pool,
            NOW,
            DAYS,
            one_page_budget(),
            Some(cursor),
        ))
        .expect("resume pass");
        union.extend(out.flagged);
        resume = next;
    }
    // Byte-identical to one complete sweep over the same fixture.
    let fresh = seeded(9_000);
    let full = run(subject::run_sweep(&fresh, NOW, DAYS)).expect("complete sweep");
    assert_eq!(union, full.flagged);
    assert_eq!(union.len(), 9_000, "nothing silently skipped across passes");
    assert_each_inserted_once(&pool, 9_000);
}

#[test]
fn expired_deadline_reads_almost_nothing_and_still_resumes() {
    let pool = seeded(5_000);
    let past = subject::SweepBudget {
        max_pages: 10_000,
        deadline: std::time::Instant::now() - std::time::Duration::from_secs(1),
    };
    let (_out, cursor) = run(subject::run_sweep_budgeted(&pool, NOW, DAYS, past, None))
        .expect("expired-deadline pass");
    assert!(
        pool.selects() <= 1,
        "expired deadline stops before paging: {} selects",
        pool.selects()
    );
    let mut resume = Some(cursor.expect("expired deadline reports more work may remain"));
    let mut union = Vec::new();
    let mut passes = 1;
    while let Some(cursor) = resume {
        passes += 1;
        assert!(passes <= 4, "resume did not terminate");
        let (out, next) =
            run(subject::run_sweep_budgeted(&pool, NOW, DAYS, ample_budget(), Some(cursor)))
                .expect("resume pass");
        union.extend(out.flagged);
        resume = next;
    }
    // The expired pass may or may not have written page one (check-before-read
    // vs read-first shapes both satisfy the budget); the union still completes
    // exactly once.
    let fresh = seeded(5_000);
    let full = run(subject::run_sweep(&fresh, NOW, DAYS)).expect("complete sweep");
    assert_each_inserted_once(&pool, 5_000);
    assert!(
        union.len() == 5_000 || union.len() == 5_000 - 4_096,
        "resume completes the remainder exactly: {} of 5000",
        union.len()
    );
    if union.len() == 5_000 - 4_096 {
        assert_eq!(
            union,
            full.flagged[4_096..],
            "remainder byte-identical to the complete sweep tail"
        );
    } else {
        assert_eq!(union, full.flagged, "resume byte-identical to complete sweep");
    }
}

#[test]
fn complete_sweep_flags_everything_exactly_once() {
    let pool = seeded(5_000);
    let full = run(subject::run_sweep(&pool, NOW, DAYS)).expect("complete sweep");
    assert_eq!(full.flagged.len(), 5_000);
    assert!(
        full.flagged
            .windows(2)
            .all(|w| (w[0].guild_id.clone(), w[0].member_id.clone())
                <= (w[1].guild_id.clone(), w[1].member_id.clone())),
        "keyset order"
    );
    assert!(
        pool.selects() <= 3,
        "two data pages plus at most one empty tail read: {}",
        pool.selects()
    );
    assert_each_inserted_once(&pool, 5_000);
}
