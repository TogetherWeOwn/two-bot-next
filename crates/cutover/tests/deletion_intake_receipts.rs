//! Authenticated deletion intake + durable requester receipts (failing first, tests only).
//!
//! Base: `0fa0ba058932ca7780b9adbfbfe31209bec22683` (merged main, no intake caller).
//! At that base `request_user_deletion` / `fulfill_user_deletion` / `run_retention`
//! exist in `crates/cutover/src/retention.rs` with direct integration-test calls,
//! but no production intake/drain caller wires them to a member entrypoint. The
//! `0438` ledger holds pending/completed rows and `retention.rs` settles receipts
//! transactionally, which proves a module, not reachable intake, actual delivery,
//! or under-72-hour service. Deferred/overdue signals already exist; this file
//! reuses them and never claims otherwise.
//!
//! These tests are RED on the current head: there is no `deletion_intake`
//! module yet, so this target fails to compile with `E0432 unresolved import`.
//! That is the missing-intake proof. The builder sibling implements production
//! code only and must NOT edit this file; these tests pass unmodified once the
//! contract below exists. No failing test merges: this file is handed off
//! unpublished for the builder's final green implementation pull request.
//!
//! ## Builder contract (implement exactly this surface in `two-bot-cutover`)
//!
//! Create `crates/cutover/src/deletion_intake.rs` and expose it as
//! `pub mod deletion_intake` in `crates/cutover/src/lib.rs`:
//!
//! ```rust,ignore
//! use sqlx::PgPool;
//! use two_bot_core::CommandDefinition;
//!
//! pub struct DeletionAck {
//!     pub request_id: String,
//!     pub status: String, // always "pending"
//! }
//! pub struct DeletionStatus {
//!     pub request_id: String,
//!     pub guild_id: String,
//!     pub user_id: String,
//!     pub status: String, // "pending" | "completed"
//!     pub requested_at_unix: i64,
//!     pub completed_at_unix: Option<i64>,
//!     pub overdue: bool,
//! }
//! pub struct DeletionReceiptView {
//!     pub request_id: String,
//!     pub receipt_id: String,
//!     pub guild_id: String,
//!     pub user_id: String,
//!     pub requested_at_unix: i64,
//!     pub completed_at_unix: i64,
//!     pub scope: String,           // guild/member footprint, never "global"
//!     pub retained_limits: String, // disclosed control-plane exceptions, never empty
//! }
//! pub async fn request_member_deletion(
//!     pool: &PgPool,
//!     guild_id: &str,
//!     requester_id: &str,
//!     interaction_id: &str,
//! ) -> Result<DeletionAck, sqlx::Error>;
//! pub async fn deletion_status_for_requester(
//!     pool: &PgPool,
//!     guild_id: &str,
//!     requester_id: &str,
//!     request_id: &str,
//! ) -> Result<DeletionStatus, sqlx::Error>;
//! pub async fn deletion_receipt_for_requester(
//!     pool: &PgPool,
//!     guild_id: &str,
//!     requester_id: &str,
//!     request_id: &str,
//! ) -> Result<DeletionReceiptView, sqlx::Error>;
//! pub fn privacy_deletion_command() -> CommandDefinition;
//! ```
//!
//! ## Required integration seam (builder owns this, tests only assert it)
//!
//! * `request_member_deletion` binds subject and scope to the authenticated
//!   guild/requester pair, validates snowflakes and the interaction id, and
//!   invokes the existing durable request ledger (the same rows `request_user_deletion`
//!   writes), never the narrower opt-out purge. A retry with the same
//!   interaction id returns the same pending request without queueing a second
//!   destructive effect. Acknowledgement is `pending` only, never completion.
//! * Status and receipt retrieval take only guild/requester/request ids (no
//!   bearer token), so they stay usable after the interaction token expires.
//!   Cross-member and cross-guild reads fail without leaking another member's
//!   data. Request ids are validated (nonempty, at most 128 chars, no control
//!   characters, must exist and belong to the caller).
//! * `privacy_deletion_command` returns the member `/privacy` definition that
//!   carries the deletion request/status entrypoint: name `privacy`, guild-only
//!   (`dm_permission == false`), no caller-supplied user/member/target option,
//!   description mentioning deletion or request. The live publish set must
//!   contain `privacy` and the core router must route it to a handler for a
//!   member with no special permissions. No new unauthenticated HTTP delete
//!   route.
//! * Draining reuses the existing scheduler: pending requests complete through
//!   `run_retention(Execute)`, never by calling fulfil directly in the
//!   happy-path proof. Guard refusals leave the request pending with no receipt;
//!   overdue pendings surface explicitly and resolve only through legitimate
//!   fence settlement, never by bypass.
//! * The receipt's `scope` names the guild/member footprint and never claims
//!   global erasure; `retained_limits` discloses the retained control-plane
//!   exceptions and is never empty. Other members and other guilds survive.
//!
//! ## Table mapping on the current schema
//!
//! * Member footprint = every erasure-plan table's guild/member rows (at least
//!   `members`, `member_levels`, voice-session `events` for the requester).
//! * Ledger = `retention_deletion_requests` (pending/completed + receipt).
//!   `retention_guild_purge_queue` drains alongside, leaving the ledger to the
//!   receipt path so a departed guild's pending receipt still completes.
//! * Retained control-plane records stay retained: `privacy_opt_outs`
//!   suppression, operator-released `moderation_idempotency` replay fence and
//!   its executions, plus the documented guild/audit exceptions. Dry run
//!   reports but deletes nothing and fabricates no receipt.
//!
//! ## Reuse boundary (adjacent work, not re-implemented here)
//!
//! Member opt-out suppression/purge semantics belong to `privacy`; guild
//! removal/rejoin, runtime scheduling and retry signals belong to the runtime
//! slice; policy publication belongs to the web surface. This file reuses their
//! real APIs (`opt_out_member`, `handle_guild_delete`, `run_retention`) and
//! asserts non-interference, never rebuilding them. Existing independent
//! rolling-window, fence, and opt-out suites stay untouched.
//!
//! ## Pinned coverage limits (do not silently claim more)
//!
//! * Voice-raw coverage is pinned to voice-session `events` rows seeded here;
//!   the append-only event-store capture persists without age TTL and leaves
//!   per-user via the erasure plan, per-guild via the purge queue.
//! * Producer paths are NOT covered: gateway ingestion cadence, backfill/replay
//!   writes, and the live-gateway call sites that must invoke the intake on real
//!   dispatches. The builder wires those call sites; live-gateway proof is
//!   follow-up work.
//! * Fixed fake snowflakes only. Disposable test databases via
//!   `two-bot-testsupport`; never staging or production bindings, never real
//!   user data, no secrets in code or comments.

