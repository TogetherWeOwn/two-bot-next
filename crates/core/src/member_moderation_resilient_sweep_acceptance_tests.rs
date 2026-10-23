#![cfg(test)]
#![forbid(dead_code)]
//! Protected acceptance: the unban sweep finishes every due job and says
//! what happened, even when one job fails.
//!
//! Baseline bug (the paired build owns the fix): `run_due_unbans`
//! (`crates/core/src/member_moderation.rs`) returns `Err` on the first
//! failing job, aborting the tick. Every expired tempban behind one 403 or
//! timeout stalls until the next tick, and the tick reports nothing about
//! the failure: no count, no recorded cause. A second defect hides behind
//! the first: a failing job leaves no audit row, so operators cannot tell a
//! refused unban from a fenced one without reading the sweep error.
//!
//! Protected file: the builder must NEVER edit this file. Return necessary
//! test corrections to the test author. This file must compile both on the
//! baseline and after the builder lands; the trait fallback below provides
//! the new summary surface on the baseline, and an inherent item shadows
//! its fallback once added.
//!
//! Builder contract (all required for GREEN):
//! 1. Keep `run_due_unbans(&self, guild_id)` with its current
//!    `Result<usize, MemberError>` signature, but continue past per-job
//!    Discord failures instead of returning `Err`. Return `Ok(completed)`
//!    counting only the jobs whose DELETE landed. Only fatal errors abort
//!    the tick with `Err`: `BotIdentityUnknown` and store failures from the
//!    claim path. Per-job dispositions are unchanged: `proves_no_effect`
//!    (Rejected, RateLimited) requeues to `pending` for the next sweep;
//!    Timeout/Unavailable stays `running` (fenced for reconciliation).
//!    The existing `safe_sweep_failure_does_not_retry_or_reserve_the_next_job`
//!    test pins the abort; update it to the continue behavior.
//! 2. Add `impl MemberModerationService { pub fn run_due_unbans_summary(&self,
//!    guild_id: &str) -> impl Future<Output = Result<(usize, usize),
//!    MemberError>> + Send }` returning `(completed, failed)`. It runs the
//!    same continuing loop as item 1; `failed` counts the jobs that did not
//!    land (requeued or fenced). Fatal errors return `Err` as in item 1.
//!    The bot-crate tick must switch to this summary and log both counts.
//! 3. Record every failing job with `store.record_audit`: `request_id` is
//!    exactly `"{original_request_id}:unban-failed"`, `outcome` is exactly
//!    `"unban_failed"`, `target_id` is the job's user id, and
//!    `metadata_json` is a JSON object with a string field `"cause"` matching
//!    `^[a-z_]{1,32}$` (lowercase + underscore, bounded). The cause names the
//!    failure class (rejected/rate-limited/timeout/unavailable/store), never
//!    the raw Discord detail. Neither the audit `reason` nor the metadata may
//!    contain the raw Discord detail substring.
//! 4. Success audits are unchanged (`{request}:unban` / `"unbanned"`).
//!
//! Offline: the MemMemberStore tests below need no database. The Postgres
//! test at the bottom self-skips without the approved disposable test
//! database (`MEMBER_TESTDB=agent-testdb` or `ci` in GitHub Actions); it
//! never touches staging or production.
//!
//! Lane placement: this file is a `#[cfg(test)]` module of the core crate
//! (wired at the end of `member_moderation.rs`), not an integration target
//! under `tests/`. The lib/bins lanes (nextest, rust tests) execute it on
//! every head; the integration step never runs on a red head because the
//! lib/bins step fails first, so a `tests/` target could never show RED.
//!
//! Expected results on the baseline: `sweep_continues_past_rejected_job_
//! and_reports_counts`, `sweep_continues_past_rate_limited_job_and_requeues_
//! it`, `sweep_fences_uncertain_timeout_and_completes_the_rest` and
//! `run_due_unbans_continues_and_returns_completed` RED (the sweep returns
//! `Err` after one Discord call and writes no failure audit); the
//! `*_control` tests GREEN (all-success and empty sweeps already return
//! counts). The Postgres test REDs for the same abort reason wherever the
//! `MEMBER_TESTDB` guard is set, and self-skips (pass with a message)
//! elsewhere.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::member_moderation::{
    DiscordCall, DiscordError, MemMemberStore, MemberDiscord, MemberError, MemberModerationService,
    MemberModerationStore,
};
use crate::ModerationPolicy;

