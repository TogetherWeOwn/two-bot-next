//! Acceptance: the unban sweep activates staged expiries through a bounded
//! fetch and still completes every due unban across passes.
//!
//! Protected test-first handoff: the paired Builder bounds the staged-activation
//! fetch (same clamp family as the candidates query: `limit.clamp(0, 25)`)
//! without editing this file. Contract pinned here:
//!
//! - One `claim_due_unbans` call over 30 equally due staged expiries claims the
//!   oldest 25 generations and leaves at least 5 staged; repeated passes claim all 30.
//! - A limit-one pass prioritizes the earliest due staged expiry even when older
//!   generations expire later; future expiries cannot consume a due claim.
//! - Claim tokens stay 32-hex and unique; intent generations, `execute_at`
//!   schedule positions and quarantined imports are unchanged by the sweep;
//!   fresh claims take no retry position (`retry_generation` stays NULL).
//!
//! Database tests use the authorized disposable test database only
//! (`TWO_TEST_DATABASE_URL`, `agent-testdb` principal): they skip with a
//! diagnostic when the URL is absent locally and fail closed in CI.
#![cfg(feature = "db")]

use sqlx::PgPool;
use two_bot_core::member_moderation::{MemberModerationStore, UncertainKind};
use two_bot_core::member_moderation_store::PgMemberModerationStore;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "100000000000000001";
const NOW: &str = "2023-11-14T22:13:20.000Z";
const DUE: &str = "2023-11-14T23:13:20.000Z";
const STAGED: usize = 30;
const BOUND: i64 = 25;

fn bootstrap_url(test: &str) -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure sweep-bounds acceptance"
            );
            eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