use two_bot_cutover::deletion_intake::{
    deletion_receipt_for_requester, deletion_status_for_requester, privacy_deletion_command,
    request_member_deletion,
};
use two_bot_cutover::privacy::opt_out_member;
use two_bot_cutover::retention::{handle_guild_delete, run_retention, RetentionMode};
use two_bot_testsupport::TestDatabase;

const GUILD_A: &str = "711111111111111111";
const GUILD_B: &str = "722222222222222222";
const MEMBER: &str = "744444444444444444";
const OTHER_MEMBER: &str = "755555555555555555";

fn guild_u64() -> u64 {
    GUILD_A.parse().expect("fixture guild is a snowflake")
}

fn router_all_on() -> two_bot_core::InteractionRouter {
    two_bot_core::InteractionRouter::new(two_bot_core::RouterGates {
        configured_guild: Some(guild_u64()),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    })
}

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the deletion intake database suite"
            );
            eprintln!("SKIP deletion intake integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .expect("disposable migrated database"),
    )
}

async fn count(pool: &sqlx::PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count query")
}

async fn seed_footprint(pool: &sqlx::PgPool, guild: &str, member: &str, key: &str) {
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(guild)
        .bind(member)
        .execute(pool)
        .await
        .expect("seed member row");
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
         VALUES ($1, $2, 150, 150, now())",
    )
    .bind(guild)
    .bind(member)
    .execute(pool)
    .await
    .expect("seed aggregate row");
    sqlx::query(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES ('voice_session_started', $2, $1, now(), 'fixture', '{}', $3)",
    )
    .bind(guild)
    .bind(member)
    .bind(key)
    .execute(pool)
    .await
    .expect("seed event row");
}

