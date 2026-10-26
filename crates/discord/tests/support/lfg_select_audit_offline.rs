#![cfg(test)]

//! Test-only external boundaries for the included LFG service. No real pool or
//! transport is constructed. Store mutations use the real pure adjudicator;
//! audit IDs have primary-key uniqueness and failed appends retain no row.

use crate::lfg::{self, LfgPost, LfgRole, LfgSignup, LfgStatus, SignupOutcome};
use crate::rsvp::RsvpAudit;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, thiserror::Error)]
#[error("offline store fault")]
pub struct Error;

#[derive(Debug, Clone, thiserror::Error)]
pub enum DiscordError {
    #[error("offline Discord unavailable")]
    Unavailable(String),
}

impl DiscordError {
    pub fn is_safe_pre_mutation(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub posts: Vec<LfgPost>,
    roles: Vec<LfgRole>,
    signups: Vec<LfgSignup>,
    audits: Vec<RsvpAudit>,
}

#[derive(Debug, Default)]
struct State {
    posts: Vec<LfgPost>,
    roles: Vec<LfgRole>,
    signups: Vec<LfgSignup>,
    audits: Vec<RsvpAudit>,
    trace: Vec<&'static str>,
    audit_attempts: usize,
    signup_calls: usize,
    fail_audit: bool,
    closed: bool,
}

#[derive(Debug, Default, Clone)]
pub struct PgPool(Arc<Mutex<State>>);

impl PgPool {
    pub fn fixture() -> Self {
        Self::default()
    }

    pub fn seed_post(&self, id: &str, guild: &str) {
        let mut state = self.0.lock().expect("fixture state");
        state.posts.push(LfgPost {
            id: id.into(),
            guild_id: guild.into(),
            channel_id: "30".into(),
            message_id: Some("40".into()),
            title: "Offline raid".into(),
            starts_at: "2099-01-01T00:00:00.000Z".into(),
            status: LfgStatus::Open,
            created_by: "5".into(),
            created_at: "2026-10-19T00:00:00.000Z".into(),
            closed_at: None,
        });
        for (position, key) in ["tank", "healer"].iter().enumerate() {
            state.roles.push(LfgRole {
                lfg_id: id.into(),
                role_key: (*key).into(),
                label: (*key).into(),
                slots: 2,
                position,
            });
        }
    }

    pub fn seed_signup(&self, id: &str, user: &str, role: &str) {
        self.0
            .lock()
            .expect("fixture state")
            .signups
            .push(LfgSignup {
                lfg_id: id.into(),
                user_id: user.into(),
                role_key: role.into(),
                joined_at: "2026-10-19T00:00:00.000Z".into(),
            });
    }

    pub fn snapshot(&self) -> Snapshot {
        let state = self.0.lock().expect("fixture state");
        Snapshot {
            posts: state.posts.clone(),
            roles: state.roles.clone(),
            signups: state.signups.clone(),
            audits: state.audits.clone(),
        }
    }

    pub fn signup_role(&self, post: &str, user: &str) -> Option<String> {
        self.0
            .lock()
            .expect("fixture state")
            .signups
            .iter()
            .find(|row| row.lfg_id == post && row.user_id == user)
            .map(|row| row.role_key.clone())
    }

    pub fn audits(&self) -> Vec<RsvpAudit> {
        self.0.lock().expect("fixture state").audits.clone()
    }
    pub fn trace(&self) -> Vec<&'static str> {
        self.0.lock().expect("fixture state").trace.clone()
    }
    pub fn audit_attempts(&self) -> usize {
        self.0.lock().expect("fixture state").audit_attempts
    }
    pub fn signup_calls(&self) -> usize {
        self.0.lock().expect("fixture state").signup_calls
    }
    pub fn fail_audit(&self, fail: bool) {
        self.0.lock().expect("fixture state").fail_audit = fail;
    }

    pub async fn begin(&self) -> Result<Transaction, Error> {
        let mut state = self.0.lock().expect("fixture state");
        state.trace.push("sql.begin");
        if state.closed {
            return Err(Error);
        }
        Ok(Transaction(Connection(self.clone())))
    }

    pub async fn close(&self) {
        self.0.lock().expect("fixture state").closed = true;
    }
}

pub mod postgres {
    use super::{Error, PgPool};
    // Compatibility for the included service's existing admission unit tests.
    // Unlike a lazy SQLx pool, this cannot open a socket at all.
    pub struct PgPoolOptions;
    impl Default for PgPoolOptions {
        fn default() -> Self {
            Self::new()
        }
    }
    impl PgPoolOptions {
        pub fn new() -> Self {
            Self
        }
        pub fn connect_lazy(&self, _url: &str) -> Result<PgPool, Error> {
            Ok(PgPool::default())
        }
    }
}

