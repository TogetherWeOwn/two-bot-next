#![cfg(feature = "db")]
//! rules_accepted capture: gateway gate-clear → deferred buffer → Postgres.
//!
//! One gate-clear round-trips through `rules_accepted_fact` + `record_fact`
//! with the once-per-member key; a redelivered clear reports a duplicate and
//! inserts nothing.

use std::str::FromStr;

use sqlx::PgPool;
use twilight_model::{
    gateway::{event::Event, payload::incoming::MemberAdd},
    guild::{Member, MemberFlags},
    id::Id,
    user::User,
    util::Timestamp,
};
use two_bot_core::{ClassifierConfig, MemStore, NoopLeveling};
use two_bot_discord::{
    CommunityDrainOutcome, CommunityFactsRuntime, DeferredCommunityFacts, NoClassification,
    Pipeline, ScriptedInvites,
};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 100_000_000_000_000_007;
const MEMBER: u64 = 900_000_000_000_007_001;
const OTHER: u64 = 900_000_000_000_007_002;

fn ts(s: &str) -> Timestamp {
    Timestamp::from_str(&format!("2026-09-20T{s}.000+00:00")).expect("fixture stamp")
}

fn user(id: u64) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot: false,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "member".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

fn join_event(member_id: u64, joined: &str) -> Event {
    Event::MemberAdd(Box::new(MemberAdd {
        guild_id: Id::new(GUILD),
        member: Member {
            avatar: None,
            avatar_decoration_data: None,
            banner: None,
            communication_disabled_until: None,
            deaf: false,
            flags: MemberFlags::empty(),
            joined_at: Some(ts(joined)),
            mute: false,
            nick: None,
            pending: false,
            premium_since: None,
            roles: vec![],
            user: user(member_id),
        },
    }))
}

struct Fixture {
    _db: TestDatabase,
    pool: PgPool,
    pipeline:
        Pipeline<MemStore, NoopLeveling, DeferredCommunityFacts, ScriptedInvites, NoClassification>,
    buffer: DeferredCommunityFacts,
    runtime: CommunityFactsRuntime,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated test database; no credential fallback");
        let pool = db.pool().clone();
        let buffer = DeferredCommunityFacts::new();
        let pipeline = Pipeline::new(
            MemStore::new(),
            Some(NoopLeveling),
            Some(buffer.clone()),
            ScriptedInvites::new(),
            NoClassification,
        );
        let runtime = CommunityFactsRuntime::new(pool.clone(), ClassifierConfig::default());
        Self {
            _db: db,
            pool,
            pipeline,
            buffer,
            runtime,
        }
    }

    async fn drain(&self) -> CommunityDrainOutcome {
        let writes = self.buffer.take();
        assert!(!writes.is_empty(), "gate-clear must buffer a fact");
        self.runtime
            .drain_writes(writes)
            .await
            .expect("drain persists")
    }
}

/// One gate-clear inserts one `rules_accepted` row under the once-per-member
/// key; redelivering the same clear reports a duplicate and changes nothing.
#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn gate_clear_drains_one_fact_and_dedupes_repeats() {
    let fx = Fixture::new().await;

    fx.pipeline
        .handle_at(&join_event(MEMBER, "12:00:00"), "2026-09-20T12:10:00.000Z");
    let outcome = fx.drain().await;
    assert_eq!((outcome.inserted, outcome.duplicates), (1, 0));

    let row: (String, String, String, String, String, String) = sqlx::query_as(
        "SELECT event_type, source_event_id, actor_id, occurred_at, source, idempotency_key
           FROM community_facts",
    )
    .fetch_one(&fx.pool)
    .await
    .expect("fact row reads back");
    assert_eq!(row.0, "rules_accepted");
    assert_eq!(row.1, format!("{GUILD}:{MEMBER}:rules"));
    assert_eq!(row.2, MEMBER.to_string());
    // The fact carries Discord's joined_at, not the receipt time.
    assert_eq!(row.3, "2026-09-20T12:00:00.000Z");
    assert_eq!(row.4, "gateway");
    assert_eq!(row.5, format!("rules-accepted:{GUILD}:{MEMBER}"));
    let classification: String = sqlx::query_scalar("SELECT classification FROM community_facts")
        .fetch_one(&fx.pool)
        .await
        .expect("classification reads back");
    assert_eq!(classification, "eligible_human");

    // Redelivered clear: same member, same key — duplicate, no second row.
    fx.pipeline
        .handle_at(&join_event(MEMBER, "12:00:00"), "2026-09-20T12:10:00.000Z");
    let outcome = fx.drain().await;
    assert_eq!((outcome.inserted, outcome.duplicates), (0, 1));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_facts")
        .fetch_one(&fx.pool)
        .await
        .expect("count reads back");
    assert_eq!(count, 1);

    // A second member inserts under its own key.
    fx.pipeline
        .handle_at(&join_event(OTHER, "12:05:00"), "2026-09-20T12:10:00.000Z");
    let outcome = fx.drain().await;
    assert_eq!((outcome.inserted, outcome.duplicates), (1, 0));
}