const GUILD: &str = "100000000000000001";
const NOW: &str = "2023-11-14T22:13:20.000Z";
/// Backdated expiry: safely under the fixed test clock (`NOW`), so every
/// staged job is due without depending on `<=`-at-equality.
const DUE: &str = "2023-11-14T22:00:00.000Z";

/// Hostile/verbose Discord detail: mentions, pill, newlines, a long run and
/// a provider-shaped code. It must never reach a reply or an audit row; only
/// the bounded cause is recorded.
const HOSTILE_DETAIL: &str = "build: 403 missing permissions (code 50013) @everyone <@999> line1\nline2 verbose-TOKEN-abc123";
const VERBOSE_RUN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "123456789012345678".into(),
        bot_user_id: Some("555555555555555555".into()),
        protected_role_ids: HashSet::new(),
    }
}

/// Baseline fallback for the builder's `run_due_unbans_summary` (contract
/// item 2). Adapts the legacy `run_due_unbans`: `Ok(completed)` becomes
/// `(completed, 0)`, and any `Err` stays `Err`. On the baseline a failing
/// sweep returns `Err`, so every summary assertion REDs; once the builder
/// adds the inherent `run_due_unbans_summary` it shadows this fallback.
trait SweepSummaryFallback {
    async fn run_due_unbans_summary(&self, guild_id: &str) -> Result<(usize, usize), MemberError>;
}

impl<D, S, F> SweepSummaryFallback for MemberModerationService<D, S, F>
where
    D: MemberDiscord,
    S: MemberModerationStore,
    F: Fn() -> i64 + Send + Sync,
{
    async fn run_due_unbans_summary(&self, guild_id: &str) -> Result<(usize, usize), MemberError> {
        self.run_due_unbans(guild_id)
            .await
            .map(|completed| (completed, 0))
    }
}

/// [`MemberDiscord`] double that fails unban for one configured user with a
/// fixed error and succeeds for everyone else. Records every call in order.
#[derive(Clone, Default)]
struct FailUserDiscord {
    inner: Arc<Mutex<FailInner>>,
}

#[derive(Default)]
struct FailInner {
    calls: Vec<DiscordCall>,
    fail_user: Option<String>,
    fail_error: Option<DiscordError>,
}

impl FailUserDiscord {
    fn failing_for(user_id: &str, err: DiscordError) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FailInner {
                calls: Vec::new(),
                fail_user: Some(user_id.to_owned()),
                fail_error: Some(err),
            })),
        }
    }

    fn unban_calls(&self) -> Vec<DiscordCall> {
        self.inner
            .lock()
            .expect("fail-user discord lock")
            .calls
            .iter()
            .filter(|c| matches!(c, DiscordCall::Unban { .. }))
            .cloned()
            .collect()
    }

    fn unban_user_ids(&self) -> Vec<String> {
        self.unban_calls()
            .iter()
            .filter_map(|c| match c {
                DiscordCall::Unban { user_id, .. } => Some(user_id.clone()),
                _ => None,
            })
            .collect()
    }
}

impl MemberDiscord for FailUserDiscord {
    async fn ban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.inner
            .lock()
            .expect("lock")
            .calls
            .push(DiscordCall::Ban {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                reason: reason.to_owned(),
            });
        Ok(())
    }

    async fn unban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        let outcome = {
            let mut inner = self.inner.lock().expect("lock");
            inner.calls.push(DiscordCall::Unban {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                reason: reason.to_owned(),
            });
            if inner.fail_user.as_deref() == Some(user_id) {
                inner.fail_error.clone()
            } else {
                None
            }
        };
        match outcome {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    async fn kick(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.inner
            .lock()
            .expect("lock")
            .calls
            .push(DiscordCall::Kick {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                reason: reason.to_owned(),
            });
        Ok(())
    }

    async fn timeout(
        &self,
        guild_id: &str,
        user_id: &str,
        until_iso: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        self.inner
            .lock()
            .expect("lock")
            .calls
            .push(DiscordCall::Timeout {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                until_iso: until_iso.to_owned(),
                reason: reason.to_owned(),
            });
        Ok(())
    }
}