fn option_names(def: &two_bot_core::CommandDefinition) -> Vec<String> {
    let mut names = Vec::new();
    for opt in &def.options {
        names.push(opt.name.to_lowercase());
        for nested in &opt.options {
            names.push(nested.name.to_lowercase());
        }
    }
    names
}

fn option_kinds(def: &two_bot_core::CommandDefinition) -> Vec<u8> {
    let mut kinds = Vec::new();
    for opt in &def.options {
        kinds.push(opt.kind);
        for nested in &opt.options {
            kinds.push(nested.kind);
        }
    }
    kinds
}

/// Registration/publication -> gateway dispatch -> authenticated adapter ->
/// persisted queue. The request must invoke the durable ledger, not the
/// narrower opt-out purge: rows survive the acknowledgement.
#[tokio::test]
async fn privacy_command_publishes_and_routes_to_authenticated_intake() {
    let def = privacy_deletion_command();
    assert_eq!(def.name, "privacy", "the member entrypoint reuses /privacy");
    assert!(
        !def.dm_permission,
        "deletion requests are guild-scoped, never DMs"
    );
    let names = option_names(&def);
    for banned in ["user", "member", "target", "user_id", "member_id"] {
        assert!(
            !names.iter().any(|n| n == banned),
            "subject binds to the authenticated requester, no caller-supplied {banned}"
        );
    }
    let haystack = format!(
        "{} {}",
        def.description,
        def.options
            .iter()
            .map(|o| o.description.clone())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_lowercase();
    assert!(
        haystack.contains("delet") || haystack.contains("request"),
        "the /privacy definition must advertise the deletion request entrypoint"
    );
    let published = two_bot_discord::publish_commands(std::slice::from_ref(&def));
    assert!(
        published.iter().any(|c| c.name == "privacy"),
        "the deletion entrypoint must publish to Discord"
    );
    let router = router_all_on();
    let live = router.publish_set(&[]).expect("publish set builds");
    assert!(
        live.iter().any(|d| d.name == "privacy"),
        "the live publish set must contain /privacy"
    );
    let outcome = router.route_slash(&two_bot_core::SlashContext {
        name: "privacy",
        guild_id: Some(guild_u64()),
        actor_permissions: Some(0),
        custom_row: None,
    });
    assert!(
        matches!(outcome, two_bot_core::SlashOutcome::Handled { .. }),
        "a member with no special permissions must route /privacy, got {outcome:?}"
    );

    let Some(db) = database().await else { return };
    let pool = db.pool();
    for (guild, member, key) in [
        (GUILD_A, MEMBER, "intake-route-own"),
        (GUILD_A, OTHER_MEMBER, "intake-route-other"),
        (GUILD_B, MEMBER, "intake-route-elsewhere"),
    ] {
        seed_footprint(pool, guild, member, key).await;
    }

    let ack = request_member_deletion(pool, GUILD_A, MEMBER, "intake-fixture-1")
        .await
        .expect("authenticated request queues");
    assert!(!ack.request_id.is_empty(), "the request must have an id");
    assert_eq!(
        ack.status, "pending",
        "acknowledgement is accepted/pending, never completion"
    );
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND guild_id = $2 AND user_id = $3 AND status = 'pending'",
    )
    .bind(&ack.request_id)
    .bind(GUILD_A)
    .bind(MEMBER)
    .fetch_one(pool)
    .await
    .expect("ledger row");
    assert_eq!(queued, 1, "the adapter must persist one pending queue row");
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM members WHERE guild_id = '711111111111111111'"
        )
        .await,
        2,
        "request acknowledgement must not purge like opt-out does"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM events WHERE guild_id = '711111111111111111' AND member_id = '744444444444444444'"
        )
        .await,
        1,
        "the requester's rows stay until the scheduler drains"
    );
    db.close().await.expect("verify teardown");
}

