#![cfg(feature = "db")]
//! rules_accepted + member_joined capture: gateway events → deferred buffer → Postgres.
//!
//! One gate-clear round-trips through `rules_accepted_fact` + `record_fact`
//! with the once-per-member key; a redelivered clear reports a duplicate and
//! inserts nothing. One gateway join round-trips through `member_join_fact` +
//! `record_fact` with the per-join key, carrying its invite attribution; a
//! redelivered burst dedupes to zero.

use std::str::FromStr;

use sqlx::PgPool;
use twilight_model::{
    gateway::{event::Event, payload::incoming::MemberAdd},
    guild::{Member, MemberFlags},
    id::Id,
    user::User,
    util::Timestamp,
};
use two_bot_core::{InviteState, MemStore, NoopLeveling};
use two_bot_discord::{DeferredCommunityFacts, NoClassification, Pipeline, ScriptedInvites};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 100_000_000_000_000_007;
const MEMBER: u64 = 900_000_000_000_007_001;
const OTHER: u64 = 900_000_000_000_007_002;
const BOT: u64 = 900_000_000_000_007_003;
const INVITER: u64 = 900_000_000_000_007_099;

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

/// Pending arrival: buffers a join fact only, with no instant gate-clear, so
/// join capture tests stay isolated from the `rules_accepted` writer.
fn pending_join_event(member_id: u64, joined: &str) -> Event {
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
            pending: true,
            premium_since: None,
            roles: vec![],
            user: user(member_id),
        },
    }))
}

fn invite_snapshot(uses: u64) -> Vec<InviteState> {
    vec![InviteState {
        code: "twodev01".to_owned(),
        uses,
        inviter_id: Some(INVITER),
        channel_id: None,
    }]
}

struct Fixture {
    _db: TestDatabase,
    pool: PgPool,
    pipeline:
        Pipeline<MemStore, NoopLeveling, DeferredCommunityFacts, ScriptedInvites, NoClassification>,
    buffer: DeferredCommunityFacts,
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
        buffer.enable(pool.clone());
        let pipeline = Pipeline::new(
            MemStore::new(),
            Some(NoopLeveling),
            Some(buffer.clone()),
            ScriptedInvites::new(),
            NoClassification,
        );
        Self {
            _db: db,
            pool,
            pipeline,
            buffer,
        }
    }

    /// Drain the shared sink: returns the inserted count (a redelivered clear
    /// dedupes to zero through the once-per-member key).
    async fn drain(&self) -> usize {
        assert!(!self.buffer.is_empty(), "gate-clear must buffer a fact");
        self.buffer.drain().await.expect("drain persists")
    }
}

/// One gate-clear inserts one `rules_accepted` row under the once-per-member
/// key; redelivering the same clear reports a duplicate and changes nothing.
/// The non-pending join also co-buffers its `member_joined` fact (covered in
/// `member_join_drains_one_fact_and_dedupes_repeats`), so drains here persist
/// two rows and the rules assertions filter by event type.
#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn gate_clear_drains_one_fact_and_dedupes_repeats() {
    let fx = Fixture::new().await;

    fx.pipeline
        .handle_at(&join_event(MEMBER, "12:00:00"), "2026-09-20T12:10:00.000Z");
    assert_eq!(fx.drain().await, 2);

    let row: (String, String, String, String, String, String) = sqlx::query_as(
        "SELECT event_type, source_event_id, actor_id, occurred_at, source, idempotency_key
           FROM community_facts WHERE event_type='rules_accepted'",
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
    let classification: String = sqlx::query_scalar(
        "SELECT classification FROM community_facts WHERE event_type='rules_accepted'",
    )
    .fetch_one(&fx.pool)
    .await
    .expect("classification reads back");
    assert_eq!(classification, "eligible_human");

    // Redelivered clear: same member, same key — duplicate, no second row.
    fx.pipeline
        .handle_at(&join_event(MEMBER, "12:00:00"), "2026-09-20T12:10:00.000Z");
    assert_eq!(fx.drain().await, 0);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM community_facts WHERE event_type='rules_accepted'",
    )
    .fetch_one(&fx.pool)
    .await
    .expect("count reads back");
    assert_eq!(count, 1);

    // A second member inserts under its own key (plus its join fact).
    fx.pipeline
        .handle_at(&join_event(OTHER, "12:05:00"), "2026-09-20T12:10:00.000Z");
    assert_eq!(fx.drain().await, 2);
}