/// Stage, accept and activate three due unbans in generation order: user-1,
/// user-2, user-3 with request ids req-1, req-2, req-3. This mirrors the end
/// state of the real TempBan execute path: `stage_unban` leaves `staged`,
/// `confirm_ban_attempt` accepts the ban intent, and `activate_staged_unban`
/// moves the expiry to `pending` (see the `sweep_requeues_*` setups).
async fn stage_three_pending(store: &MemMemberStore) {
    for (user, req) in [
        ("user-1", "req-1"),
        ("user-2", "req-2"),
        ("user-3", "req-3"),
    ] {
        let attempt = store
            .stage_unban(GUILD, user, DUE, "expiry", req, NOW)
            .await
            .expect("stage");
        store
            .confirm_ban_attempt(GUILD, user, req, attempt, NOW)
            .await
            .expect("confirm");
        store
            .activate_staged_unban(GUILD, user, req, NOW)
            .await
            .expect("activate");
        assert_eq!(store.unban_state(req).as_deref(), Some("pending"));
    }
}

fn failure_audit_for(audits: &[crate::member_moderation::AuditRow], req: &str) -> Option<usize> {
    audits
        .iter()
        .position(|row| row.request_id == format!("{req}:unban-failed"))
}

/// The recorded-failure shape from contract item 3: exact ids, bounded cause,
/// never the raw Discord detail.
fn assert_failure_recorded(
    audits: &[crate::member_moderation::AuditRow],
    req: &str,
    user: &str,
    raw_detail: &str,
) {
    let Some(index) = failure_audit_for(audits, req) else {
        panic!("sweep must record the failing job {req} as {req}:unban-failed; audits: {audits:?}");
    };
    let row = &audits[index];
    assert_eq!(
        row.outcome, "unban_failed",
        "failure outcome must be unban_failed"
    );
    assert_eq!(
        row.target_id.as_deref(),
        Some(user),
        "failure audit names the failed user"
    );
    let metadata: HashMap<String, serde_json::Value> =
        serde_json::from_str(&row.metadata_json).expect("failure metadata must be JSON");
    let Some(serde_json::Value::String(cause)) = metadata.get("cause") else {
        panic!(
            "failure metadata must carry a string cause: {}",
            row.metadata_json
        );
    };
    assert!(
        !cause.is_empty()
            && cause.len() <= 32
            && cause.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
        "cause must be bounded lowercase+underscore, got {cause:?}"
    );
    for leaked in [raw_detail, "@everyone", "50013", VERBOSE_RUN] {
        assert!(
            !row.reason.contains(leaked),
            "failure audit reason must not leak raw detail {leaked:?}: {:?}",
            row.reason
        );
        assert!(
            !row.metadata_json.contains(leaked),
            "failure audit metadata must not leak raw detail {leaked:?}: {}",
            row.metadata_json
        );
    }
}

#[tokio::test]
async fn sweep_continues_past_rejected_job_and_reports_counts() {
    let store = MemMemberStore::new();
    stage_three_pending(&store).await;
    let detail = format!("{HOSTILE_DETAIL} {VERBOSE_RUN}");
    let discord = FailUserDiscord::failing_for("user-2", DiscordError::Rejected(detail.clone()));
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let summary = svc
        .run_due_unbans_summary(GUILD)
        .await
        .expect("a rejected job must not abort the sweep");
    assert_eq!(summary, (2, 1), "two land, one fails");
    assert_eq!(discord.unban_calls().len(), 3, "every due job is attempted");
    assert_eq!(discord.unban_user_ids(), vec!["user-1", "user-2", "user-3"]);
    assert_eq!(store.unban_state("req-1").as_deref(), Some("done"));
    assert_eq!(
        store.unban_state("req-2").as_deref(),
        Some("pending"),
        "a proven no-effect refusal requeues"
    );
    assert_eq!(store.unban_state("req-3").as_deref(), Some("done"));
    assert_failure_recorded(&store.audits(), "req-2", "user-2", &detail);
}

