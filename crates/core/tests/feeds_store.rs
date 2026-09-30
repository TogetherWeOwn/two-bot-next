#![cfg(feature = "db")]

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_core::feeds::*;
use two_bot_core::feeds_store::*;

fn test_options() -> PgConnectOptions {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
    let options: PgConnectOptions = url.parse().expect("valid test database URL");
    assert_eq!(
        options.get_host(),
        "agent-testdb",
        "test-container-only host"
    );
    assert_eq!(
        options.get_username(),
        "agent_test",
        "test-container-only user"
    );
    assert_eq!(
        options.get_database(),
        Some("agent_test"),
        "test-container-only database"
    );
    assert_eq!(options.get_port(), 5432, "test-container-only port");
    options
}

async fn connect(schema: &str) -> Pool<Postgres> {
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(test_options().options([("search_path", schema)]))
        .await
        .expect("connect to test container")
}

async fn setup() -> (Pool<Postgres>, String) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let schema = format!(
        "feeds_{}_{}_{}",
        std::process::id(),
        nonce,
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    assert!(safe_schema(&schema));
    let pool = connect("public").await;
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let pool = connect(&schema).await;
    // Prove coexistence with the earlier shared audit-table migration.
    sqlx::raw_sql(include_str!("../../cutover/migrations/0160_rsvp.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../cutover/migrations/0180_feeds.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../cutover/migrations/0180_feeds.sql"))
        .execute(&pool)
        .await
        .unwrap();
    (pool, schema)
}

