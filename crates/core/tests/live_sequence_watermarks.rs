//! Reservation and recovery against a disposable Postgres database.

#![cfg(feature = "db")]

use std::future::pending;

use sqlx::{migrate::Migrator, PgPool};
use time::OffsetDateTime;
use two_bot_core::realtime_wire::{
    recovery::{PublishError, Recovery},
    watermark::{read, reserve, ReservationError},
    Guild, Id, Member, Permissions, Role, Topics, WireTime, MAX_SEQ,
};
use two_bot_testsupport::TestDatabase;

fn id(value: u64) -> Id {
    Id(value.try_into().unwrap())
}

async fn fixture() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    TestDatabase::create(&url, &Migrator::DEFAULT)
        .await
        .unwrap()
}

async fn install(pool: &PgPool) {
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0482_live_sequence_watermarks.sql"
    ))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn one_reservation_wins_and_a_new_pool_continues_each_guild() {
    let db = fixture().await;
    let first = db.pool();
    install(first).await;
    let second = db.independent_pool().await.unwrap();
    let guild = id(42);
    let other = id(u64::MAX);
    assert_eq!(read(first, guild).await, Ok(0));
    assert_eq!(
        reserve(first, guild, 1, 2).await,
        Err(ReservationError::Conflict)
    );
    for expected in [0, 1] {
        let (a, b) = tokio::join!(
            reserve(first, guild, expected, expected + 1),
            reserve(&second, guild, expected, expected + 1)
        );
        assert!(matches!(
            (a, b),
            (Ok(()), Err(ReservationError::Conflict)) | (Err(ReservationError::Conflict), Ok(()))
        ));
    }
    assert_eq!(read(&second, other).await, Ok(0));
    reserve(&second, other, 0, 1).await.unwrap();
    first.close().await;
    second.close().await;

    let restarted = db.independent_pool().await.unwrap();
    assert_eq!(read(&restarted, guild).await, Ok(2));
    assert_eq!(read(&restarted, other).await, Ok(1));
    reserve(&restarted, guild, 2, 3).await.unwrap();
    assert_eq!(read(&restarted, other).await, Ok(1));
    assert_eq!(read(&restarted, guild).await, Ok(3));
    db.close().await.unwrap();
}

#[tokio::test]
async fn sql_and_reservation_enforce_the_wire_bounds() {
    let db = fixture().await;
    let pool = db.pool();
    install(pool).await;
    for (guild, seq) in [
        ("42", 0),
        ("42", -1),
        ("42", i64::try_from(MAX_SEQ + 1).unwrap()),
        ("0", 1),
        ("01", 1),
        ("18446744073709551616", 1),
    ] {
        let error = sqlx::query("INSERT INTO public.live_sequence_watermarks VALUES ($1, $2)")
            .bind(guild)
            .bind(seq)
            .execute(pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }

    let guild = id(42);
    for (expected, next) in [
        (0, 0),
        (0, 3),
        (2, 1),
        (MAX_SEQ, MAX_SEQ + 1),
        (u64::MAX, 1),
    ] {
        assert_eq!(
            reserve(pool, guild, expected, next).await,
            Err(ReservationError::InvalidAdvance)
        );
    }
    assert_eq!(read(pool, guild).await, Ok(0));
    sqlx::query("INSERT INTO public.live_sequence_watermarks VALUES ('42', $1)")
        .bind(i64::try_from(MAX_SEQ - 1).unwrap())
        .execute(pool)
        .await
        .unwrap();
    reserve(pool, guild, MAX_SEQ - 1, MAX_SEQ).await.unwrap();
    assert_eq!(read(pool, guild).await, Ok(MAX_SEQ));
    assert_eq!(
        reserve(pool, guild, MAX_SEQ, MAX_SEQ + 1).await,
        Err(ReservationError::InvalidAdvance)
    );
    db.close().await.unwrap();
}

fn topics(guild: Id) -> Topics {
    let owner_id = id(9);
    Topics {
        guild: Guild {
            owner_id,
            afk_channel_id: None,
        },
        channels: vec![],
        roles: vec![Role {
            id: guild,
            permissions: Permissions(0),
        }],
        members: vec![Member {
            user_id: owner_id,
            role_ids: vec![],
            nick: None,
            bot: false,
            pending: false,
        }],
        voice: vec![],
        events: vec![],
    }
}

#[tokio::test]
async fn recovery_withholds_state_until_a_fresh_reservation_covers_the_gap() {
    let db = fixture().await;
    let pool = db.pool();
    let guild = id(42);
    let at = WireTime(OffsetDateTime::UNIX_EPOCH);
    let mut recovery = Recovery::new(guild);
    let generation = recovery.begin();
    let persist = async |guild, expected, next| reserve(pool, guild, expected, next).await;
    assert_eq!(read(pool, guild).await, Err(ReservationError::Storage));
    assert_eq!(
        recovery
            .publish(&generation, 0, at, topics(guild), persist)
            .await,
        Err(PublishError::Persist(ReservationError::Storage))
    );
    assert!(recovery.baseline().is_none());

    install(pool).await;
    recovery
        .publish(&generation, 0, at, topics(guild), persist)
        .await
        .unwrap();
    let published = recovery.baseline().unwrap().seq;
    let generation = recovery.begin();

    // Lose the result after Postgres committed the next delta reservation.
    let (committed, observed_commit) = tokio::sync::oneshot::channel();
    tokio::select! {
        biased;
        () = async {
            reserve(pool, guild, published, published + 1).await.unwrap();
            committed.send(()).unwrap();
            pending::<()>().await;
        } => panic!("reservation acknowledgement must be withheld"),
        result = observed_commit => result.unwrap(),
    }
    assert_eq!(
        reserve(pool, guild, published, published + 1).await,
        Err(ReservationError::Conflict)
    );
    assert_eq!(
        recovery
            .publish(&generation, published, at, topics(guild), persist)
            .await,
        Err(PublishError::Persist(ReservationError::Conflict))
    );
    assert!(recovery.baseline().is_none());

    let durable = read(pool, guild).await.unwrap();
    assert_eq!(durable, published + 1);
    let marker = recovery
        .publish(&generation, durable, at, topics(guild), persist)
        .await
        .unwrap();
    let marker: serde_json::Value = serde_json::from_slice(&marker).unwrap();
    let baseline = recovery.baseline().unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&baseline.snapshot).unwrap();
    // The installed hub withdraws on a forward gap and rejects a snapshot
    // below the marker. This pair supplies a covering snapshot.
    assert_eq!(baseline.seq, durable + 2);
    assert_eq!(marker["seq"], baseline.seq);
    assert_eq!(snapshot["seq"], marker["seq"]);
    assert_eq!(read(pool, guild).await, Ok(baseline.seq));
    db.close().await.unwrap();
}