#[tokio::test]
async fn sweep_continues_past_rate_limited_job_and_requeues_it() {
    let store = MemMemberStore::new();
    stage_three_pending(&store).await;
    let discord = FailUserDiscord::failing_for("user-2", DiscordError::RateLimited);
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let summary = svc
        .run_due_unbans_summary(GUILD)
        .await
        .expect("a rate-limited job must not abort the sweep");
    assert_eq!(summary, (2, 1));
    assert_eq!(discord.unban_calls().len(), 3);
    assert_eq!(store.unban_state("req-2").as_deref(), Some("pending"));
    let audits = store.audits();
    assert!(
        failure_audit_for(&audits, "req-2").is_some(),
        "a rate-limited job is recorded; audits: {audits:?}"
    );
}

#[tokio::test]
async fn sweep_fences_uncertain_timeout_and_completes_the_rest() {
    let store = MemMemberStore::new();
    stage_three_pending(&store).await;
    let detail = format!("upstream timeout {VERBOSE_RUN}");
    let discord = FailUserDiscord::failing_for("user-2", DiscordError::Timeout);
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let summary = svc
        .run_due_unbans_summary(GUILD)
        .await
        .expect("an uncertain job must not abort the sweep");
    assert_eq!(summary, (2, 1));
    assert_eq!(discord.unban_calls().len(), 3);
    assert_eq!(store.unban_state("req-1").as_deref(), Some("done"));
    assert_eq!(
        store.unban_state("req-2").as_deref(),
        Some("running"),
        "uncertain stays fenced for reconciliation"
    );
    assert_eq!(store.unban_state("req-3").as_deref(), Some("done"));
    assert_failure_recorded(&store.audits(), "req-2", "user-2", &detail);
}

#[tokio::test]
async fn run_due_unbans_continues_and_returns_completed() {
    // The legacy entry point keeps its signature but adopts the continuing
    // behavior: `Ok` counts only landed jobs, never `Err` for per-job Discord
    // failures.
    let store = MemMemberStore::new();
    stage_three_pending(&store).await;
    let discord = FailUserDiscord::failing_for(
        "user-2",
        DiscordError::Rejected("403 missing permissions".to_owned()),
    );
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let completed = svc
        .run_due_unbans(GUILD)
        .await
        .expect("per-job failure must not abort");
    assert_eq!(completed, 2);
    assert_eq!(discord.unban_calls().len(), 3);
}

#[tokio::test]
async fn all_succeeding_sweep_reports_zero_failed_control() {
    let store = MemMemberStore::new();
    stage_three_pending(&store).await;
    let discord = FailUserDiscord::default();
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let summary = svc
        .run_due_unbans_summary(GUILD)
        .await
        .expect("clean sweep");
    assert_eq!(summary, (3, 0));
    assert_eq!(
        svc.run_due_unbans(GUILD)
            .await
            .expect("second sweep is empty"),
        0
    );
}

#[tokio::test]
async fn empty_sweep_reports_zero_zero_control() {
    let store = MemMemberStore::new();
    let discord = FailUserDiscord::default();
    let svc = MemberModerationService::new(discord, store, policy(), || 1_700_000_000_000);
    let summary = svc
        .run_due_unbans_summary(GUILD)
        .await
        .expect("empty sweep");
    assert_eq!(summary, (0, 0));
}

// Explicit trait calls keep the baseline adapter exercised after the inherent
// summary shadows it. Acceptance assertions above must keep using method syntax.
#[tokio::test]
async fn legacy_summary_fallback_counts_completed_control() {
    let store = MemMemberStore::new();
    stage_three_pending(&store).await;
    let discord = FailUserDiscord::default();
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let summary = SweepSummaryFallback::run_due_unbans_summary(&svc, GUILD)
        .await
        .expect("legacy adapter forwards a successful sweep");
    assert_eq!(summary, (3, 0));
    assert_eq!(discord.unban_user_ids(), vec!["user-1", "user-2", "user-3"]);
    for req in ["req-1", "req-2", "req-3"] {
        assert_eq!(store.unban_state(req).as_deref(), Some("done"));
    }
    assert_eq!(
        SweepSummaryFallback::run_due_unbans_summary(&svc, GUILD)
            .await
            .expect("legacy adapter forwards an empty sweep"),
        (0, 0)
    );
}