/// Subject and scope bind to the authenticated guild/requester pair. Option
/// surface carries no user target, and cross-member/cross-guild reads plus bad
/// request ids fail without leaking another member's data.
#[tokio::test]
async fn deletion_subject_binds_to_requester_not_caller_target() {
    let def = privacy_deletion_command();
    let names = option_names(&def);
    let kinds = option_kinds(&def);
    for (name, kind) in names.iter().zip(kinds.iter()) {
        // CommandOptionType::User is 6; a user picker would be a caller-supplied target.
        assert!(
            !(*kind == 6
                && ["user", "member", "target", "user_id", "member_id"].contains(&name.as_str())),
            "no user-picker target option is allowed"
        );
    }

    let Some(db) = database().await else { return };
    let pool = db.pool();
    seed_footprint(pool, GUILD_A, MEMBER, "intake-bind-own").await;
    seed_footprint(pool, GUILD_A, OTHER_MEMBER, "intake-bind-other").await;
    seed_footprint(pool, GUILD_B, MEMBER, "intake-bind-elsewhere").await;

    let ack = request_member_deletion(pool, GUILD_A, MEMBER, "intake-fixture-bind")
        .await
        .expect("request queues");
    let long_id = "x".repeat(129);
    for (guild, requester, request_id) in [
        (GUILD_A, OTHER_MEMBER, ack.request_id.as_str()),
        (GUILD_B, MEMBER, ack.request_id.as_str()),
        (GUILD_A, MEMBER, ""),
        (GUILD_A, MEMBER, "   "),
        (GUILD_A, MEMBER, "has\ncontrol"),
        (GUILD_A, MEMBER, long_id.as_str()),
        (GUILD_A, MEMBER, "del-does-not-exist"),
    ] {
        assert!(
            deletion_status_for_requester(pool, guild, requester, request_id)
                .await
                .is_err(),
            "status read must fail for {guild}/{requester}/{request_id:?}"
        );
        assert!(
            deletion_receipt_for_requester(pool, guild, requester, request_id)
                .await
                .is_err(),
            "receipt read must fail for {guild}/{requester}/{request_id:?}"
        );
    }
    let status = deletion_status_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
        .await
        .expect("owner read succeeds");
    assert_eq!(status.guild_id, GUILD_A);
    assert_eq!(status.user_id, MEMBER);
    assert!(
        !status.guild_id.contains(OTHER_MEMBER) && !status.user_id.contains(OTHER_MEMBER),
        "the status must not leak another member's data"
    );
    db.close().await.expect("verify teardown");
}

/// Interaction retries are idempotent: the same interaction id returns the same
/// pending request without queueing a second destructive effect.
#[tokio::test]
async fn interaction_retries_are_idempotent_without_duplicate_queue() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    seed_footprint(pool, GUILD_A, MEMBER, "intake-retry-own").await;

    let first = request_member_deletion(pool, GUILD_A, MEMBER, "intake-retry-1")
        .await
        .expect("first attempt queues");
    let second = request_member_deletion(pool, GUILD_A, MEMBER, "intake-retry-1")
        .await
        .expect("interaction retry is idempotent");
    assert_eq!(
        first.request_id, second.request_id,
        "the same interaction must return the same request"
    );
    assert_eq!(second.status, "pending");
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .fetch_one(pool)
    .await
    .expect("queue count");
    assert_eq!(
        rows, 1,
        "a retry must not queue a duplicate destructive effect"
    );
    for (guild, requester, interaction) in [
        ("not-a-snowflake", MEMBER, "intake-retry-bad"),
        (GUILD_A, "0", "intake-retry-bad"),
        (GUILD_A, MEMBER, ""),
        (GUILD_A, MEMBER, "has\ncontrol"),
    ] {
        assert!(
            request_member_deletion(pool, guild, requester, interaction)
                .await
                .is_err(),
            "invalid identities and interaction ids must be refused before any write"
        );
    }
    db.close().await.expect("verify teardown");
}

