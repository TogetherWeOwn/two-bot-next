//! Per-pair pending-cap regression for the authenticated deletion intake.
//!
//! Builder-owned complement to the Test Author acceptance suite
//! (`deletion_intake_receipts.rs`, which asserts no rate-limit behavior and
//! must not be edited here): concurrent `request_member_deletion` calls with
//! distinct interaction ids serialize under the per-pair advisory lock, so at
//! most `MAX_PENDING_PER_PAIR` (5) are accepted and the rest are rate limited.

use two_bot_cutover::deletion_intake::request_member_deletion;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "733333333333333333";
const MEMBER: &str = "766666666666666666";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the deletion intake cap suite"
            );
            eprintln!("SKIP deletion intake cap: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .expect("disposable migrated database"),
    )
}

/// Ten concurrent requests for one pair must yield exactly five pending rows:
///
/// the per-pair advisory lock serializes the count check with the insert, so
/// the sixth and later callers observe the cap and fail rate limited instead
/// of overshooting it.
#[tokio::test]
async fn pending_cap_holds_under_concurrent_requests() {
    let Some(db) = database().await else { return };
    const CALLS: usize = 10;
    const CAP: usize = 5;

    let mut handles = Vec::with_capacity(CALLS);
    for index in 0..CALLS {
        let pool = db.pool().clone();
        let interaction = format!("cap-race-{index}");
        handles.push(tokio::spawn(async move {
            request_member_deletion(&pool, GUILD, MEMBER, &interaction).await
        }));
    }

    let mut accepted = 0_usize;
    let mut limited = 0_usize;
    for handle in handles {
        match handle.await.expect("request task joins") {
            Ok(ack) => {
                assert_eq!(ack.status, "pending", "acknowledgement stays pending");
                assert!(!ack.request_id.is_empty(), "the request must have an id");
                accepted += 1;
            }
            Err(sqlx::Error::InvalidArgument(message)) if message.contains("rate limited") => {
                limited += 1;
            }
            Err(other) => panic!("unexpected request error: {other:?}"),
        }
    }

    assert_eq!(accepted + limited, CALLS, "every request resolves");
    assert_eq!(
        accepted, CAP,
        "exactly the per-pair cap is accepted under concurrency"
    );
    assert_eq!(limited, CALLS - CAP, "the rest are rate limited");

    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM retention_deletion_requests \
         WHERE guild_id = $1 AND user_id = $2 AND status = 'pending'",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_one(db.pool())
    .await
    .expect("pending count");
    assert_eq!(pending, CAP as i64, "the ledger holds exactly the cap");

    db.close().await.expect("verify teardown");
}