#[tokio::test]
async fn legacy_summary_fallback_preserves_fatal_error_control() {
    let store = MemMemberStore::new();
    stage_three_pending(&store).await;
    let discord = FailUserDiscord::default();
    let mut unknown_identity = policy();
    unknown_identity.bot_user_id = None;
    let svc =
        MemberModerationService::new(discord.clone(), store.clone(), unknown_identity, || {
            1_700_000_000_000
        });
    assert!(matches!(
        SweepSummaryFallback::run_due_unbans_summary(&svc, GUILD).await,
        Err(MemberError::BotIdentityUnknown)
    ));
    assert!(discord.unban_calls().is_empty());
    for req in ["req-1", "req-2", "req-3"] {
        assert_eq!(store.unban_state(req).as_deref(), Some("pending"));
    }
}

// --- Disposable-DB sweep proof (self-skips without MEMBER_TESTDB) -------------

#[cfg(feature = "db")]
use crate::member_moderation_store::PgMemberModerationStore;
#[cfg(feature = "db")]
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
#[cfg(feature = "db")]
use sqlx::{PgPool, Postgres, QueryBuilder};
#[cfg(feature = "db")]
use std::time::Duration;

#[cfg(feature = "db")]
async fn db_pool() -> Option<(PgPool, PgPool, String)> {
    let host = match std::env::var("MEMBER_TESTDB").as_deref() {
        Ok("agent-testdb") => "agent-testdb",
        Ok("ci") if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") => "localhost",
        _ => return None,
    };
    let options = PgConnectOptions::new()
        .host(host)
        .port(5432)
        .username("agent_test")
        .password("")
        .database("postgres");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone())
        .await
        .expect("approved test DB must connect; no credential fallback");
    let schema = format!("member_sweep_acceptance_{:032x}", rand::random::<u128>());
    QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await
        .expect("test schema");
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let query = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(query)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .expect("schema pool");
    for migration in [
        include_str!("../../cutover/migrations/0110_moderation_member.sql"),
        include_str!("../../cutover/migrations/0111_moderation_ban_ownership.sql"),
        include_str!("../../cutover/migrations/0112_moderation_legacy_timestamps.sql"),
        include_str!("../../cutover/migrations/0113_moderation_unban_retry_order.sql"),
    ] {
        sqlx::raw_sql(migration)
            .execute(&pool)
            .await
            .expect("migration");
    }
    Some((admin, pool, schema))
}

#[cfg(feature = "db")]
async fn db_unban_state(pool: &PgPool, req: &str) -> String {
    sqlx::query_scalar("SELECT state FROM moderation_scheduled_unbans WHERE request_id = $1")
        .bind(req)
        .fetch_one(pool)
        .await
        .expect("schedule state")
}

#[cfg(feature = "db")]
#[tokio::test]
async fn postgres_sweep_continues_past_rejected_job() {
    let Some((admin, pool, schema)) = db_pool().await else {
        eprintln!("skipping member-moderation sweep test: MEMBER_TESTDB not set");
        return;
    };
    let store = PgMemberModerationStore::new(pool.clone(), GUILD.to_owned());
    for (user, req) in [
        ("user-1", "req-1"),
        ("user-2", "req-2"),
        ("user-3", "req-3"),
    ] {
        let attempt = store
            .stage_unban(GUILD, user, DUE, "expiry", req, NOW)
            .await
            .expect("stage");
        store
            .confirm_ban_attempt(GUILD, user, req, attempt, NOW)
            .await
            .expect("confirm");
        store
            .activate_staged_unban(GUILD, user, req, NOW)
            .await
            .expect("activate");
    }
    let discord = FailUserDiscord::failing_for(
        "user-2",
        DiscordError::Rejected("403 missing permissions".to_owned()),
    );
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let summary = svc
        .run_due_unbans_summary(GUILD)
        .await
        .expect("a rejected job must not abort the Postgres sweep");
    assert_eq!(summary, (2, 1));
    assert_eq!(discord.unban_user_ids().len(), 3, "every due job attempted");
    assert_eq!(db_unban_state(&pool, "req-1").await, "done");
    assert_eq!(db_unban_state(&pool, "req-3").await, "done");
    // The refused job requeues for the next sweep; its bounded-cause audit is
    // asserted offline above (same record path through the shared trait).
    assert_eq!(db_unban_state(&pool, "req-2").await, "pending");
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop test schema");
    admin.close().await;
    pool.close().await;
}