/// One gateway join inserts one `member_joined` row under the per-join key,
/// carrying its invite attribution; a redelivered burst dedupes to zero, and
/// a bot join is captured as `bot`.
#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn member_join_drains_one_fact_and_dedupes_repeats() {
    let fx = Fixture::new().await;

    // Script one invite growth so the join attributes source + inviter.
    fx.pipeline.invite_source().push(GUILD, invite_snapshot(5));
    fx.pipeline.prime_invite_snapshot(GUILD);
    fx.pipeline.invite_source().push(GUILD, invite_snapshot(6));

    // Pending arrival buffers a join fact only (no instant gate-clear).
    fx.pipeline.handle_at(
        &pending_join_event(MEMBER, "12:00:00"),
        "2026-09-20T12:10:00.000Z",
    );
    assert_eq!(fx.buffer.joins_len(), 1, "join must buffer a fact");
    assert_eq!(fx.buffer.drain().await.expect("drain persists"), 1);

    let row: (String, String, String, String, String, String, Option<String>) = sqlx::query_as(
        "SELECT event_type, source_event_id, actor_id, occurred_at, source, idempotency_key, metadata
           FROM community_facts WHERE event_type='member_joined'",
    )
    .fetch_one(&fx.pool)
    .await
    .expect("fact row reads back");
    assert_eq!(row.0, "member_joined");
    assert_eq!(row.1, format!("{GUILD}:{MEMBER}:2026-09-20T12:00:00.000Z"));
    assert_eq!(row.2, MEMBER.to_string());
    // The fact carries Discord's joined_at, not the receipt time.
    assert_eq!(row.3, "2026-09-20T12:00:00.000Z");
    assert_eq!(row.4, "invite:twodev01");
    assert_eq!(
        row.5,
        format!("member-join:{GUILD}:{MEMBER}:2026-09-20T12:00:00.000Z")
    );
    let expected_metadata = format!("{{\"inviterId\":\"{INVITER}\"}}");
    assert_eq!(
        row.6.as_deref(),
        Some(expected_metadata.as_str()),
        "the inviter attribution survives the round trip"
    );
    let classification: String = sqlx::query_scalar(
        "SELECT classification FROM community_facts WHERE event_type='member_joined'",
    )
    .fetch_one(&fx.pool)
    .await
    .expect("classification reads back");
    assert_eq!(classification, "eligible_human");

    // Redelivered burst: same join, same key — duplicate, no second row.
    fx.pipeline.handle_at(
        &pending_join_event(MEMBER, "12:00:00"),
        "2026-09-20T12:10:00.000Z",
    );
    assert_eq!(fx.buffer.joins_len(), 1, "the burst buffers, then loses");
    assert_eq!(fx.buffer.drain().await.expect("retry dedupes"), 0);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM community_facts WHERE event_type='member_joined'")
            .fetch_one(&fx.pool)
            .await
            .expect("count reads back");
    assert_eq!(count, 1);

    // A second member inserts under its own key (unattributed this time).
    fx.pipeline.handle_at(
        &pending_join_event(OTHER, "12:05:00"),
        "2026-09-20T12:10:00.000Z",
    );
    assert_eq!(fx.buffer.drain().await.expect("drain persists"), 1);

    // A bot join is captured as `bot`.
    let mut bot_join = pending_join_event(BOT, "12:06:00");
    if let Event::MemberAdd(ref mut add) = bot_join {
        add.member.user.bot = true;
    }
    fx.pipeline.handle_at(&bot_join, "2026-09-20T12:10:00.000Z");
    assert_eq!(fx.buffer.drain().await.expect("drain persists"), 1);
    let bot_class: String =
        sqlx::query_scalar("SELECT classification FROM community_facts WHERE actor_id=$1")
            .bind(BOT.to_string())
            .fetch_one(&fx.pool)
            .await
            .expect("bot classification reads back");
    assert_eq!(bot_class, "bot");
}