/// Acknowledgement is pending only. Status and receipt stay usable without any
/// bearer token, the ledger stores no token column, and the completed receipt
/// identifies request/scope/timestamps/retained limits without cross-member leak.
#[tokio::test]
async fn ack_is_pending_with_durable_requester_receipt_after_token_expiry() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    for (guild, member, key) in [
        (GUILD_A, MEMBER, "intake-durable-own"),
        (GUILD_A, OTHER_MEMBER, "intake-durable-other"),
        (GUILD_B, MEMBER, "intake-durable-elsewhere"),
    ] {
        seed_footprint(pool, guild, member, key).await;
    }

    let ack = request_member_deletion(pool, GUILD_A, MEMBER, "intake-fixture-durable")
        .await
        .expect("request queues");
    assert_eq!(ack.status, "pending");
    let status = deletion_status_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
        .await
        .expect("durable status without any token");
    assert_eq!(status.status, "pending");
    assert!(
        status.requested_at_unix > 0,
        "the request time must be recorded"
    );
    assert_eq!(status.completed_at_unix, None);
    assert!(
        deletion_receipt_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
            .await
            .is_err(),
        "no receipt may exist while pending"
    );
    let token_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'retention_deletion_requests'",
    )
    .fetch_all(pool)
    .await
    .expect("ledger columns");
    assert!(
        token_columns
            .iter()
            .all(|c| !c.to_lowercase().contains("token") && !c.to_lowercase().contains("bearer")),
        "the durable ledger must not store transient bearer tokens, got {token_columns:?}"
    );

    let report = run_retention(pool, RetentionMode::Execute)
        .await
        .expect("scheduler drains");
    assert_eq!(
        report.deletions_completed, 1,
        "the drain must complete the request, got {report:?}"
    );
    let status = deletion_status_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
        .await
        .expect("status after completion");
    assert_eq!(status.status, "completed");
    assert!(status.completed_at_unix.is_some());
    let receipt = deletion_receipt_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
        .await
        .expect("durable receipt after the interaction token expired");
    assert_eq!(receipt.request_id, ack.request_id);
    assert!(!receipt.receipt_id.is_empty());
    assert_eq!(receipt.guild_id, GUILD_A);
    assert_eq!(receipt.user_id, MEMBER);
    assert!(receipt.completed_at_unix >= receipt.requested_at_unix);
    assert!(
        receipt.scope.contains(GUILD_A) && receipt.scope.contains(MEMBER),
        "the receipt must identify its guild/member scope, got {:?}",
        receipt.scope
    );
    assert!(
        !receipt.scope.to_lowercase().contains("global"),
        "never advertise global erasure for a guild/member footprint"
    );
    assert!(
        !receipt.retained_limits.is_empty(),
        "the receipt must disclose retained limits"
    );
    assert!(
        !receipt.retained_limits.contains(OTHER_MEMBER) && !receipt.scope.contains(OTHER_MEMBER),
        "the receipt must not leak another member's data"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '722222222222222222'"
        )
        .await,
        1,
        "the other guild's rows survive"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '711111111111111111' AND member_id = '755555555555555555'"
        )
        .await,
        1,
        "the other member's rows survive"
    );
    db.close().await.expect("verify teardown");
}

/// Happy-path proof reuses the scheduler across a restart: request on one pool,
/// drain on an independent pool, then read the requester-visible receipt. Never
/// by calling fulfil directly. Elapsed stays under 72 hours with a bounded run.
#[tokio::test]
async fn drain_via_restart_completes_within_72h_without_direct_fulfil() {
    let Some(db) = database().await else { return };
    seed_footprint(db.pool(), GUILD_A, MEMBER, "intake-restart-own").await;
    seed_footprint(db.pool(), GUILD_A, OTHER_MEMBER, "intake-restart-other").await;

    // Request through the authenticated adapter, not the raw storage call.
    let ack = request_member_deletion(db.pool(), GUILD_A, MEMBER, "intake-fixture-restart")
        .await
        .expect("request queues");
    let peer = db
        .independent_pool()
        .await
        .expect("independent pool for restart");
    // Restart/retry drain: the scheduler completes the pending request on the
    // peer pool. This test never calls fulfil directly.
    let report = run_retention(&peer, RetentionMode::Execute)
        .await
        .expect("restart drain");
    assert_eq!(
        report.deletions_completed, 1,
        "one restart drain must complete the request, got {report:?}"
    );
    assert!(
        report.duration_ms < 60_000,
        "the bounded queue delay must stay bounded, got {report:?}"
    );
    let receipt = deletion_receipt_for_requester(db.pool(), GUILD_A, MEMBER, &ack.request_id)
        .await
        .expect("requester-visible receipt after restart");
    assert_eq!(receipt.request_id, ack.request_id);
    assert!(
        receipt.completed_at_unix - receipt.requested_at_unix < 72 * 3600,
        "elapsed service must stay under 72 hours"
    );
    assert_eq!(
        count(
            db.pool(),
            "SELECT count(*) FROM members WHERE guild_id = '711111111111111111' AND member_id = '744444444444444444'"
        )
        .await,
        0,
        "the requester's member row must be transactionally erased with the receipt"
    );
    assert_eq!(
        count(
            db.pool(),
            "SELECT count(*) FROM members WHERE guild_id = '711111111111111111' AND member_id = '755555555555555555'"
        )
        .await,
        1,
        "the other member survives the transactional erase"
    );
    peer.close().await;
    db.close().await.expect("verify teardown");
}