async fn fixture(bootstrap: &str) -> TestDatabase {
    TestDatabase::create(bootstrap, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

fn member_user(i: usize) -> String {
    format!("1444444444444{i:05}")
}

async fn seed_accepted(store: &PgMemberModerationStore, request: &str, user: &str) {
    seed_accepted_at(store, request, user, NOW).await;
}

async fn seed_accepted_at(
    store: &PgMemberModerationStore,
    request: &str,
    user: &str,
    execute_at: &str,
) {
    let attempt = store
        .stage_unban(GUILD, user, execute_at, "expiry", request, NOW)
        .await
        .expect("expiry stages");
    store
        .confirm_ban_attempt(GUILD, user, request, attempt, NOW)
        .await
        .expect("ban accepted");
}

async fn staged_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM moderation_scheduled_unbans WHERE state = 'staged'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn intent_generations(pool: &PgPool) -> Vec<(String, i64)> {
    sqlx::query_as("SELECT request_id, generation FROM moderation_member_bans ORDER BY request_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn unban_schedule(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT request_id, execute_at::text FROM moderation_scheduled_unbans ORDER BY request_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn sweep_single_pass_leaves_staged_rows_beyond_the_bound() {
    let Some(bootstrap) = bootstrap_url("bounded staged activation") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgMemberModerationStore::new(db.pool().clone(), GUILD);
    // Seed newest lexical identities first so request-id sorting cannot stand
    // in for the oldest-generation tie-break at the activation boundary.
    let requests: Vec<String> = (0..STAGED)
        .rev()
        .map(|i| format!("bound-due-{i:02}"))
        .collect();
    for (i, request) in requests.iter().enumerate() {
        seed_accepted(&store, request, &member_user(i)).await;
    }
    assert_eq!(staged_count(db.pool()).await, STAGED as i64);
    let jobs = store
        .claim_due_unbans(GUILD, DUE, BOUND)
        .await
        .expect("due claim");
    assert!(!jobs.is_empty(), "a bounded pass still makes progress");
    assert!(
        jobs.len() as i64 <= BOUND,
        "one pass claims at most the bound"
    );
    let remaining = staged_count(db.pool()).await;
    assert!(
        remaining >= STAGED as i64 - BOUND,
        "one pass activates at most {BOUND} staged rows, {remaining} of {STAGED} still staged"
    );
    // Pin membership, not the candidate query's return order: exactly the
    // first 25 seeded generations must cross the staged activation boundary.
    let mut claimed: Vec<&str> = jobs.iter().map(|job| job.request_id.as_str()).collect();
    claimed.sort_unstable();
    let mut oldest: Vec<&str> = requests[..BOUND as usize]
        .iter()
        .map(String::as_str)
        .collect();
    oldest.sort_unstable();
    assert_eq!(
        claimed, oldest,
        "equal-time activation chooses oldest intents"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn sweep_limit_one_claims_earliest_due_before_older_future_staged_expiries() {
    let Some(bootstrap) = bootstrap_url("staged due-time priority") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgMemberModerationStore::new(db.pool().clone(), GUILD);
    const FUTURE: &str = "2023-11-15T00:13:20.000Z";
    // Both generation and request-id order oppose due-time priority.
    for (i, (request, execute_at)) in [
        ("mixed-a-future", FUTURE),
        ("mixed-b-later-due", "2023-11-14T23:00:00.000Z"),
        ("mixed-z-earliest-due", "2023-11-14T22:30:00.000Z"),
    ]
    .into_iter()
    .enumerate()
    {
        seed_accepted_at(&store, request, &member_user(i), execute_at).await;
    }
    assert_eq!(
        staged_count(db.pool()).await,
        3,
        "all fixtures start staged"
    );
    let generations = intent_generations(db.pool()).await;
    let schedule = unban_schedule(db.pool()).await;

    for expected in ["mixed-z-earliest-due", "mixed-b-later-due"] {
        let jobs = store
            .claim_due_unbans(GUILD, DUE, 1)
            .await
            .expect("limit-one due claim");
        assert_eq!(
            jobs.iter()
                .map(|job| job.request_id.as_str())
                .collect::<Vec<_>>(),
            [expected],
            "each bounded call must immediately claim the earliest remaining due expiry"
        );
        store
            .complete_unban(&jobs[0].request_id, &jobs[0].claim_token)
            .await
            .expect("complete due claim");
    }
    assert!(
        store
            .claim_due_unbans(GUILD, DUE, 1)
            .await
            .expect("future-only pass")
            .is_empty(),
        "a future expiry is never claimed before its execute_at"
    );
    let future_unclaimed: bool = sqlx::query_scalar(
        "SELECT claimed_at IS NULL AND claim_token IS NULL
         FROM moderation_scheduled_unbans WHERE request_id = 'mixed-a-future'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(future_unclaimed, "future expiry retains no claim");
    assert_eq!(intent_generations(db.pool()).await, generations);
    assert_eq!(unban_schedule(db.pool()).await, schedule);

    let future = store
        .claim_due_unbans(GUILD, FUTURE, 1)
        .await
        .expect("future expiry becomes due");
    assert_eq!(
        future
            .iter()
            .map(|job| job.request_id.as_str())
            .collect::<Vec<_>>(),
        ["mixed-a-future"],
        "deferred expiry is preserved and claimable when due"
    );
    store
        .complete_unban(&future[0].request_id, &future[0].claim_token)
        .await
        .expect("complete deferred expiry");
    db.close().await.unwrap();
}

#[tokio::test]
async fn sweep_completes_across_passes_without_changing_tokens_generations_quarantine_or_retry() {
    let Some(bootstrap) = bootstrap_url("sweep completion invariants") else {
        return;
    };
    let db = fixture(&bootstrap).await;
    let store = PgMemberModerationStore::new(db.pool().clone(), GUILD);
    for i in 0..STAGED {
        seed_accepted(&store, &format!("bound-full-{i:02}"), &member_user(i)).await;
    }
    // An imported schedule with no trustworthy intent: the sweep must leave it
    // quarantined, exactly as the import path left it.
    sqlx::query(
        "INSERT INTO moderation_scheduled_unbans
         (request_id, guild_id, user_id, execute_at, reason, state, created_at, claimed_at, claim_token)
         VALUES ('bound-quarantined-import', $1, $2, $3::text::timestamptz, 'imported reason',
                 'quarantined', $3::text::timestamptz, $3::text::timestamptz, 'imported-token')",
    )
    .bind(GUILD)
    .bind(member_user(99))
    .bind(NOW)
    .execute(db.pool())
    .await
    .unwrap();
    let generations = intent_generations(db.pool()).await;
    let schedule = unban_schedule(db.pool()).await;

    let mut tokens = std::collections::HashSet::new();
    let mut total = 0;
    for _ in 0..40 {
        let jobs = store
            .claim_due_unbans(GUILD, DUE, BOUND)
            .await
            .expect("due claim");
        if jobs.is_empty() {
            break;
        }
        for job in &jobs {
            assert_eq!(job.claim_token.len(), 32, "claim token stays 32-hex");
            assert!(
                job.claim_token.bytes().all(|b| b.is_ascii_hexdigit()),
                "claim token stays hex"
            );
            assert!(
                tokens.insert(job.claim_token.clone()),
                "claim tokens stay unique"
            );
        }
        total += jobs.len();
    }
    assert_eq!(total, STAGED, "every due unban activates across passes");
    assert_eq!(
        intent_generations(db.pool()).await,
        generations,
        "generations unchanged"
    );
    assert_eq!(
        unban_schedule(db.pool()).await,
        schedule,
        "execute_at positions unchanged"
    );
    let retried: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderation_scheduled_unbans WHERE retry_generation IS NOT NULL",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(retried, 0, "fresh claims take no retry position");
    let fenced: (String, String) = sqlx::query_as(
        "SELECT state, claim_token FROM moderation_scheduled_unbans
         WHERE request_id = 'bound-quarantined-import'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(fenced.0, "quarantined");
    assert_eq!(fenced.1, "imported-token");
    let surfaced = store.surface_uncertain(GUILD).await.expect("fenced rows");
    assert!(
        surfaced
            .iter()
            .any(|row| row.request_id == "bound-quarantined-import"
                && row.kind == UncertainKind::QuarantinedUnban),
        "quarantined import stays fenced for the operator"
    );
    db.close().await.unwrap();
}
