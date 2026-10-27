#![cfg(feature = "db")]
//! Protected store controls for the XP visibility slice; no production credentials.
//! These controls must remain GREEN while the runtime visibility tests turn GREEN.

use two_bot_core::leveling_store as store;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const A: &str = "100000000000000001";
const B: &str = "100000000000000002";

#[tokio::test]
async fn source_fields_are_independent_and_lifetime_reads_stay_bounded_and_guild_scoped() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create disposable fixture");
    for member in [B, A] {
        sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,message_xp,voice_xp,imported_xp,updated_at) VALUES ($1,$2,1234,137,59,1038,NOW())")
            .bind(GUILD).bind(member).execute(db.pool()).await.unwrap();
    }
    for offset in 3..=30 {
        sqlx::query(
            "INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,100,100,NOW())",
        )
        .bind(GUILD)
        .bind(format!("1000000000000000{offset:02}"))
        .execute(db.pool())
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ('other-guild',$1,99999,99999,NOW())")
        .bind(A).execute(db.pool()).await.unwrap();
    let profile = store::profile(db.pool(), GUILD, A).await.unwrap();
    let absent = store::profile(db.pool(), GUILD, "100000000000000099")
        .await
        .unwrap();
    let top = store::leaderboard(db.pool(), GUILD, 10).await.unwrap();
    let minimum = store::leaderboard(db.pool(), GUILD, 0).await.unwrap();
    let maximum = store::leaderboard(db.pool(), GUILD, u64::MAX)
        .await
        .unwrap();
    let empty = store::seasonal_leaderboard(db.pool(), GUILD, "2027-Q1", 10)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM member_levels WHERE guild_id=$1")
        .bind(GUILD)
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.close().await.expect("drop disposable fixture");
    assert_eq!(
        (
            profile.xp,
            profile.message_xp,
            profile.voice_xp,
            profile.imported_xp
        ),
        (1234, 137, 59, 1038)
    );
    assert_eq!((profile.rank, profile.member_count), (Some(1), 30));
    assert_eq!((absent.xp, absent.rank, absent.member_count), (0, None, 30));
    assert_eq!(top.len(), 10);
    assert_eq!(
        (top[0].member_id.as_str(), top[0].xp, top[0].rank),
        (A, 1234, 1)
    );
    assert_eq!(
        (top[1].member_id.as_str(), top[1].xp, top[1].rank),
        (B, 1234, 2)
    );
    assert_eq!(minimum.len(), 1);
    assert_eq!(
        maximum.len(),
        25,
        "existing lifetime query cap is not bypassed"
    );
    assert!(
        empty.is_empty(),
        "seasonal empty remains empty despite lifetime rows"
    );
    assert_eq!(count, 30, "absent profile creates no XP row");
}
