//! Protected acceptance tests: gateway activity timestamps must honor the
//! guild analytics-family toggles of their originating family.
//!
//! Contract under test (from the card spec, blind to the implementation):
//!
//! - A message (activity family) or voice (voice family) touch that reaches
//!   the gateway timestamp buffer must keep its origin through the batch, and
//!   the batch commit must apply the matching guild toggle inside its
//!   transaction before creating or advancing `members.last_active_at`.
//! - An isolated event from a disabled family advances nothing: it creates no
//!   `last_active_at` and does not move an existing one, in the same batch or
//!   in a later batch. In a mixed batch a newer disabled-family timestamp
//!   never replaces the allowed family's timestamp.
//! - The other enabled family, and the default no-settings-row behavior
//!   (every family enabled), still advance `last_active_at` monotonically.
//! - Guild-scoped suppression (opt-out) still blocks every family, bot
//!   members never gain `last_active_at`, and membership lifecycle rows
//!   (join/leave) persist even with every analytics family disabled.
//! - No new persisted schema: these tests run against the existing migrated
//!   schema and read only `members` / `events`.
//!
//! Every scenario drives the public runtime seam end to end —
//! `FunnelHandlers` over a `GatewayFunnelBuffer`, then
//! `GatewaySessionStore::commit_dispatch` — so the tests pin behavior, not
//! the builder's internal representation of activity origin. Fixtures avoid
//! the milestone projection masking the forbidden write: milestone events
//! from a disabled family are already dropped at commit, and the
//! cross-batch cases use post-milestone touches (a second voice join, a
//! fourth message) whose only `last_active_at` path is the timestamp buffer.
//!
//! PROTECTED: the paired builder implements production code against this
//! file unchanged. Do not edit these tests to make an implementation pass;
//! if a test is wrong, that is a finding for the test author, not a builder
//! edit.

use two_bot_core::database_tls::TlsPolicy;
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::{DispatchAction, GatewaySession};
use two_bot_core::{ChannelClass, FunnelHandlers, JoinInput, MessageInput, VoiceInput};
use two_bot_cutover::db::CutoverDb;
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_cutover::privacy::set_analytics_settings;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const MEMBER: &str = "123456789012345678";
const BOT_MEMBER: &str = "123456789012345679";

const T0: &str = "2026-10-13T00:00:00.000Z";
const T1: &str = "2026-10-13T01:00:00.000Z";
const T2: &str = "2026-10-13T02:00:00.000Z";
const T3: &str = "2026-10-13T03:00:00.000Z";
const T4: &str = "2026-10-13T04:00:00.000Z";

fn guild_u64() -> u64 {
    GUILD.parse().unwrap()
}

fn member_u64(member: &str) -> u64 {
    member.parse().unwrap()
}

async fn database() -> Option<(TestDatabase, CutoverDb)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the gateway activity family acceptance database"
            );
            eprintln!("SKIP gateway activity family acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let test_db = TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
        .await
        .expect("disposable migrated database");
    let fixture_url = match base_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", test_db.name()),
        None => base_url,
    };
    let db = two_bot_cutover::connect_with_tls(&fixture_url, 5, true, TlsPolicy::LocalOnly)
        .await
        .expect("connect to owned disposable database");
    Some((test_db, db))
}

type Handlers = FunnelHandlers<GatewayFunnelBuffer>;

fn handlers() -> Handlers {
    FunnelHandlers::new(GatewayFunnelBuffer::default(), None, None)
}

fn message(member: &str, at: &str, is_bot: bool) -> MessageInput {
    MessageInput {
        guild_id: guild_u64(),
        member_id: member_u64(member),
        is_bot,
        message_id: None,
        webhook_id: None,
        is_staff_automation: false,
        channel_id: 4444,
        channel_class: ChannelClass::Human,
        capture_only: false,
        occurred_at: Some(at.to_owned()),
        content_length: None,
        has_embed_or_attachment: false,
    }
}

fn voice(member: &str, at: &str, is_bot: bool) -> VoiceInput {
    VoiceInput {
        guild_id: guild_u64(),
        member_id: member_u64(member),
        is_bot,
        channel_id: 5555,
        occurred_at: Some(at.to_owned()),
    }
}

/// One serial dispatch: drain the buffer into one batch commit, exactly like
/// the gateway worker does after handling a dispatch.
struct Dispatcher {
    store: GatewaySessionStore,
    sequence: u64,
}

impl Dispatcher {
    fn new(db: &CutoverDb) -> Self {
        Self {
            store: GatewaySessionStore::new(db.pool().clone(), GUILD.to_owned(), 0),
            sequence: 0,
        }
    }