/// Guard refusal plus transient retry: pending stays pending with no false
/// receipt, safe retries report deferred/overdue, then legitimate fence
/// settlement completes exactly once. Fences are never bypassed for the SLA.
#[tokio::test]
async fn guard_refusal_stays_pending_then_legitimate_resolution_completes_once() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    seed_footprint(pool, GUILD_A, MEMBER, "intake-guard-own").await;
    seed_footprint(pool, GUILD_A, OTHER_MEMBER, "intake-guard-other").await;
    sqlx::query(
        "INSERT INTO internal_idempotency
           (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
         VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2, $2)",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .expect("in-flight fence");

    let ack = request_member_deletion(pool, GUILD_A, MEMBER, "intake-fixture-guard")
        .await
        .expect("guarded request still queues as pending");
    let first = run_retention(pool, RetentionMode::Execute)
        .await
        .expect("first drain");
    assert_eq!(
        first.deletions_completed, 0,
        "a guarded request must not complete, got {first:?}"
    );
    assert_eq!(
        first.deletions_deferred, 1,
        "the refusal must report deferred, got {first:?}"
    );
    assert!(
        deletion_receipt_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
            .await
            .is_err(),
        "no false receipt while guarded"
    );
    // Safe retry while the fence holds: still pending, still no receipt.
    let retry = run_retention(pool, RetentionMode::Execute)
        .await
        .expect("safe retry");
    assert_eq!(retry.deletions_completed, 0);
    assert_eq!(retry.deletions_deferred, 1);
    // Escalation signal: backdate past the SLA and observe overdue.
    sqlx::query(
        "UPDATE retention_deletion_requests SET requested_at = now() - make_interval(hours => 73) WHERE request_id = $1",
    )
    .bind(&ack.request_id)
    .execute(pool)
    .await
    .expect("backdate for overdue");
    let overdue = run_retention(pool, RetentionMode::DryRun)
        .await
        .expect("dry overdue signal");
    assert_eq!(
        overdue.deletions_overdue, 1,
        "the breach must stay explicit, got {overdue:?}"
    );
    assert_eq!(overdue.deletions_completed, 0);
    // Legitimate fence settlement only: mark the replay fence settled.
    sqlx::query(
        "UPDATE internal_idempotency SET state = 'completed', response_code = 'success',
         http_status = 200, resource_id = actor_id, affected = 1 WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .execute(pool)
    .await
    .expect("settle fence");
    // Restore a fresh request clock so the legitimate completion still proves
    // under-72-hour service; the earlier overdue report remains the explicit
    // breach record for the guarded interval.
    sqlx::query(
        "UPDATE retention_deletion_requests SET requested_at = clock_timestamp() WHERE request_id = $1",
    )
    .bind(&ack.request_id)
    .execute(pool)
    .await
    .expect("fresh clock after settlement");
    let done = run_retention(pool, RetentionMode::Execute)
        .await
        .expect("settled drain");
    assert_eq!(
        done.deletions_completed, 1,
        "settlement must complete exactly once, got {done:?}"
    );
    let receipt = deletion_receipt_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
        .await
        .expect("receipt only after legitimate settlement");
    assert_eq!(receipt.request_id, ack.request_id);
    let again = run_retention(pool, RetentionMode::Execute)
        .await
        .expect("second drain");
    assert_eq!(
        again.deletions_completed, 0,
        "completion happens once, got {again:?}"
    );
    db.close().await.expect("verify teardown");
}

/// Opt-out, guild-purge, and replay fences never silently remove an outstanding
/// request or counterfeit its completion. Control-plane exceptions survive.
#[tokio::test]
async fn opt_out_guild_purge_and_fences_do_not_remove_or_counterfeit_request() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    for (guild, member, key) in [
        (GUILD_A, MEMBER, "intake-fence-own"),
        (GUILD_A, OTHER_MEMBER, "intake-fence-other"),
    ] {
        seed_footprint(pool, guild, member, key).await;
    }
    // Operator-released replay fence for the same guild: must survive.
    sqlx::query(
        "INSERT INTO moderation_idempotency
           (guild_id, idempotency_key, action, request_hash, state, outcome, claimed_at, result_json)
         VALUES ($1, 'intake-fence-released', 'warn', 'operator_released', 'done', 'operator_released', '2026-10-01T00:00:00Z', '{}')",
    )
    .bind(GUILD_A)
    .execute(pool)
    .await
    .expect("operator fence");
    sqlx::query(
        "INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
         VALUES ('intake-fence-chan', $1, 'intake-fence-released', 'tok')",
    )
    .bind(GUILD_A)
    .execute(pool)
    .await
    .expect("fence child");

    let ack = request_member_deletion(pool, GUILD_A, MEMBER, "intake-fixture-fence")
        .await
        .expect("request queues");
    // Narrower opt-out purges event-store rows for the pair but must not touch
    // the outstanding deletion request or fabricate its receipt.
    let db_url = std::env::var("TWO_TEST_DATABASE_URL").expect("bootstrap required");
    let fixture_url = match db_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", db.name()),
        None => db_url,
    };
    let cutover = two_bot_cutover::connect_with_tls(
        &fixture_url,
        5,
        true,
        two_bot_core::database_tls::TlsPolicy::LocalOnly,
    )
    .await
    .expect("connect for opt-out");
    opt_out_member(&cutover, GUILD_A, MEMBER, "operator:intake-fence")
        .await
        .expect("opt-out commits");
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(&ack.request_id)
    .fetch_one(pool)
    .await
    .expect("pending after opt-out");
    assert_eq!(
        pending, 1,
        "opt-out must not silently remove the outstanding request"
    );
    assert!(
        deletion_receipt_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
            .await
            .is_err(),
        "opt-out must not counterfeit completion"
    );
    // Departed-guild purge leaves the ledger: the pending receipt still completes.
    assert!(handle_guild_delete(pool, GUILD_A, None)
        .await
        .expect("purge enqueues"));
    let report = run_retention(pool, RetentionMode::Execute)
        .await
        .expect("drain with purge queued");
    assert_eq!(
        report.deletions_completed, 1,
        "the pending receipt must complete against pre-purge rows, got {report:?}"
    );
    let receipt = deletion_receipt_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
        .await
        .expect("receipt survives the guild purge");
    assert_eq!(receipt.request_id, ack.request_id);
    let fence: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = 'intake-fence-released'",
    )
    .bind(GUILD_A)
    .fetch_one(pool)
    .await
    .expect("fence survives");
    assert_eq!(
        fence, 1,
        "the operator-released replay fence must survive deletion and purge"
    );
    let suppression: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .fetch_one(pool)
    .await
    .expect("suppression survives");
    assert_eq!(
        suppression, 1,
        "the opt-out suppression tombstone stays retained"
    );
    cutover.close().await;
    db.close().await.expect("verify teardown");
}