pub struct Connection(PgPool);
pub struct Transaction(Connection);
impl std::ops::Deref for Transaction {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for Transaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Transaction {
    pub async fn commit(self) -> Result<(), Error> {
        self.0
             .0
             .0
            .lock()
            .expect("fixture state")
            .trace
            .push("sql.commit");
        Ok(())
    }
}

// Match SQLx's generic executor boundary: transactions require an explicit
// dereference to their connection instead of an implicit argument coercion.
pub trait Executor<'a> {
    fn connection(self) -> &'a mut Connection;
}
impl<'a> Executor<'a> for &'a mut Connection {
    fn connection(self) -> &'a mut Connection {
        self
    }
}

pub struct Query {
    sql: String,
    binds: Vec<String>,
}
pub fn query(sql: &str) -> Query {
    Query {
        sql: sql.into(),
        binds: Vec::new(),
    }
}
pub fn query_scalar(sql: &str) -> Query {
    query(sql)
}

pub enum SweepBind {
    Text(String),
    Timestamp(time::OffsetDateTime),
    Limit(i64),
}
impl From<&str> for SweepBind {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}
impl From<time::OffsetDateTime> for SweepBind {
    fn from(value: time::OffsetDateTime) -> Self {
        Self::Timestamp(value)
    }
}
impl From<i64> for SweepBind {
    fn from(value: i64) -> Self {
        Self::Limit(value)
    }
}

pub struct SweepQuery {
    sql: String,
    binds: Vec<SweepBind>,
}
pub fn query_as(sql: &str) -> SweepQuery {
    SweepQuery {
        sql: sql.into(),
        binds: Vec::new(),
    }
}
impl SweepQuery {
    pub fn bind(mut self, value: impl Into<SweepBind>) -> Self {
        self.binds.push(value.into());
        self
    }

    pub async fn fetch_all(self, pool: &PgPool) -> Result<Vec<(String,)>, Error> {
        assert_eq!(
            self.sql.split_whitespace().collect::<Vec<_>>().join(" "),
            "SELECT id FROM lfg_posts WHERE guild_id = $1 AND status = 'open' AND (starts_at <= $2 OR (message_id IS NULL AND created_at <= $3)) ORDER BY created_at, id LIMIT $4",
            "unsupported offline row query"
        );
        let [SweepBind::Text(guild), SweepBind::Timestamp(expired), SweepBind::Timestamp(orphan), SweepBind::Limit(limit)] =
            self.binds.as_slice()
        else {
            panic!("offline lifecycle scan requires guild, two timestamps and limit")
        };
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("sql.lifecycle_candidates");
        if state.closed {
            return Err(Error);
        }
        let timestamp = |at: &str| {
            time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
                .expect("fixture timestamp")
        };
        let mut rows: Vec<_> = state
            .posts
            .iter()
            .filter(|post| {
                post.guild_id == *guild
                    && post.status == LfgStatus::Open
                    && (timestamp(&post.starts_at) <= *expired
                        || (post.message_id.is_none() && timestamp(&post.created_at) <= *orphan))
            })
            .collect();
        rows.sort_by(|left, right| {
            timestamp(&left.created_at)
                .cmp(&timestamp(&right.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(rows
            .into_iter()
            .take(usize::try_from(*limit).expect("nonnegative fixture scan limit"))
            .map(|post| (post.id.clone(),))
            .collect())
    }
}

impl Query {
    pub fn bind(mut self, value: impl ToString) -> Self {
        self.binds.push(value.to_string());
        self
    }

    pub async fn execute<'a>(self, executor: impl Executor<'a>) -> Result<(), Error> {
        let connection = executor.connection();
        assert!(
            self.sql.contains("pg_advisory_xact_lock"),
            "unsupported offline SQL execution"
        );
        assert_eq!(self.binds.len(), 1, "runtime lock must have one bound key");
        connection
            .0
             .0
            .lock()
            .expect("fixture state")
            .trace
            .push("sql.lock");
        Ok(())
    }

    pub async fn fetch_one<'a>(self, executor: impl Executor<'a>) -> Result<bool, Error> {
        assert_eq!(
            self.sql, "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))",
            "unsupported offline scalar query"
        );
        assert_eq!(self.binds.len(), 1, "runtime lock must have one bound key");
        let connection = executor.connection();
        let mut state = connection.0 .0.lock().expect("fixture state");
        state.trace.push("sql.try_lock");
        if state.closed {
            return Err(Error);
        }
        // Fixtures execute sequentially; the transaction owns the only lock.
        Ok(true)
    }

    pub async fn fetch_optional(self, pool: &PgPool) -> Result<Option<String>, Error> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("sql.audit_lookup");
        let sql = self
            .sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let (selection, predicates) = sql
            .split_once(" from announcements_audit_log where ")
            .expect("offline audit SELECT with explicit WHERE required");
        assert!(
            selection == "select outcome" || selection == "select reason",
            "unsupported offline scalar projection: {selection}"
        );
        let row = state.audits.iter().find(|row| {
            predicates.split(" and ").all(|predicate| {
                let (column, operand) = predicate
                    .split_once(" = ")
                    .expect("offline audit equality predicate required");
                let expected = if let Some(index) = operand.strip_prefix('$') {
                    self.binds[index.parse::<usize>().expect("bound parameter index") - 1].as_str()
                } else {
                    operand
                        .strip_prefix('\'')
                        .and_then(|s| s.strip_suffix('\''))
                        .expect("offline SQL bound parameter or literal required")
                };
                let actual = match column {
                    "id" => Some(row.id.as_str()),
                    "guild_id" => Some(row.guild_id.as_str()),
                    "actor_id" => row.actor_id.as_deref(),
                    "target_key" => row.target_key.as_deref(),
                    "action" => Some(row.action.as_str()),
                    "outcome" => Some(row.outcome.as_str()),
                    "reason" => row.reason.as_deref(),
                    _ => panic!("unsupported offline audit predicate column"),
                };
                actual == Some(expected)
            })
        });
        Ok(row.and_then(|row| {
            if selection == "select outcome" {
                Some(row.outcome.clone())
            } else {
                row.reason.clone()
            }
        }))
    }
}

pub mod lfg_store {
    use super::*;
    use crate::automation_quota::QuotaWriteError;

