#![cfg(feature = "db")]

//! Store proof for giveaways slice 1, on the disposable test database only.
//! Create an empty two_bot_test_local bootstrap as described in
//! CONTRIBUTING.md, then run:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test giveaway_store
//! No DATABASE_URL or inherited application credentials are consulted; an
//! absent TWO_TEST_DATABASE_URL skips vacuously on a workstation while CI
//! must configure it (the helper asserts so).
//!
//! Covers what the protected `giveaway_domain_store_acceptance` corpus
//! deliberately leaves to the builder: CRUD round-trips, entry-upsert
//! idempotency through the store API, the claim-lease lifecycle (contention
//! yields exactly one winner, expired leases reclaim, only the holder
//! completes or releases), atomic draw completion (a failed guard rolls the
//! insert back), the drawn-once announcement guard, raced draw inserts
//! leaving one row, and composite foreign keys binding entries and draws
//! to a live giveaway (missing parents fail, live children refuse a
//! parent delete, the child-first store delete still succeeds) on BOTH
//! chains: the cutover fixtures chain and the store chain the production
//! boot runner applies, so neither copy can silently lose the constraint.

use sqlx::types::time::OffsetDateTime;
use two_bot_core::giveaway::MemberStanding;
use two_bot_core::giveaway_store as store;
use two_bot_core::giveaway_store::{EntryAdmission, NewDraw, NewGiveaway};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const CHANNEL: &str = "200000000000000001";
const MESSAGE: &str = "200000000000000002";
const MOD: &str = "100000000000000099";
const BUDGET_USER: &str = "100000000000000021";
const BUDGET_OTHER: &str = "100000000000000022";
const NONCE: &str = "9f2c4a7e1b5d8f03a6c9e2b4d7f1a83c";
const SEED_0: &str = "fdab72cb4823dbf292f0ed3fea3abef20dc268169a90f6aea0a1cd4c1a8c15ab";
const SEED_1: &str = "16688231019cd5f768dcb0e2a604881b365c0ba7b242433b3e7ff96bde0bdf88";
const ELIGIBLE_HASH: &str = "deebe3bffe4ba1543bd568b42a31508a96ef5c0ba98464a3f1e572286ead0299";

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_760_000_000 + seconds).expect("fixed test stamp")
}