/// Dry run reports but never queues a destructive effect or fabricates completion.
#[tokio::test]
async fn dry_run_never_queues_or_fabricates_completion() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    seed_footprint(pool, GUILD_A, MEMBER, "intake-dry-own").await;

    let ack = request_member_deletion(pool, GUILD_A, MEMBER, "intake-fixture-dry")
        .await
        .expect("request queues");
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_deletion_requests")
        .fetch_one(pool)
        .await
        .expect("queue count");
    let dry = run_retention(pool, RetentionMode::DryRun)
        .await
        .expect("dry run");
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM retention_deletion_requests")
        .fetch_one(pool)
        .await
        .expect("queue count");
    assert_eq!(before, after, "dry run must not queue or drop requests");
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests WHERE request_id = $1 AND status = 'pending'",
    )
    .bind(&ack.request_id)
    .fetch_one(pool)
    .await
    .expect("still pending");
    assert_eq!(pending, 1, "dry run must leave the request pending");
    assert!(
        deletion_receipt_for_requester(pool, GUILD_A, MEMBER, &ack.request_id)
            .await
            .is_err(),
        "dry run must not fabricate completion"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM members WHERE guild_id = '711111111111111111'"
        )
        .await,
        1,
        "dry run must delete nothing"
    );
    assert_eq!(
        dry.guild_purges_failed, 0,
        "dry run attempts no purges, got {dry:?}"
    );
    db.close().await.expect("verify teardown");
}