    async fn commit(&mut self, handlers: &Handlers) {
        self.sequence += 1;
        let session = GatewaySession {
            session_id: "family-acceptance".into(),
            sequence: self.sequence,
            resume_url: "ws://family-acceptance".into(),
            updated_at_ms: 1_760_000_000_000 + self.sequence as i64,
        };
        assert_eq!(
            self.store
                .commit_dispatch(&session, handlers.store().take_batch())
                .await
                .expect("gateway batch commit"),
            DispatchAction::Apply,
            "each acceptance dispatch applies exactly once"
        );
    }
}

/// `last_active_at` rendered like the fixture stamps, or `None` when the
/// member has no row or the column was never set. The contract only ever
/// distinguishes "never advanced" from an exact stamp, so both absent forms
/// collapse to `None`.
async fn last_active_at(db: &CutoverDb, member: &str) -> Option<String> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT to_char(last_active_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         FROM members WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD)
    .bind(member)
    .fetch_optional(db.pool())
    .await
    .unwrap();
    row.and_then(|(stamp,)| stamp)
}

async fn event_count(db: &CutoverDb, member: &str, event_type: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = $3",
    )
    .bind(GUILD)
    .bind(member)
    .bind(event_type)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn suppress(db: &CutoverDb, member: &str) {
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind(GUILD)
        .bind(member)
        .execute(db.pool())
        .await
        .expect("suppression insert");
}

/// Pure buffer staging: one message, one voice join and one voice leave each
/// stage exactly one activity entry. This pins that touches survive the
/// buffer for both families without dictating how origin is represented
/// inside an entry; the commit-level tests below prove the origin survives
/// in a form the toggle gate can use.
#[test]
fn buffer_stages_one_activity_entry_per_message_and_voice_touch() {
    let handlers = handlers();
    handlers.on_message(message(MEMBER, T1, false));
    handlers.on_voice_join(voice(MEMBER, T2, false));
    handlers.on_voice_leave(voice(MEMBER, T3, false));
    let batch = handlers.store().take_batch();
    assert_eq!(
        batch.activity.len(),
        3,
        "message, voice join and voice leave each stage one activity entry"
    );
    assert!(
        handlers.store().take_batch().activity.is_empty(),
        "taking the batch drains staged activity"
    );
}

