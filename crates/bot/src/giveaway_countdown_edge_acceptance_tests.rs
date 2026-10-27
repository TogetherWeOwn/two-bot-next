//! Forward-seam acceptance for GiveawayRuntime::refresh, supplied by the
//! runtime builder. Apply/register this file only after that module exists.
//! Mutations caught: editing every tick, skipping any of 3600/600/60/30-second
//! boundaries, editing another guild, and changing admission/draw state from
//! the countdown-only refresh. SQL advances the real stored deadline; there
//! is no clock mock or product test hook.
#![cfg(test)]

use super::giveaway_admission_edge_acceptance_tests::{create_persisted, message_patches};
use super::*;
use crate::giveaway_runtime::GiveawayRuntime;

async fn deadline(pool: &PgPool, opaque: &str, remaining_seconds: i64) {
    let changed = sqlx::query(
        "UPDATE giveaways SET ends_at = now() + $3 * interval '1 second' WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD_S)
    .bind(opaque)
    .bind(remaining_seconds as f64)
    .execute(pool)
    .await
    .expect("controlled stored deadline");
    assert_eq!(changed.rows_affected(), 1);
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn countdown_refresh_edits_once_per_threshold_not_once_per_tick() {
    let db = TestDb::new().await;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let commands = CommandRuntime::new(
        db.pool.clone(),
        executor_at(origin.clone()),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    let opaque = create_persisted(&db.pool, &commands, 86_000, 0).await;
    let countdown = GiveawayRuntime::new(db.pool.clone(), executor_at(origin));
    deadline(&db.pool, &opaque, 7_200).await;
    // A freshly constructed runtime may prime its first view. The bounded
    // oracle begins once that first view is established, not at boot.
    countdown.refresh(GUILD_S).await.expect("prime countdown");
    let primed = message_patches(&mock).len();
    for _ in 0..3 {
        countdown
            .refresh(GUILD_S)
            .await
            .expect("same countdown bucket");
    }
    assert_eq!(
        message_patches(&mock).len(),
        primed,
        "unchanged ticks make no Discord edit"
    );
    deadline(&db.pool, &opaque, 3_500).await;
    countdown
        .refresh("999999")
        .await
        .expect("unrelated guild while a boundary is due");
    assert_eq!(
        message_patches(&mock).len(),
        primed,
        "another guild cannot consume this guild's pending boundary"
    );

    // Descending, wide-margin samples cross exactly the documented boundaries.
    // Each same-bucket sample then rejects an over-eager refresh implementation.
    for (seconds, edits) in [
        (3_500, 1),
        (3_000, 0),
        (500, 1),
        (400, 0),
        (50, 1),
        (40, 0),
        (20, 1),
        (15, 0),
    ] {
        deadline(&db.pool, &opaque, seconds).await;
        let before = message_patches(&mock).len();
        countdown.refresh(GUILD_S).await.expect("countdown refresh");
        assert_eq!(
            message_patches(&mock).len() - before,
            edits,
            "remaining {seconds}s crosses a threshold iff one edit is due"
        );
        for _ in 0..3 {
            countdown
                .refresh(GUILD_S)
                .await
                .expect("repeated countdown tick");
        }
        assert_eq!(
            message_patches(&mock).len() - before,
            edits,
            "remaining {seconds}s refresh is idempotent between boundaries"
        );
    }
    assert_eq!(
        message_patches(&mock).len() - primed,
        4,
        "the whole countdown costs only four boundary edits"
    );
    let patches = message_patches(&mock);
    let body = |request: &RestRequest| -> serde_json::Value {
        serde_json::from_slice(&request.body).expect("countdown message JSON")
    };
    let boundary_edits = &patches[primed..];
    for pair in boundary_edits.windows(2) {
        assert_ne!(
            body(&pair[0]),
            body(&pair[1]),
            "each threshold visibly changes the countdown"
        );
    }
    assert!(
        patches.iter().all(|request| request
            .path
            .contains(&format!("/channels/{CHANNEL_S}/messages/"))
            || request.path.contains("/webhooks/")),
        "only the real giveaway message is edited"
    );

    let before = patches.len();
    countdown
        .refresh("999999")
        .await
        .expect("unrelated guild refresh");
    assert_eq!(
        message_patches(&mock).len(),
        before,
        "refresh is guild-scoped"
    );
    let state: (String, i64, i64) = sqlx::query_as(
        "SELECT status, (SELECT count(*) FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2), \
         (SELECT count(*) FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2) \
         FROM giveaways WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD_S).bind(&opaque).fetch_one(&db.pool).await.expect("countdown leaves domain state alone");
    assert_eq!(
        state,
        ("open".to_owned(), 0, 0),
        "countdown refresh neither enters nor draws a giveaway"
    );
    mock.shutdown().await;
    db.close().await;
}