    pub async fn get_lfg(pool: &PgPool, guild: &str, id: &str) -> Result<Option<LfgPost>, Error> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("store.get");
        Ok(state
            .posts
            .iter()
            .find(|post| post.id == id && post.guild_id == guild)
            .cloned())
    }
    pub async fn list_lfg_roles(pool: &PgPool, id: &str) -> Result<Vec<LfgRole>, Error> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("store.roles");
        Ok(state
            .roles
            .iter()
            .filter(|row| row.lfg_id == id)
            .cloned()
            .collect())
    }
    pub async fn list_lfg_signups(pool: &PgPool, id: &str) -> Result<Vec<LfgSignup>, Error> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("store.signups");
        Ok(state
            .signups
            .iter()
            .filter(|row| row.lfg_id == id)
            .cloned()
            .collect())
    }
    pub async fn signup_lfg(
        pool: &PgPool,
        guild: &str,
        id: &str,
        role: &str,
        actor: &str,
        at: &str,
    ) -> Result<SignupOutcome, Error> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("store.signup");
        state.signup_calls += 1;
        let post = state
            .posts
            .iter()
            .find(|post| post.id == id && post.guild_id == guild);
        let current = state
            .signups
            .iter()
            .find(|row| row.lfg_id == id && row.user_id == actor);
        let capacity = state
            .roles
            .iter()
            .find(|row| row.lfg_id == id && row.role_key == role)
            .map(|row| row.slots);
        let used = state
            .signups
            .iter()
            .filter(|row| row.lfg_id == id && row.role_key == role && row.user_id != actor)
            .count() as u64;
        let outcome = lfg::adjudicate_signup(
            post.is_some(),
            post.map_or(LfgStatus::Open, |post| post.status),
            capacity,
            current.map(|row| row.role_key.as_str()),
            role,
            used,
        );
        if matches!(outcome, SignupOutcome::Joined | SignupOutcome::Moved) {
            if let Some(current) = state
                .signups
                .iter_mut()
                .find(|row| row.lfg_id == id && row.user_id == actor)
            {
                if current.role_key != role {
                    current.role_key = role.into();
                    current.joined_at = at.into();
                }
            } else {
                state.signups.push(LfgSignup {
                    lfg_id: id.into(),
                    user_id: actor.into(),
                    role_key: role.into(),
                    joined_at: at.into(),
                });
            }
        }
        Ok(outcome)
    }
    pub async fn leave_lfg(pool: &PgPool, id: &str, actor: &str) -> Result<bool, Error> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("store.leave");
        let before = state.signups.len();
        state
            .signups
            .retain(|row| row.lfg_id != id || row.user_id != actor);
        Ok(state.signups.len() != before)
    }
    pub async fn close_lfg(pool: &PgPool, guild: &str, id: &str, at: &str) -> Result<bool, Error> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("store.close");
        let Some(post) = state
            .posts
            .iter_mut()
            .find(|post| post.id == id && post.guild_id == guild && post.status == LfgStatus::Open)
        else {
            return Ok(false);
        };
        post.status = LfgStatus::Closed;
        post.closed_at = Some(at.into());
        Ok(true)
    }
    // Close-message repair is outside this signup/audit acceptance slice.
    // Reuse its production state type, but fail loudly if a test reaches I/O.
    pub use crate::real_core::lfg_store::CloseRefreshState;

    pub async fn get_lfg_close_refresh(
        _pool: &PgPool,
        _guild_id: &str,
        _id: &str,
    ) -> Result<Option<CloseRefreshState>, Error> {
        panic!("close repair is outside this offline acceptance slice")
    }

    pub async fn clear_lfg_close_refresh(
        _pool: &PgPool,
        _guild_id: &str,
        _id: &str,
    ) -> Result<(), Error> {
        panic!("close repair is outside this offline acceptance slice")
    }

    pub async fn defer_lfg_close_refresh(
        _pool: &PgPool,
        _guild_id: &str,
        _id: &str,
        _attempts: i32,
        _next_at: time::OffsetDateTime,
    ) -> Result<(), Error> {
        panic!("close repair is outside this offline acceptance slice")
    }

    // The included service also compiles its metadata-edit branch. Keep that
    // store boundary explicit without pretending this signup/audit double tests it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LfgEditOutcome {
        Missing,
        Closed,
        Unchanged { starts_at: String },
        Updated { starts_at: String },
    }

    pub async fn edit_lfg_metadata(
        _pool: &PgPool,
        _guild_id: &str,
        _id: &str,
        _title: Option<&str>,
        _starts_at: Option<&str>,
    ) -> Result<LfgEditOutcome, Error> {
        panic!("metadata edit is outside this offline acceptance slice")
    }

    // Create/nonce recovery is deliberately outside this acceptance slice.
    // Fail loudly if a test accidentally invokes it instead of silently faking it.
    pub async fn put_lfg(
        _pool: &PgPool,
        _post: &LfgPost,
        _roles: &[LfgRole],
        _replace: bool,
    ) -> Result<(), QuotaWriteError> {
        panic!("create is outside this offline acceptance slice")
    }
    pub async fn save_lfg_message_id(
        _pool: &PgPool,
        _guild: &str,
        _id: &str,
        _message: &str,
    ) -> Result<(), Error> {
        panic!("create is outside this offline acceptance slice")
    }
    pub async fn delete_lfg(_pool: &PgPool, _guild: &str, _id: &str) -> Result<bool, Error> {
        panic!("create is outside this offline acceptance slice")
    }
}