/// Isolated voice origin, voice family disabled: neither the join touch nor
/// the leave touch may create `last_active_at`.
#[tokio::test]
async fn voice_only_activity_advances_nothing_when_voice_disabled() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    // presence on, activity on, voice OFF.
    set_analytics_settings(&db, GUILD, true, true, false)
        .await
        .unwrap();

    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);
    handlers.on_voice_join(voice(MEMBER, T1, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await,
        None,
        "voice join with voice analytics disabled creates no last_active_at"
    );

    handlers.on_voice_leave(voice(MEMBER, T2, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await,
        None,
        "voice leave with voice analytics disabled creates no last_active_at"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Isolated message origin, activity family disabled: the message touch may
/// not create `last_active_at`.
#[tokio::test]
async fn message_only_activity_advances_nothing_when_activity_disabled() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    // presence on, activity OFF, voice on.
    set_analytics_settings(&db, GUILD, true, false, true)
        .await
        .unwrap();

    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);
    handlers.on_message(message(MEMBER, T1, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await,
        None,
        "message with activity analytics disabled creates no last_active_at"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Mixed batch, voice disabled: an earlier allowed message stamp wins over a
/// newer voice stamp staged in the same batch.
#[tokio::test]
async fn mixed_batch_newer_voice_cannot_replace_allowed_message_timestamp() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    set_analytics_settings(&db, GUILD, true, true, false)
        .await
        .unwrap();

    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);
    handlers.on_message(message(MEMBER, T1, false));
    handlers.on_voice_join(voice(MEMBER, T2, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T1),
        "newer disabled-family voice stamp must not replace the allowed message stamp"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Mixed batch, activity disabled: an earlier allowed voice stamp wins over
/// a newer message stamp staged in the same batch.
#[tokio::test]
async fn mixed_batch_newer_message_cannot_replace_allowed_voice_timestamp() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    set_analytics_settings(&db, GUILD, true, false, true)
        .await
        .unwrap();

    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);
    handlers.on_voice_join(voice(MEMBER, T1, false));
    handlers.on_message(message(MEMBER, T2, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T1),
        "newer disabled-family message stamp must not replace the allowed voice stamp"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Cross-batch, voice: a timestamp already recorded while voice was enabled
/// must not move when later voice touches arrive after the family is
/// disabled. The second join and the leave carry no milestone projection, so
/// only the timestamp buffer could move it.
#[tokio::test]
async fn later_disabled_voice_batches_cannot_advance_existing_timestamp() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);

    handlers.on_voice_join(voice(MEMBER, T1, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T1),
        "enabled voice baseline advances last_active_at"
    );

    set_analytics_settings(&db, GUILD, true, true, false)
        .await
        .unwrap();
    handlers.on_voice_leave(voice(MEMBER, T2, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T1),
        "voice leave after disabling voice must not advance last_active_at"
    );

    handlers.on_voice_join(voice(MEMBER, T3, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T1),
        "second voice join after disabling voice must not advance last_active_at"
    );

    // The still-enabled activity family keeps working afterwards.
    handlers.on_message(message(MEMBER, T4, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T4),
        "enabled message family still advances after voice is disabled"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Cross-batch, activity: after the three message rungs are filled, a fourth
/// message's only `last_active_at` path is the timestamp buffer. With the
/// activity family disabled it must not move the existing stamp, while the
/// still-enabled voice family keeps working.
#[tokio::test]
async fn later_disabled_activity_batches_cannot_advance_existing_timestamp() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);

    for at in [T0, T1, T2] {
        handlers.on_message(message(MEMBER, at, false));
        dispatcher.commit(&handlers).await;
    }
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T2),
        "enabled message baseline advances last_active_at"
    );

    set_analytics_settings(&db, GUILD, true, false, true)
        .await
        .unwrap();
    handlers.on_message(message(MEMBER, T3, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T2),
        "post-ladder message after disabling activity must not advance last_active_at"
    );

    handlers.on_voice_join(voice(MEMBER, T4, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T4),
        "enabled voice family still advances after activity is disabled"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Default behavior with no settings row: both families advance, and an
/// older stamp never moves `last_active_at` backwards.
#[tokio::test]
async fn defaults_without_settings_row_still_advance_both_families_monotonically() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);

    handlers.on_message(message(MEMBER, T1, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(last_active_at(&db, MEMBER).await.as_deref(), Some(T1));

    handlers.on_voice_join(voice(MEMBER, T2, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(last_active_at(&db, MEMBER).await.as_deref(), Some(T2));

    handlers.on_message(message(MEMBER, T0, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await.as_deref(),
        Some(T2),
        "an older allowed stamp never moves last_active_at backwards"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Every family disabled: membership lifecycle rows still persist (a disabled
/// analytics category must never blind moderation to a join or a leave), but
/// no family touch sets `last_active_at`.
#[tokio::test]
async fn all_families_disabled_keep_lifecycle_but_never_set_last_active() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    set_analytics_settings(&db, GUILD, false, false, false)
        .await
        .unwrap();

    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);
    handlers.on_join(JoinInput {
        guild_id: guild_u64(),
        member_id: member_u64(MEMBER),
        is_bot: false,
        source: "gateway".to_owned(),
        occurred_at: Some(T0.to_owned()),
        inviter_id: None,
        source_event_id: None,
    });
    handlers.on_message(message(MEMBER, T1, false));
    handlers.on_voice_join(voice(MEMBER, T2, false));
    dispatcher.commit(&handlers).await;

    handlers.on_leave(
        guild_u64(),
        member_u64(MEMBER),
        Some(T3.to_owned()),
        Some(false),
    );
    dispatcher.commit(&handlers).await;

    assert_eq!(
        event_count(&db, MEMBER, "member_join").await,
        1,
        "join lifecycle row persists with every family disabled"
    );
    assert_eq!(
        event_count(&db, MEMBER, "member_leave").await,
        1,
        "leave lifecycle row persists with every family disabled"
    );
    assert_eq!(
        last_active_at(&db, MEMBER).await,
        None,
        "no disabled family touch sets last_active_at"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Suppression still wins over both enabled families: an opted-out member
/// gains no `last_active_at` from message or voice touches.
#[tokio::test]
async fn suppression_still_blocks_activity_when_families_enabled() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    suppress(&db, MEMBER).await;

    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);
    handlers.on_message(message(MEMBER, T1, false));
    handlers.on_voice_join(voice(MEMBER, T2, false));
    dispatcher.commit(&handlers).await;
    assert_eq!(
        last_active_at(&db, MEMBER).await,
        None,
        "suppressed member gains no last_active_at from any family"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// Bot safety classification is retained: a bot is marked as a bot, but its
/// message/voice activity never sets `last_active_at`.
#[tokio::test]
async fn bot_activity_never_sets_last_active() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let handlers = handlers();
    let mut dispatcher = Dispatcher::new(&db);
    handlers.on_join(JoinInput {
        guild_id: guild_u64(),
        member_id: member_u64(BOT_MEMBER),
        is_bot: true,
        source: "gateway".to_owned(),
        occurred_at: Some(T0.to_owned()),
        inviter_id: None,
        source_event_id: None,
    });
    handlers.on_message(message(BOT_MEMBER, T1, true));
    handlers.on_voice_join(voice(BOT_MEMBER, T2, true));
    dispatcher.commit(&handlers).await;

    let is_bot: (bool,) =
        sqlx::query_as("SELECT is_bot FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(BOT_MEMBER)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(
        is_bot.0,
        "bot classification still records the member as a bot"
    );
    assert_eq!(
        last_active_at(&db, BOT_MEMBER).await,
        None,
        "bot activity never sets last_active_at"
    );

    db.close().await;
    test_db.close().await.unwrap();
}