fn safe_schema(schema: &str) -> bool {
    schema.len() <= 63
        && schema.starts_with("feeds_")
        && schema
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

async fn cleanup(pool: Pool<Postgres>, schema: &str) {
    assert!(safe_schema(schema));
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

fn relay() -> FeedRelay {
    FeedRelay {
        id: "feed-1".into(),
        guild_id: "guild-1".into(),
        channel_id: "channel-1".into(),
        kind: FeedKind::Rss,
        source: "https://example.org/feed.xml".into(),
        enabled: true,
        last_checked_at: None,
        created_by: "actor".into(),
        created_at: 1_790_731_234_567,
        updated_at: 1_790_731_234_567,
    }
}

fn post(feed: &FeedRelay, key: &str) -> FeedPost {
    plan_post(
        feed,
        &FeedItem {
            key: key.into(),
            title: key.into(),
            url: "https://example.org/post".into(),
            published_at: None,
        },
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; CI runs explicitly"]
async fn feed_crud_audit_scope_and_timestamp_roundtrip() {
    let (pool, schema) = setup().await;
    let feed = relay();
    add_feed(&pool, &feed).await.unwrap();
    assert_eq!(
        list_feeds(&pool, "guild-1", false).await.unwrap(),
        vec![feed.clone()]
    );
    assert!(list_feeds(&pool, "other", false).await.unwrap().is_empty());
    let mut hijack = feed.clone();
    hijack.guild_id = "other".into();
    assert!(add_feed(&pool, &hijack).await.is_err());
    assert!(!remove_feed(&pool, "other", &feed.id).await.unwrap());
    let now = feed.created_at + 1234;
    mark_checked(&pool, "guild-1", &feed.id, now).await.unwrap();
    assert_eq!(
        list_feeds(&pool, "guild-1", true).await.unwrap()[0].last_checked_at,
        Some(now)
    );
    write_audit(
        &pool,
        &FeedAudit {
            id: "audit-1",
            guild_id: "guild-1",
            actor_id: Some("actor"),
            action: "feed.create",
            target_key: &feed.id,
            outcome: "rss",
            reason: None,
            now_ms: now,
        },
    )
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM announcements_audit_log WHERE action = 'feed.create'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let p = post(&feed, "x");
    assert!(claim_delivery(&pool, "guild-1", &p, "claim-1", now)
        .await
        .unwrap()
        .is_some());
    assert!(remove_feed(&pool, "guild-1", &feed.id).await.unwrap());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM feed_deliveries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "cascade deleted this relay's ledger");
    cleanup(pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; CI runs explicitly"]
async fn concurrent_claims_lease_takeover_and_stale_owner_fencing() {
    let (pool, schema) = setup().await;
    let feed = relay();
    add_feed(&pool, &feed).await.unwrap();
    let p = post(&feed, "x");
    assert_eq!(
        claim_delivery(&pool, "other", &p, "foreign", 1000)
            .await
            .unwrap(),
        None
    );
    let mut tasks = Vec::new();
    for i in 0..16 {
        let pool = pool.clone();
        let p = p.clone();
        tasks.push(tokio::spawn(async move {
            let token = format!("owner-{i}");
            (
                token.clone(),
                claim_delivery(&pool, "guild-1", &p, &token, 1000)
                    .await
                    .unwrap(),
            )
        }));
    }
    let mut winners = Vec::new();
    for task in tasks {
        let (token, claim) = task.await.unwrap();
        if claim.is_some() {
            winners.push(token);
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(
        claim_delivery(&pool, "guild-1", &p, "early", 60_999)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        claim_delivery(&pool, "guild-1", &p, "new-owner", 61_000)
            .await
            .unwrap(),
        Some(DeliveryClaim::Recovered)
    );
    assert!(
        !mark_delivered(&pool, "guild-1", &p, &winners[0], "old-msg", 61_001)
            .await
            .unwrap()
    );
    assert!(!release_unposted(&pool, "guild-1", &p, &winners[0])
        .await
        .unwrap());
    assert!(
        !mark_delivered(&pool, "other", &p, "new-owner", "msg", 61_002)
            .await
            .unwrap()
    );
    assert!(
        mark_delivered(&pool, "guild-1", &p, "new-owner", "msg", 61_002)
            .await
            .unwrap()
    );
    assert_eq!(
        claim_delivery(&pool, "guild-1", &p, "retry", 1_000_000_000)
            .await
            .unwrap(),
        None
    );
    cleanup(pool, &schema).await;
}

#[derive(Default)]
struct MockDiscord {
    sent: Vec<(String, String)>,
}
impl MockDiscord {
    fn send(&mut self, p: &FeedPost) -> String {
        assert!(p.suppress_mentions && p.enforce_nonce);
        let id = format!("message-{}", self.sent.len());
        self.sent.push((p.nonce.clone(), id.clone()));
        id
    }
    fn find(&self, nonce: &str) -> Option<String> {
        self.sent
            .iter()
            .find(|(n, _)| n == nonce)
            .map(|(_, id)| id.clone())
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; CI runs explicitly"]
async fn mock_delivery_crash_reconciliation_restart_and_overflow() {
    let (pool, schema) = setup().await;
    let feed = relay();
    add_feed(&pool, &feed).await.unwrap();
    let p = post(&feed, "crash-item");
    let mut discord = MockDiscord::default();
    let claim = claim_delivery(&pool, "guild-1", &p, "before-crash", 1000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery_action(claim, 0), DeliveryAction::Send);
    discord.send(&p); // crash after accepting message, before mark_delivered
    pool.close().await;
    let pool = connect(&schema).await;
    let claim = claim_delivery(&pool, "guild-1", &p, "after-restart", 120_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery_action(claim, 0), DeliveryAction::ReconcileByNonce);
    let message = discord.find(&p.nonce).unwrap();
    assert!(
        mark_delivered(&pool, "guild-1", &p, "after-restart", &message, 120_001)
            .await
            .unwrap()
    );
    assert_eq!(
        discord.sent.len(),
        1,
        "recovered the existing message, never reposted"
    );
    assert_eq!(
        claim_delivery(&pool, "guild-1", &p, "later", 1_000_000)
            .await
            .unwrap(),
        None
    );
    let unresolved = post(&feed, "crash-before-send");
    claim_delivery(&pool, "guild-1", &unresolved, "before-send", 1000)
        .await
        .unwrap();
    let recovered = claim_delivery(&pool, "guild-1", &unresolved, "recovery", 120_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        delivery_action(recovered, 0),
        DeliveryAction::ReconcileByNonce
    );
    assert_eq!(discord.find(&unresolved.nonce), None);
    // Missing in bounded history is NOT authoritative absence. Leave pending,
    // never turn this into a blind resend outside Discord's nonce window.
    assert_eq!(discord.sent.len(), 1);
    let candidates: Vec<_> = (0..25).map(|i| post(&feed, &format!("item-{i}"))).collect();
    for pass in 0..2 {
        let mut posted = 0;
        for p in &candidates {
            let token = format!("pass-{pass}-{}", p.item_key);
            let Some(claim) = claim_delivery(&pool, "guild-1", p, &token, 200_000 + pass)
                .await
                .unwrap()
            else {
                continue;
            };
            match delivery_action(claim, posted) {
                DeliveryAction::Send => {
                    let message = discord.send(p);
                    assert!(
                        mark_delivered(&pool, "guild-1", p, &token, &message, 200_000 + pass)
                            .await
                            .unwrap()
                    );
                    posted += 1;
                }
                DeliveryAction::Defer => {
                    assert!(release_unposted(&pool, "guild-1", p, &token).await.unwrap());
                }
                DeliveryAction::ReconcileByNonce => panic!("fresh or delivered candidates only"),
            }
        }
        assert_eq!(posted, if pass == 0 { 20 } else { 5 });
    }
    assert_eq!(discord.sent.len(), 26);
    pool.close().await;
    let pool = connect(&schema).await;
    for p in &candidates {
        assert_eq!(
            claim_delivery(&pool, "guild-1", p, "third-restart", 2_000_000)
                .await
                .unwrap(),
            None
        );
    }
    cleanup(pool, &schema).await;
}