pub mod rsvp_store {
    use super::*;
    #[derive(Debug, thiserror::Error)]
    #[error("offline audit fault")]
    pub struct RsvpStoreError;

    pub async fn write_audit(pool: &PgPool, row: &RsvpAudit) -> Result<bool, RsvpStoreError> {
        let mut state = pool.0.lock().expect("fixture state");
        state.trace.push("audit.append");
        state.audit_attempts += 1;
        if state.fail_audit {
            return Err(RsvpStoreError);
        }
        if state.audits.iter().any(|existing| existing.id == row.id) {
            return Ok(false);
        }
        state.audits.push(row.clone());
        Ok(true)
    }
}

#[derive(Debug, Clone)]
pub struct Edit {
    pub channel: String,
    pub message: String,
    pub content: String,
}
#[derive(Debug, Default)]
struct Transport {
    edits: Vec<Edit>,
    fail: bool,
}
#[derive(Debug, Default, Clone)]
pub struct ActionExecutor(Arc<Mutex<Transport>>);
impl ActionExecutor {
    pub fn edits(&self) -> Vec<Edit> {
        self.0.lock().expect("fixture transport").edits.clone()
    }
    pub fn fail_refresh(&self, fail: bool) {
        self.0.lock().expect("fixture transport").fail = fail;
    }
    pub fn with_proxy(_token: String, _proxy: Option<String>) -> Result<Self, DiscordError> {
        Ok(Self::default())
    }
    pub async fn edit_message_with_components(
        &self,
        channel: &str,
        message: &str,
        content: &str,
        _components: &[Value],
    ) -> Result<(), DiscordError> {
        let mut transport = self.0.lock().expect("fixture transport");
        transport.edits.push(Edit {
            channel: channel.into(),
            message: message.into(),
            content: content.into(),
        });
        if transport.fail {
            return Err(DiscordError::Unavailable("offline refresh fault".into()));
        }
        Ok(())
    }
    pub async fn edit_message_for_close_refresh(
        &self,
        _channel: &str,
        _message: &str,
        _content: &str,
        _components: &[Value],
    ) -> crate::CloseRefreshEdit {
        panic!("close repair is outside this offline acceptance slice")
    }
    pub async fn recover_message_by_nonce(
        &self,
        _channel: &str,
        _nonce: &str,
        _bot: u64,
    ) -> Result<Option<String>, DiscordError> {
        panic!("create is outside this offline acceptance slice")
    }
    pub async fn post_message_with_components(
        &self,
        _channel: &str,
        _content: &str,
        _components: &[Value],
        _nonce: &str,
    ) -> Result<String, DiscordError> {
        panic!("create is outside this offline acceptance slice")
    }
}