async fn connect(test: &str) -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated test database"),
        ),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure giveaway_store ({test})"
            );
            eprintln!("SKIP giveaway_store ({test}): TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

/// Same as [`connect`], but migrates the store chain
/// (`crates/store/migrations`, the copy the production boot runner applies)
/// instead of the cutover fixtures chain, so the giveaway constraints are
/// pinned where they ship, not just where the fixtures live.
async fn connect_store(test: &str) -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../store/migrations"))
                .await
                .expect("create store-chain test database"),
        ),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure giveaway_store ({test})"
            );
            eprintln!("SKIP giveaway_store ({test}): TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

fn assert_sqlstate(error: &sqlx::Error, code: &str) {
    assert_eq!(
        error
            .as_database_error()
            .expect("database constraint error")
            .code()
            .as_deref(),
        Some(code),
        "{error}"
    );
}

fn new_giveaway(id: &str, ends_at: OffsetDateTime) -> NewGiveaway {
    NewGiveaway {
        guild_id: GUILD.to_owned(),
        id: id.to_owned(),
        prize: "Two game keys".to_owned(),
        channel_id: CHANNEL.to_owned(),
        message_id: MESSAGE.to_owned(),
        created_by: MOD.to_owned(),
        winners_count: 2,
        ends_at,
        requirements_json: serde_json::json!({"roles": [], "min_account_age_days": 0}),
        max_entries: 500,
        draw_nonce: NONCE.to_owned(),
        created_at: at(0),
    }
}

fn new_draw(giveaway_id: &str, draw_no: i32, reroll_kind: Option<&str>, seed: &str) -> NewDraw {
    NewDraw {
        guild_id: GUILD.to_owned(),
        giveaway_id: giveaway_id.to_owned(),
        draw_no,
        drawn_at: at(3_700),
        eligible_count: 10,
        eligible_hash: ELIGIBLE_HASH.to_owned(),
        seed: seed.to_owned(),
        winner_ids: serde_json::json!(["100000000000000009", "100000000000000002"]),
        reroll_kind: reroll_kind.map(ToString::to_string),
    }
}

#[tokio::test]
async fn giveaway_crud_round_trips_the_definition() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("giveaway_crud_round_trips_the_definition").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    let created = store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(3_600))).await?;
    assert_eq!(created.guild_id, GUILD);
    assert_eq!(created.prize, "Two game keys");
    assert_eq!(created.winners_count, 2);
    assert_eq!(created.status, "open");
    assert_eq!(created.claim_token, None);
    assert_eq!(created.claimed_at, None);
    assert_eq!(created.announced_at, None);
    assert_eq!(
        created.requirements_json,
        serde_json::json!({"roles": [], "min_account_age_days": 0})
    );
    let read = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("created giveaway reads back");
    assert_eq!(read, created);
    assert_eq!(store::get_giveaway(&pool, GUILD, "missing").await?, None);
    assert!(store::set_giveaway_status(&pool, GUILD, "gw01h455k", "paused", at(10), "open").await?);
    let paused = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("paused giveaway reads back");
    assert_eq!(paused.status, "paused");
    assert!(
        !store::set_giveaway_status(&pool, GUILD, "gw01h455k", "cancelled", at(11), "open").await?
    );
    assert!(!store::set_giveaway_status(&pool, GUILD, "missing", "paused", at(10), "open").await?);
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn giveaway_delete_removes_definitions_entries_and_draws() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("giveaway_delete_removes_definitions_entries_and_draws").await
    else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(3_600))).await?;
    assert!(store::upsert_entry(&pool, GUILD, "gw01h455k", "100000000000000001", at(1)).await?);
    store::insert_draw(&pool, &new_draw("gw01h455k", 0, None, SEED_0)).await?;
    assert!(store::delete_giveaway(&pool, GUILD, "gw01h455k").await?);
    assert_eq!(store::get_giveaway(&pool, GUILD, "gw01h455k").await?, None);
    assert!(store::list_entries(&pool, GUILD, "gw01h455k")
        .await?
        .is_empty());
    assert!(store::get_draws(&pool, GUILD, "gw01h455k")
        .await?
        .is_empty());
    assert_eq!(
        store::latest_draw_no(&pool, GUILD, "gw01h455k").await?,
        None
    );
    assert!(!store::delete_giveaway(&pool, GUILD, "gw01h455k").await?);
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn entry_upsert_collapses_repeats_and_keeps_first_stamp() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("entry_upsert_collapses_repeats_and_keeps_first_stamp").await
    else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(3_600))).await?;
    assert!(store::upsert_entry(&pool, GUILD, "gw01h455k", "100000000000000001", at(1)).await?);
    assert!(!store::upsert_entry(&pool, GUILD, "gw01h455k", "100000000000000001", at(2)).await?);
    assert!(store::upsert_entry(&pool, GUILD, "gw01h455k", "100000000000000002", at(3)).await?);
    let entries = store::list_entries(&pool, GUILD, "gw01h455k").await?;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].user_id, "100000000000000001");
    assert_eq!(
        entries[0].entered_at,
        at(1),
        "the first click wins the stamp"
    );
    assert_eq!(entries[0].weight, 1);
    assert_eq!(entries[1].user_id, "100000000000000002");
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn claim_due_leases_exactly_one_due_giveaway() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("claim_due_leases_exactly_one_due_giveaway").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw_future", at(7_200))).await?;
    store::create_giveaway(&pool, &new_giveaway("gw_late", at(1_800))).await?;
    store::create_giveaway(&pool, &new_giveaway("gw_early", at(900))).await?;
    // Nothing due before the first deadline.
    assert_eq!(
        store::claim_due_giveaway(&pool, GUILD, at(899), at(0), "lease-0")
            .await?
            .map(|row| row.id),
        None
    );
    // Earliest deadline first, one row per call.
    let first = store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-1")
        .await?
        .expect("earliest due giveaway leases");
    assert_eq!(first.id, "gw_early");
    assert_eq!(first.claim_token.as_deref(), Some("lease-1"));
    assert_eq!(first.claimed_at, Some(at(3_600)));
    assert_eq!(first.status, "open", "a claim is a lease, not a draw");
    let second = store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-2")
        .await?
        .expect("next due giveaway leases");
    assert_eq!(second.id, "gw_late");
    assert_eq!(
        store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-3")
            .await?
            .map(|row| row.id),
        None,
        "the future giveaway stays unclaimed"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn claim_due_holds_the_lease_until_reclaim() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("claim_due_holds_the_lease_until_reclaim").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    let claimed = store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-1")
        .await?
        .expect("due giveaway leases");
    assert_eq!(claimed.claim_token.as_deref(), Some("lease-1"));
    // A live lease is not reclaimable.
    assert_eq!(
        store::claim_due_giveaway(&pool, GUILD, at(3_601), at(100), "lease-2")
            .await?
            .map(|row| row.id),
        None
    );
    // A crashed worker's lease expires; the next tick re-claims the row.
    sqlx::query("UPDATE giveaways SET claimed_at = $3 WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind("gw01h455k")
        .bind(at(50))
        .execute(&pool)
        .await?;
    let reclaimed = store::claim_due_giveaway(&pool, GUILD, at(3_602), at(3_602), "lease-3")
        .await?
        .expect("expired lease reclaims");
    assert_eq!(reclaimed.claim_token.as_deref(), Some("lease-3"));
    assert_eq!(reclaimed.claimed_at, Some(at(3_602)));
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn claim_due_contention_leases_exactly_one_winner() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("claim_due_contention_leases_exactly_one_winner").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    let rival = fixture.independent_pool().await.expect("rival pool");
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    let (left, right) = tokio::join!(
        store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-left"),
        store::claim_due_giveaway(&rival, GUILD, at(3_600), at(0), "lease-right")
    );
    let leases = [left?, right?];
    assert_eq!(
        leases.iter().filter(|lease| lease.is_some()).count(),
        1,
        "two tickers racing one due row lease exactly one winner"
    );
    let winner = leases.into_iter().flatten().next().expect("one winner");
    assert!(["lease-left", "lease-right"].contains(&winner.claim_token.as_deref().unwrap_or("")));
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn release_claim_only_honors_the_holder() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("release_claim_only_honors_the_holder").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-1")
        .await?
        .expect("due giveaway leases");
    assert!(!store::release_claim(&pool, GUILD, "gw01h455k", "lease-wrong", at(3_601)).await?);
    let held = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("giveaway reads back");
    assert_eq!(held.claim_token.as_deref(), Some("lease-1"));
    assert!(store::release_claim(&pool, GUILD, "gw01h455k", "lease-1", at(3_601)).await?);
    let released = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("giveaway reads back");
    assert_eq!(released.claim_token, None);
    assert_eq!(released.claimed_at, None);
    assert_eq!(released.updated_at, at(3_601));
    assert!(!store::release_claim(&pool, GUILD, "gw01h455k", "lease-1", at(3_602)).await?);
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn mark_drawn_completes_only_for_the_holder() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("mark_drawn_completes_only_for_the_holder").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-1")
        .await?
        .expect("due giveaway leases");
    assert!(!store::mark_drawn(&pool, GUILD, "gw01h455k", "lease-wrong", at(3_601)).await?);
    let open = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("giveaway reads back");
    assert_eq!(open.status, "open");
    assert!(
        store::set_giveaway_status(&pool, GUILD, "gw01h455k", "cancelled", at(3_602), "open")
            .await?
    );
    assert!(!store::mark_drawn(&pool, GUILD, "gw01h455k", "lease-1", at(3_603)).await?);
    let cancelled = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("giveaway reads back");
    assert_eq!(cancelled.status, "cancelled");
    assert!(
        store::set_giveaway_status(&pool, GUILD, "gw01h455k", "open", at(3_604), "cancelled")
            .await?
    );
    assert!(store::mark_drawn(&pool, GUILD, "gw01h455k", "lease-1", at(3_605)).await?);
    let drawn = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("giveaway reads back");
    assert_eq!(drawn.status, "drawn");
    assert_eq!(drawn.claim_token, None);
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn mark_announced_only_stamps_a_drawn_row_once() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("mark_announced_only_stamps_a_drawn_row_once").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw_open", at(900))).await?;
    store::create_giveaway(&pool, &new_giveaway("gw_paused", at(900))).await?;
    store::create_giveaway(&pool, &new_giveaway("gw_cancelled", at(900))).await?;
    assert!(store::set_giveaway_status(&pool, GUILD, "gw_paused", "paused", at(10), "open").await?);
    assert!(
        store::set_giveaway_status(&pool, GUILD, "gw_cancelled", "cancelled", at(10), "open")
            .await?
    );
    for id in ["gw_open", "gw_paused", "gw_cancelled", "missing"] {
        assert!(
            !store::mark_announced(&pool, GUILD, id, at(3_700)).await?,
            "{id} must refuse the stamp"
        );
    }
    // Drive the open giveaway to drawn through the composed completion, then
    // the first announcement stamps and a second announcer keeps the marker.
    let claimed = store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-1")
        .await?
        .expect("open due giveaway leases");
    assert_eq!(claimed.id, "gw_open");
    store::complete_draw(
        &pool,
        &new_draw("gw_open", 0, None, SEED_0),
        "lease-1",
        at(3_605),
    )
    .await?
    .expect("holder completes");
    assert!(store::mark_announced(&pool, GUILD, "gw_open", at(3_700)).await?);
    assert!(!store::mark_announced(&pool, GUILD, "gw_open", at(3_701)).await?);
    let announced = store::get_giveaway(&pool, GUILD, "gw_open")
        .await?
        .expect("giveaway reads back");
    assert_eq!(announced.announced_at, Some(at(3_700)));
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn complete_draw_rolls_back_when_the_guard_fails() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("complete_draw_rolls_back_when_the_guard_fails").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-1")
        .await?
        .expect("due giveaway leases");
    // A superseded worker's token completes nothing and persists no draw row.
    assert_eq!(
        store::complete_draw(
            &pool,
            &new_draw("gw01h455k", 0, None, SEED_0),
            "lease-wrong",
            at(3_605)
        )
        .await?,
        None
    );
    assert!(store::get_draws(&pool, GUILD, "gw01h455k")
        .await?
        .is_empty());
    let open = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("giveaway reads back");
    assert_eq!(open.status, "open");
    assert_eq!(open.claim_token.as_deref(), Some("lease-1"));
    // A moderator cancel between claim and completion refuses the same way.
    assert!(
        store::set_giveaway_status(&pool, GUILD, "gw01h455k", "cancelled", at(3_606), "open")
            .await?
    );
    assert_eq!(
        store::complete_draw(
            &pool,
            &new_draw("gw01h455k", 0, None, SEED_0),
            "lease-1",
            at(3_607)
        )
        .await?,
        None
    );
    assert!(store::get_draws(&pool, GUILD, "gw01h455k")
        .await?
        .is_empty());
    // Reopened, the holder's completion stores the row and draws the giveaway.
    assert!(
        store::set_giveaway_status(&pool, GUILD, "gw01h455k", "open", at(3_608), "cancelled")
            .await?
    );
    let row = store::complete_draw(
        &pool,
        &new_draw("gw01h455k", 0, None, SEED_0),
        "lease-1",
        at(3_609),
    )
    .await?
    .expect("holder completes");
    assert_eq!(row.draw_no, 0);
    assert_eq!(row.seed, SEED_0);
    assert_eq!(store::get_draws(&pool, GUILD, "gw01h455k").await?.len(), 1);
    let drawn = store::get_giveaway(&pool, GUILD, "gw01h455k")
        .await?
        .expect("giveaway reads back");
    assert_eq!(drawn.status, "drawn");
    assert_eq!(drawn.claim_token, None);
    assert_eq!(drawn.updated_at, at(3_609));
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn draw_insert_stores_replay_inputs_and_lists_rounds() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("draw_insert_stores_replay_inputs_and_lists_rounds").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    assert_eq!(
        store::latest_draw_no(&pool, GUILD, "gw01h455k").await?,
        None
    );
    let first = store::insert_draw(&pool, &new_draw("gw01h455k", 0, None, SEED_0)).await?;
    assert_eq!(first.draw_no, 0);
    assert_eq!(first.eligible_hash, ELIGIBLE_HASH);
    assert_eq!(first.seed, SEED_0);
    assert_eq!(
        first.winner_ids,
        serde_json::json!(["100000000000000009", "100000000000000002"])
    );
    assert_eq!(first.reroll_kind, None);
    assert_eq!(
        store::latest_draw_no(&pool, GUILD, "gw01h455k").await?,
        Some(0)
    );
    store::insert_draw(&pool, &new_draw("gw01h455k", 1, Some("full"), SEED_1)).await?;
    store::insert_draw(
        &pool,
        &new_draw("gw01h455k", 2, Some("single:100000000000000001"), SEED_1),
    )
    .await?;
    let draws = store::get_draws(&pool, GUILD, "gw01h455k").await?;
    assert_eq!(draws.len(), 3);
    assert_eq!(draws[0].draw_no, 0);
    assert_eq!(draws[1].reroll_kind.as_deref(), Some("full"));
    assert_eq!(
        draws[2].reroll_kind.as_deref(),
        Some("single:100000000000000001")
    );
    assert_eq!(
        store::latest_draw_no(&pool, GUILD, "gw01h455k").await?,
        Some(2)
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn raced_draw_inserts_leave_one_row() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("raced_draw_inserts_leave_one_row").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    let rival = fixture.independent_pool().await.expect("rival pool");
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    let left_draw = new_draw("gw01h455k", 0, None, SEED_0);
    let right_draw = new_draw("gw01h455k", 0, None, SEED_0);
    let (left, right) = tokio::join!(
        store::insert_draw(&pool, &left_draw),
        store::insert_draw(&rival, &right_draw)
    );
    let mut outcomes = [left, right];
    outcomes.sort_by_key(|outcome| outcome.is_err());
    assert!(outcomes[0].is_ok(), "one racer wins the round");
    assert_sqlstate(outcomes[1].as_ref().expect_err("one racer loses"), "23505");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD)
    .bind("gw01h455k")
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1, "the same round draws exactly once");
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn non_open_giveaways_are_never_due() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("non_open_giveaways_are_never_due").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    assert!(
        store::set_giveaway_status(&pool, GUILD, "gw01h455k", "cancelled", at(100), "open").await?
    );
    assert_eq!(
        store::claim_due_giveaway(&pool, GUILD, at(3_600), at(0), "lease-1")
            .await?
            .map(|row| row.id),
        None
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn child_rows_stay_bound_to_their_giveaway() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("child_rows_stay_bound_to_their_giveaway").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    // Entries and draws for a missing giveaway fail with a foreign-key
    // violation instead of leaving orphan rows behind.
    let missing_entry =
        store::upsert_entry(&pool, GUILD, "gw-missing", "100000000000000001", at(1))
            .await
            .expect_err("entry without a giveaway must fail");
    assert_sqlstate(&missing_entry, "23503");
    let missing_draw = store::insert_draw(&pool, &new_draw("gw-missing", 0, None, SEED_0))
        .await
        .expect_err("draw without a giveaway must fail");
    assert_sqlstate(&missing_draw, "23503");
    // Deleting a giveaway with live children is refused: the foreign key
    // fails closed instead of orphaning rows, and `delete_giveaway` (which
    // removes children first) still succeeds.
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    assert!(store::upsert_entry(&pool, GUILD, "gw01h455k", "100000000000000001", at(1)).await?);
    store::insert_draw(&pool, &new_draw("gw01h455k", 0, None, SEED_0)).await?;
    let refused = sqlx::query("DELETE FROM giveaways WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind("gw01h455k")
        .execute(&pool)
        .await
        .expect_err("parent delete with live children must fail");
    assert_sqlstate(&refused, "23503");
    assert!(store::delete_giveaway(&pool, GUILD, "gw01h455k").await?);
    let entries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD)
    .bind("gw01h455k")
    .fetch_one(&pool)
    .await?;
    let draws: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD)
    .bind("gw01h455k")
    .fetch_one(&pool)
    .await?;
    assert_eq!((entries, draws), (0, 0), "explicit delete removes all rows");
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn store_chain_enforces_the_giveaway_foreign_keys() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect_store("store_chain_enforces_the_giveaway_foreign_keys").await
    else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    // The store chain (0461, the copy the production boot runner applies)
    // carries the same composite foreign keys as the cutover chain (0460):
    // orphan entries and draws fail, and a parent delete with live children
    // is refused. Stripping either FOREIGN KEY line from 0461 must turn
    // these assertions red.
    let missing_entry = store::upsert_entry(&pool, GUILD, "gw-missing", "100000000000001", at(1))
        .await
        .expect_err("store-chain entry without a giveaway must fail");
    assert_sqlstate(&missing_entry, "23503");
    let missing_draw = store::insert_draw(&pool, &new_draw("gw-missing", 0, None, SEED_0))
        .await
        .expect_err("store-chain draw without a giveaway must fail");
    assert_sqlstate(&missing_draw, "23503");
    store::create_giveaway(&pool, &new_giveaway("gw01h455k", at(900))).await?;
    assert!(store::upsert_entry(&pool, GUILD, "gw01h455k", "100000000000001", at(1)).await?);
    store::insert_draw(&pool, &new_draw("gw01h455k", 0, None, SEED_0)).await?;
    let refused = sqlx::query("DELETE FROM giveaways WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind("gw01h455k")
        .execute(&pool)
        .await
        .expect_err("store-chain parent delete with live children must fail");
    assert_sqlstate(&refused, "23503");
    assert!(store::delete_giveaway(&pool, GUILD, "gw01h455k").await?);
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn create_admitted_refuses_the_11th_active_giveaway() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("create_admitted_refuses_the_11th_active_giveaway").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    for n in 0..10 {
        let id = format!("gw_quota_{n:02}");
        assert!(
            store::create_admitted(&pool, &new_giveaway(&id, at(3_600)))
                .await?
                .is_some(),
            "control create {n} of 10 succeeds"
        );
    }
    assert_eq!(
        store::create_admitted(&pool, &new_giveaway("gw_quota_11th", at(3_600))).await?,
        None,
        "the 11th active giveaway is refused"
    );
    let definitions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM giveaways WHERE guild_id = $1")
        .bind(GUILD)
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        definitions, 10,
        "the refused create inserts no definition row"
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM automation_audit_log WHERE guild_id = $1 AND action = 'giveaway.create'",
    )
    .bind(GUILD)
    .fetch_one(&pool)
    .await?;
    assert_eq!(audits, 10, "the refused create writes no audit row");
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn durable_entry_budget_refuses_the_11th_write_without_the_runtime() -> Result<(), sqlx::Error>
{
    let Some(fixture) =
        connect("durable_entry_budget_refuses_the_11th_write_without_the_runtime").await
    else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    // Direct store calls only: no runtime exists here, so the in-process
    // budget cannot mask the durable arm. Removing the `writes >= 10` check
    // must turn the refusal below red.
    let now = at(100);
    let standing = MemberStanding {
        role_ids: Vec::new(),
        account_age_days: 400,
        guild_age_days: 400,
    };
    for n in 0..10 {
        let id = format!("gw_budget_{n:02}");
        assert!(
            store::create_admitted(&pool, &new_giveaway(&id, at(3_600)))
                .await?
                .is_some(),
            "setup create {n} of 10 succeeds"
        );
    }
    for n in 0..10 {
        let id = format!("gw_budget_{n:02}");
        assert_eq!(
            store::admit_entry(&pool, GUILD, &id, BUDGET_USER, &standing, now).await?,
            EntryAdmission::Entered,
            "entry {n} of 10 is admitted"
        );
    }
    // The 11th attempt needs an 11th definition, past the 10-open quota:
    // cancelling one giveaway frees a slot while its entry row still counts
    // toward the durable per-user budget (entries outlive cancels).
    assert!(
        store::cancel_audited(&pool, GUILD, "gw_budget_00", MOD)
            .await?
            .is_some(),
        "a cancel frees one quota slot"
    );
    assert!(
        store::create_admitted(&pool, &new_giveaway("gw_budget_10", at(3_600)))
            .await?
            .is_some(),
        "the freed slot admits one more definition"
    );
    assert_eq!(
        store::admit_entry(&pool, GUILD, "gw_budget_10", BUDGET_USER, &standing, now).await?,
        EntryAdmission::RateLimited,
        "the 11th write in the minute is refused by the durable budget"
    );
    let writes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(GUILD)
    .bind(BUDGET_USER)
    .fetch_one(&pool)
    .await?;
    assert_eq!(writes, 10, "the refused entry inserts no row");
    assert_eq!(
        store::admit_entry(&pool, GUILD, "gw_budget_10", BUDGET_OTHER, &standing, now).await?,
        EntryAdmission::Entered,
        "a different member is unaffected by the per-user budget"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn pause_resume_extend_move_the_lifecycle() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("pause_resume_extend_move_the_lifecycle").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw_pause_01", at(3_600))).await?;
    assert!(store::pause_giveaway(&pool, GUILD, "gw_pause_01", at(100)).await?);
    assert!(
        !store::pause_giveaway(&pool, GUILD, "gw_pause_01", at(101)).await?,
        "a paused row does not pause twice"
    );
    assert!(
        !store::pause_giveaway(&pool, GUILD, "gw_pause_missing", at(101)).await?,
        "an unknown id does not pause"
    );
    let paused = store::get_giveaway(&pool, GUILD, "gw_pause_01")
        .await?
        .expect("paused row");
    assert_eq!(paused.status, "paused");
    assert_eq!(paused.paused_at, Some(at(100)));
    let resumed = store::resume_giveaway(&pool, GUILD, "gw_pause_01", at(100 + 1_800))
        .await?
        .expect("resume extends the deadline");
    assert_eq!(
        resumed.unix_timestamp(),
        at(3_600 + 1_800).unix_timestamp(),
        "resume adds the paused span to the deadline"
    );
    let reopened = store::get_giveaway(&pool, GUILD, "gw_pause_01")
        .await?
        .expect("resumed row");
    assert_eq!(reopened.status, "open");
    assert_eq!(reopened.paused_at, None);
    assert!(
        store::resume_giveaway(&pool, GUILD, "gw_pause_01", at(2_000))
            .await?
            .is_none(),
        "an open row does not resume"
    );
    let extended = store::extend_giveaway(&pool, GUILD, "gw_pause_01", 600, at(2_000))
        .await?
        .expect("extend adds seconds");
    assert_eq!(
        extended.unix_timestamp(),
        at(3_600 + 1_800 + 600).unix_timestamp()
    );
    assert!(
        store::extend_giveaway(&pool, GUILD, "gw_pause_01", 0, at(2_001))
            .await?
            .is_none(),
        "an out-of-bounds extension changes nothing"
    );
    assert!(
        store::cancel_audited(&pool, GUILD, "gw_pause_01", MOD)
            .await?
            .is_some(),
        "cancel the control row"
    );
    assert!(
        store::extend_giveaway(&pool, GUILD, "gw_pause_01", 600, at(2_002))
            .await?
            .is_none(),
        "a cancelled row does not extend"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn claim_specific_leases_one_named_giveaway() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("claim_specific_leases_one_named_giveaway").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw_end_01", at(3_600))).await?;
    store::create_giveaway(&pool, &new_giveaway("gw_end_02", at(3_600))).await?;
    assert!(store::pause_giveaway(&pool, GUILD, "gw_end_02", at(10)).await?);
    let open = store::claim_specific(&pool, GUILD, "gw_end_01", at(100), at(0), "claim-open")
        .await?
        .expect("open giveaway claims");
    assert_eq!(open.claim_token.as_deref(), Some("claim-open"));
    let paused = store::claim_specific(&pool, GUILD, "gw_end_02", at(100), at(0), "claim-paused")
        .await?
        .expect("paused giveaway claims for draw-now");
    assert_eq!(paused.status, "paused");
    assert_eq!(
        store::claim_specific(&pool, GUILD, "gw_end_01", at(101), at(0), "claim-race").await?,
        None,
        "a live claim is not stolen"
    );
    assert_eq!(
        store::claim_specific(
            &pool,
            GUILD,
            "gw_end_missing",
            at(101),
            at(0),
            "claim-missing"
        )
        .await?,
        None,
        "an unknown id does not claim"
    );
    let reclaimed =
        store::claim_specific(&pool, GUILD, "gw_end_01", at(800), at(700), "claim-reclaim")
            .await?
            .expect("an expired lease reclaims");
    assert_eq!(reclaimed.claim_token.as_deref(), Some("claim-reclaim"));
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn commit_draw_from_commits_paused_and_rolls_back_on_guard() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("commit_draw_from_commits_paused_and_rolls_back_on_guard").await
    else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw_commit_01", at(3_600))).await?;
    assert!(store::pause_giveaway(&pool, GUILD, "gw_commit_01", at(10)).await?);
    assert!(
        store::claim_specific(&pool, GUILD, "gw_commit_01", at(100), at(0), "claim-commit")
            .await?
            .is_some()
    );
    let draw = store::commit_draw_from(
        &pool,
        &new_draw("gw_commit_01", 0, None, SEED_0),
        "claim-commit",
        at(101),
        "paused",
    )
    .await?
    .expect("paused draw commits");
    assert_eq!(draw.draw_no, 0);
    let settled = store::get_giveaway(&pool, GUILD, "gw_commit_01")
        .await?
        .expect("drawn row");
    assert_eq!(settled.status, "drawn");
    assert_eq!(settled.paused_at, None);
    assert_eq!(settled.claim_token, None);
    store::create_giveaway(&pool, &new_giveaway("gw_commit_02", at(3_600))).await?;
    assert!(
        store::claim_specific(&pool, GUILD, "gw_commit_02", at(100), at(0), "claim-stale")
            .await?
            .is_some()
    );
    assert!(
        store::cancel_audited(&pool, GUILD, "gw_commit_02", MOD)
            .await?
            .is_some(),
        "a cancel between claim and commit clears the claim"
    );
    assert_eq!(
        store::commit_draw_from(
            &pool,
            &new_draw("gw_commit_02", 0, None, SEED_0),
            "claim-stale",
            at(101),
            "open",
        )
        .await?,
        None,
        "a stale claim commits nothing"
    );
    assert!(
        store::get_draws(&pool, GUILD, "gw_commit_02")
            .await?
            .is_empty(),
        "the rolled-back insert leaves no orphan draw row"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn unannounced_sweep_lists_drawn_until_marked() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("unannounced_sweep_lists_drawn_until_marked").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw_sweep_01", at(3_600))).await?;
    assert!(
        store::claim_specific(&pool, GUILD, "gw_sweep_01", at(100), at(0), "claim-sweep")
            .await?
            .is_some()
    );
    assert!(store::complete_draw(
        &pool,
        &new_draw("gw_sweep_01", 0, None, SEED_0),
        "claim-sweep",
        at(101)
    )
    .await?
    .is_some());
    let pending = store::list_unannounced(&pool, GUILD, 25).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "gw_sweep_01");
    assert!(store::mark_announced(&pool, GUILD, "gw_sweep_01", at(102)).await?);
    assert!(
        store::list_unannounced(&pool, GUILD, 25).await?.is_empty(),
        "a marked row leaves the sweep"
    );
    assert!(store::unmark_announced(&pool, GUILD, "gw_sweep_01", at(103)).await?);
    assert_eq!(
        store::list_unannounced(&pool, GUILD, 25).await?.len(),
        1,
        "reopening the marker rejoins the sweep"
    );
    assert!(
        !store::unmark_announced(&pool, GUILD, "gw_sweep_missing", at(104)).await?,
        "an unknown id does not reopen"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn draw_and_reroll_audits_carry_round_ids_idempotently() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("draw_and_reroll_audits_carry_round_ids_idempotently").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    store::create_giveaway(&pool, &new_giveaway("gw_audit_01", at(3_600))).await?;
    store::audit_draw(&pool, GUILD, "gw_audit_01", 0, None, at(100)).await?;
    store::audit_reroll(&pool, GUILD, "gw_audit_01", 1, Some(MOD), at(200)).await?;
    store::audit_draw(&pool, GUILD, "gw_audit_01", 0, None, at(101)).await?;
    let audits: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT action::text, target_key::text, outcome::text FROM automation_audit_log \
         WHERE guild_id = $1 AND action IN ('giveaway.draw', 'giveaway.reroll') ORDER BY id",
    )
    .bind(GUILD)
    .fetch_all(&pool)
    .await?;
    assert_eq!(audits.len(), 2, "the rewrite is a no-op, not a duplicate");
    assert_eq!(
        audits[0],
        (
            "giveaway.draw".to_owned(),
            Some("gw_audit_01".to_owned()),
            "ok".to_owned()
        )
    );
    assert_eq!(audits[1].0, "giveaway.reroll");
    assert_eq!(audits[1].1.as_deref(), Some("gw_audit_01"));
    assert!(
        !format!("{audits:?}").contains("Two game keys"),
        "audit rows carry ids, never prize text"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn paused_at_column_exists_on_both_chains() -> Result<(), sqlx::Error> {
    let Some(cutover) = connect("paused_at_column_exists_on_both_chains").await else {
        return Ok(());
    };
    let Some(stored) = connect_store("paused_at_column_exists_on_both_chains").await else {
        cutover.close().await.expect("drop test database");
        return Ok(());
    };
    for pool in [cutover.pool(), stored.pool()] {
        let nullable: Option<String> = sqlx::query_scalar(
            "SELECT is_nullable FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'giveaways' AND column_name = 'paused_at'",
        )
        .fetch_optional(pool)
        .await?;
        assert_eq!(
            nullable.as_deref(),
            Some("YES"),
            "paused_at is a nullable column on this chain"
        );
    }
    cutover.close().await.expect("drop test database");
    stored.close().await.expect("drop test database");
    Ok(())
}
