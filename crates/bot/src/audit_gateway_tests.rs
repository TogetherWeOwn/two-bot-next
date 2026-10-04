#![cfg(test)]

//! Gateway audit recording acceptance: fixtures for member
//! role/nick changes, voice join/leave/move, raw message edit/delete and the
//! 14 audit-log action classes produce stored rows matching the core/legacy
//! contracts. No bodies, names or reasons are stored, and a store failure is
//! logged as a scalar while the event continues.
//!
//! Discord doubles only; the database tests use the guarded disposable
//! Postgres (`TWO_TEST_DATABASE_URL`, agent-testdb or CI service) and skip
//! gracefully without it.

use std::str::FromStr as _;

use twilight_model::{
    channel::{
        message::{MessageFlags, MessageType},
        Message,
    },
    gateway::payload::incoming::{
        GuildAuditLogEntryCreate, MemberAdd, MemberUpdate, MessageDelete, VoiceStateUpdate,
    },
    guild::{
        audit_log::{AuditLogEntry, AuditLogEventType, AuditLogOptionalEntryInfo},
        Member, MemberFlags,
    },
    id::Id,
    user::User,
    util::Timestamp,
    voice::VoiceState,
};
use two_bot_core::audit::{AuditChannelIds, AuditEvent, AuditKind};
use two_bot_core::audit_mirror::{MirrorChannel, MirrorError, MirrorMessage, MirrorOverwrite};
use two_bot_core::audit_store::AuditStore;
use two_bot_core::classify::ModerationAuditAction;
use two_bot_discord::build_cache;
use two_bot_testsupport::TestDatabase;

use super::*;
use crate::jobs::ErrorClass;

const GUILD: u64 = 100_000_000_000_000_001;
const MEMBER: u64 = 900_000_000_000_001_111;
const BOT: u64 = 900_000_000_000_000_099;
const CH_TEXT: u64 = 200_000_000_000_000_001;
const CH_VOICE_A: u64 = 300_000_000_000_000_001;
const CH_VOICE_B: u64 = 300_000_000_000_000_002;
const AT: &str = "2026-09-30T00:01:02.003Z";

fn guild_str() -> String {
    GUILD.to_string()
}

fn user(id: u64, bot: bool) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot,
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

fn cached_member(user_id: u64, roles: &[u64], nick: Option<&str>) -> Member {
    Member {
        avatar: None,
        avatar_decoration_data: None,
        banner: None,
        communication_disabled_until: None,
        deaf: false,
        flags: MemberFlags::empty(),
        joined_at: None,
        mute: false,
        nick: nick.map(str::to_owned),
        pending: false,
        premium_since: None,
        roles: roles.iter().map(|role| Id::new(*role)).collect(),
        user: user(user_id, false),
    }
}

fn seed_member(cache: &InMemoryCache, user_id: u64, roles: &[u64], nick: Option<&str>) {
    cache.update(&Event::MemberAdd(Box::new(MemberAdd {
        guild_id: Id::new(GUILD),
        member: cached_member(user_id, roles, nick),
    })));
}

fn member_update(roles: &[u64], nick: Option<&str>) -> Event {
    Event::MemberUpdate(Box::new(MemberUpdate {
        avatar: None,
        communication_disabled_until: None,
        guild_id: Id::new(GUILD),
        flags: None,
        deaf: None,
        joined_at: None,
        mute: None,
        nick: nick.map(str::to_owned),
        pending: false,
        premium_since: None,
        roles: roles.iter().map(|role| Id::new(*role)).collect(),
        user: user(MEMBER, false),
    }))
}

fn voice_event(channel: Option<u64>) -> Event {
    Event::VoiceStateUpdate(Box::new(VoiceStateUpdate(VoiceState {
        channel_id: channel.map(Id::new),
        deaf: false,
        guild_id: Some(Id::new(GUILD)),
        member: Some(cached_member(MEMBER, &[], None)),
        mute: false,
        self_deaf: false,
        self_mute: false,
        self_stream: false,
        self_video: false,
        session_id: "mock-voice".to_owned(),
        suppress: false,
        user_id: Id::new(MEMBER),
        request_to_speak_timestamp: None,
    })))
}

fn message_inner(message_id: u64, edited: Option<&str>) -> Message {
    Message {
        activity: None,
        application: None,
        application_id: None,
        attachments: vec![],
        author: user(MEMBER, false),
        call: None,
        channel_id: Id::new(CH_TEXT),
        components: vec![],
        content: "message bodies never enter audit rows".to_owned(),
        edited_timestamp: edited.map(|stamp| {
            Timestamp::from_str(&format!("2026-09-30T{stamp}.000+00:00")).expect("fixture stamp")
        }),
        embeds: vec![],
        flags: Some(MessageFlags::empty()),
        guild_id: Some(Id::new(GUILD)),
        id: Id::new(message_id),
        #[allow(deprecated)]
        interaction: None,
        interaction_metadata: None,
        kind: MessageType::Regular,
        member: None,
        mention_channels: vec![],
        mention_everyone: false,
        mention_roles: vec![],
        mentions: vec![],
        message_snapshots: vec![],
        pinned: false,
        poll: None,
        reactions: vec![],
        reference: None,
        referenced_message: None,
        role_subscription_data: None,
        sticker_items: vec![],
        timestamp: Timestamp::from_str("2026-09-30T00:01:02.003+00:00").expect("fixture stamp"),
        thread: None,
        tts: false,
        webhook_id: None,
    }
}

fn empty_options() -> AuditLogOptionalEntryInfo {
    AuditLogOptionalEntryInfo {
        auto_moderation_rule_name: None,
        auto_moderation_rule_trigger_type: None,
        channel_id: None,
        count: None,
        delete_member_days: None,
        id: None,
        integration_type: None,
        kind: None,
        members_removed: None,
        message_id: None,
        role_name: None,
    }
}

fn audit_entry(action: AuditLogEventType, log_id: u64) -> Event {
    audit_entry_full(action, log_id, None, empty_options())
}

fn audit_entry_full(
    action: AuditLogEventType,
    log_id: u64,
    reason: Option<String>,
    options: AuditLogOptionalEntryInfo,
) -> Event {
    Event::GuildAuditLogEntryCreate(Box::new(GuildAuditLogEntryCreate(AuditLogEntry {
        action_type: action,
        changes: vec![],
        guild_id: Some(Id::new(GUILD)),
        id: Id::new(log_id),
        options: Some(options),
        reason,
        target_id: Some(Id::new(MEMBER)),
        user_id: Some(Id::new(BOT)),
    })))
}

fn event_type_for(action: ModerationAuditAction) -> AuditLogEventType {
    match action {
        ModerationAuditAction::MemberKick => AuditLogEventType::MemberKick,
        ModerationAuditAction::MemberPrune => AuditLogEventType::MemberPrune,
        ModerationAuditAction::MemberBan => AuditLogEventType::MemberBanAdd,
        ModerationAuditAction::MemberUnban => AuditLogEventType::MemberBanRemove,
        ModerationAuditAction::MemberUpdate => AuditLogEventType::MemberUpdate,
        ModerationAuditAction::MemberRoleUpdate => AuditLogEventType::MemberRoleUpdate,
        ModerationAuditAction::MemberMove => AuditLogEventType::MemberMove,
        ModerationAuditAction::MemberDisconnect => AuditLogEventType::MemberDisconnect,
        ModerationAuditAction::MessageDelete => AuditLogEventType::MessageDelete,
        ModerationAuditAction::MessageBulkDelete => AuditLogEventType::MessageBulkDelete,
        ModerationAuditAction::ChannelUpdate => AuditLogEventType::ChannelUpdate,
        ModerationAuditAction::ChannelOverwriteCreate => AuditLogEventType::ChannelOverwriteCreate,
        ModerationAuditAction::ChannelOverwriteUpdate => AuditLogEventType::ChannelOverwriteUpdate,
        ModerationAuditAction::ChannelOverwriteDelete => AuditLogEventType::ChannelOverwriteDelete,
    }
}

// ----------------------------------------------------------------- translate --

#[test]
fn member_role_and_nick_change_matches_core_contract() {
    let cache = build_cache();
    seed_member(&cache, MEMBER, &[11, 12], None);
    let events = translate_with(
        &member_update(&[12, 13], Some("NewNick")),
        &cache,
        AT,
        7,
        None,
        None,
    );
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.kind, AuditKind::MemberUpdate);
    assert!(
        event
            .entry_id
            .starts_with(&format!("member-update:{GUILD}:{MEMBER}:{AT}:")),
        "hyphenated legacy namespace: {}",
        event.entry_id
    );
    assert_eq!(event.target_id, Some(MEMBER.to_string()));
    assert!(event.metadata_json.contains("\"nicknameChanged\":true"));
    assert!(event.metadata_json.contains("\"addedRoleIds\":[\"13\"]"));
    assert!(event.metadata_json.contains("\"removedRoleIds\":[\"11\"]"));
    assert!(
        !event.metadata_json.contains("NewNick"),
        "display names never enter rows"
    );
}

#[test]
fn member_update_without_baseline_or_change_records_nothing() {
    let cache = build_cache();
    // No cached member: the baseline is untrustworthy, so no row.
    assert!(translate_with(&member_update(&[12], None), &cache, AT, 7, None, None).is_empty());

    // Seeded with the same roles and nick: no delta, no row.
    seed_member(&cache, MEMBER, &[12], None);
    assert!(translate_with(&member_update(&[12], None), &cache, AT, 7, None, None).is_empty());
}

#[test]
fn voice_join_leave_move_match_legacy_keys() {
    // Join: nothing cached, frame carries a channel.
    let cache = build_cache();
    let events = translate_with(&voice_event(Some(CH_VOICE_A)), &cache, AT, 7, None, None);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, AuditKind::VoiceJoin);
    assert_eq!(
        events[0].entry_id,
        format!("voice_join:{GUILD}:{MEMBER}:none:{CH_VOICE_A}:{AT}")
    );
    assert_eq!(
        events[0].destination_channel_id,
        Some(CH_VOICE_A.to_string())
    );

    // Move: seeded on A, frame carries B.
    let cache = build_cache();
    cache.update(&voice_event(Some(CH_VOICE_A)));
    let events = translate_with(&voice_event(Some(CH_VOICE_B)), &cache, AT, 7, None, None);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, AuditKind::VoiceMove);
    assert_eq!(
        events[0].entry_id,
        format!("voice_move:{GUILD}:{MEMBER}:{CH_VOICE_A}:{CH_VOICE_B}:{AT}")
    );

    // Leave: seeded on A, frame carries no channel; the row keys on A.
    let cache = build_cache();
    cache.update(&voice_event(Some(CH_VOICE_A)));
    let events = translate_with(&voice_event(None), &cache, AT, 7, None, None);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, AuditKind::VoiceLeave);
    assert_eq!(
        events[0].entry_id,
        format!("voice_leave:{GUILD}:{MEMBER}:{CH_VOICE_A}:none:{AT}")
    );
    assert_eq!(events[0].source_channel_id, Some(CH_VOICE_A.to_string()));

    // Mute/deafen frame on the same channel: not a boundary, no row.
    let cache = build_cache();
    cache.update(&voice_event(Some(CH_VOICE_A)));
    assert!(translate_with(&voice_event(Some(CH_VOICE_A)), &cache, AT, 7, None, None).is_empty());
}

#[test]
fn raw_message_edit_and_delete_match_core_contracts() {
    let cache = build_cache();
    let message_id = 4_000_000_000_000_000_001;

    let edit = Event::MessageUpdate(Box::new(
        twilight_model::gateway::payload::incoming::MessageUpdate(message_inner(
            message_id,
            Some("00:01:03"),
        )),
    ));
    let events = translate_with(&edit, &cache, AT, 9, None, None);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, AuditKind::MessageEdit);
    assert_eq!(
        events[0].entry_id,
        format!("message-edit:{GUILD}:{message_id}:2026-09-30T00:01:03.000Z")
    );
    assert_eq!(events[0].message_id, Some(message_id.to_string()));
    assert!(
        !events[0]
            .metadata_json
            .contains("message bodies never enter"),
        "bodies never enter rows"
    );

    let delete = Event::MessageDelete(MessageDelete {
        channel_id: Id::new(CH_TEXT),
        guild_id: Some(Id::new(GUILD)),
        id: Id::new(message_id),
    });
    let events = translate_with(&delete, &cache, AT, 9, None, None);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, AuditKind::MessageDelete);
    assert_eq!(
        events[0].entry_id,
        format!("message-delete:{GUILD}:{message_id}")
    );

    // DM delete (no guild): no row.
    let dm = Event::MessageDelete(MessageDelete {
        channel_id: Id::new(CH_TEXT),
        guild_id: None,
        id: Id::new(message_id),
    });
    assert!(translate_with(&dm, &cache, AT, 9, None, None).is_empty());
}

#[test]
fn all_fourteen_audit_log_actions_produce_uncorrelated_rows() {
    let cache = build_cache();
    for (index, action) in ModerationAuditAction::ALL.into_iter().enumerate() {
        let log_id = 800_000_000_000_000_001 + index as u64;
        let events = translate_with(
            &audit_entry(event_type_for(action), log_id),
            &cache,
            AT,
            7,
            None,
            None,
        );
        assert_eq!(events.len(), 1, "{action:?} must produce a row");
        let event = &events[0];
        assert_eq!(event.kind, AuditKind::ModerationAction);
        assert_eq!(event.entry_id, format!("discord-audit:{GUILD}:{log_id}"));
        assert_eq!(event.action.as_deref(), Some(action.as_str()));
        // Snowflake-embedded instant, not the receipt stamp.
        assert_eq!(
            event.occurred_at,
            two_bot_core::format_iso_millis(((log_id >> 22) + 1_420_070_400_000) as i64)
        );
    }
}

/// Marker MAC key from the public, non-production vectors shared with the
/// core MAC acceptance tests; no operational key is embedded in test source.
fn fixture_mac_key() -> String {
    let json: serde_json::Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/moderation-mac.json"
    ))
    .expect("public MAC vector fixture");
    json.as_array().expect("vector list")[0]["secret"]
        .as_str()
        .expect("fixture vector key")
        .to_owned()
}

#[test]
fn correlated_audit_log_row_matches_moderation_service_shape() {
    let cache = build_cache();
    let secret = fixture_mac_key();
    let bot = BOT.to_string();
    let reason = two_bot_core::mac::moderation_audit_reason(
        Some(secret.as_str()),
        &guild_str(),
        "idem-1",
        "moderation.ban",
        &bot,
        "free-form reasons never enter rows",
    );
    let log_id = 800_000_000_000_000_099;
    let events = translate_with(
        &audit_entry_full(
            AuditLogEventType::MemberBanAdd,
            log_id,
            Some(reason),
            empty_options(),
        ),
        &cache,
        AT,
        7,
        Some(secret.as_str()),
        Some(&bot),
    );
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert!(event
        .entry_id
        .starts_with(&format!("moderation-success:{GUILD}:")));
    assert_eq!(event.action.as_deref(), Some("moderation.ban"));
    assert!(event
        .metadata_json
        .contains("\"origin\":\"moderation_service\""));
    assert!(
        !event
            .metadata_json
            .contains("free-form reasons never enter rows"),
        "reasons never enter rows"
    );
}

#[test]
fn audit_log_counts_and_channel_options_follow_legacy() {
    let cache = build_cache();
    let options = AuditLogOptionalEntryInfo {
        channel_id: Some(Id::new(CH_TEXT)),
        count: Some("5".to_owned()),
        ..empty_options()
    };
    let events = translate_with(
        &audit_entry_full(
            AuditLogEventType::MessageDelete,
            800_000_000_000_000_201,
            None,
            options,
        ),
        &cache,
        AT,
        7,
        None,
        None,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].source_channel_id, Some(CH_TEXT.to_string()));
    assert!(events[0].metadata_json.contains("\"count\":5"));

    // Prune reports `members_removed` through the same count slot.
    let options = AuditLogOptionalEntryInfo {
        members_removed: Some("7".to_owned()),
        ..empty_options()
    };
    let events = translate_with(
        &audit_entry_full(
            AuditLogEventType::MemberPrune,
            800_000_000_000_000_202,
            None,
            options,
        ),
        &cache,
        AT,
        7,
        None,
        None,
    );
    assert!(events[0].metadata_json.contains("\"count\":7"));
}

#[test]
fn unrelated_dispatches_record_nothing() {
    let cache = build_cache();
    assert!(translate_with(&Event::Resumed, &cache, AT, 7, None, None).is_empty());
}

// ------------------------------------------------------------------- record --

/// Trivial mirror: recording never delivers, so these bodies never run.
#[derive(Clone, Default)]
struct NoopMirror;

impl two_bot_core::audit_mirror::AuditMirror for NoopMirror {
    async fn post_mirror_checked<Fut, E>(
        &self,
        _channel_id: &str,
        _content: &str,
        _nonce: &str,
        authorize: Fut,
    ) -> Result<Result<String, MirrorError>, E>
    where
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: Send,
    {
        authorize.await?;
        Ok(Ok("900000".to_owned()))
    }

    async fn channel_document(&self, _channel_id: &str) -> Result<MirrorChannel, MirrorError> {
        Ok(MirrorChannel {
            guild_id: guild_str(),
            everyone: Some(MirrorOverwrite {
                allow: "0".to_owned(),
                deny: "1024".to_owned(),
            }),
        })
    }

    async fn channel_history(
        &self,
        _channel_id: &str,
        _before: Option<&str>,
        _limit: u8,
    ) -> Result<Vec<MirrorMessage>, MirrorError> {
        Ok(vec![])
    }
}

fn channels() -> AuditChannelIds {
    AuditChannelIds {
        audit: Some("1111".to_owned()),
        voice: Some("2222".to_owned()),
        moderation: Some("3333".to_owned()),
    }
}

async fn database(test: &str) -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

/// Every translated fixture stores through the shared runtime and reads back.
#[tokio::test]
async fn translated_fixtures_produce_stored_rows() {
    let Some(db) = database("translated_fixtures_produce_stored_rows").await else {
        return;
    };
    let pool = db.pool().clone();
    let runtime = std::sync::Arc::new(AuditRuntime::new(
        channels(),
        guild_str(),
        Box::new(move || {
            let pool = pool.clone();
            Box::pin(async move {
                Ok(crate::audit_runtime::Parts {
                    pool: pool.clone(),
                    mirror: NoopMirror,
                    bot_user_id: BOT.to_string(),
                })
            })
        }),
    ));

    let cache = build_cache();
    seed_member(&cache, MEMBER, &[11, 12], None);
    cache.update(&voice_event(None));

    let message_id = 4_000_000_000_000_000_011;
    let fixtures: Vec<Event> = vec![
        member_update(&[12, 13], Some("NewNick")),
        voice_event(Some(CH_VOICE_A)),
        Event::MessageUpdate(Box::new(
            twilight_model::gateway::payload::incoming::MessageUpdate(message_inner(
                message_id,
                Some("00:01:03"),
            )),
        )),
        Event::MessageDelete(MessageDelete {
            channel_id: Id::new(CH_TEXT),
            guild_id: Some(Id::new(GUILD)),
            id: Id::new(message_id),
        }),
    ];
    let mut events: Vec<AuditEvent> = fixtures
        .iter()
        .flat_map(|event| translate_with(event, &cache, AT, 9, None, None))
        .collect();
    for (index, action) in ModerationAuditAction::ALL.into_iter().enumerate() {
        events.extend(translate_with(
            &audit_entry(
                event_type_for(action),
                800_000_000_000_000_301 + index as u64,
            ),
            &cache,
            AT,
            9,
            None,
            None,
        ));
    }
    // 1 member + 1 voice + 1 edit + 1 delete + 14 audit-log rows.
    assert_eq!(events.len(), 18);

    record_into(&runtime, &events).await;

    let store = AuditStore::new(db.pool());
    for event in &events {
        let stored = store
            .get(&event.entry_id)
            .await
            .unwrap_or_else(|_| panic!("stored row for {}", event.entry_id));
        assert!(stored.is_some(), "missing stored row {}", event.entry_id);
    }
}

/// A failed store write is a scalar log while the event continues: the call
/// returns normally instead of propagating the failure.
#[tokio::test]
async fn store_failure_is_logged_as_a_scalar_while_the_event_continues() {
    let runtime: AuditRuntime<NoopMirror> = AuditRuntime::new(
        channels(),
        guild_str(),
        Box::new(|| Box::pin(async { Err(ErrorClass::Database) })),
    );
    let event = AuditEvent::new(
        "member-update:1:2:2026-09-30T00:01:02.003Z:fixture".to_owned(),
        AuditKind::MemberUpdate,
        guild_str(),
        AT.to_owned(),
    );
    assert!(runtime.record(&event).await.is_err());
    // The gateway path swallows that failure and continues with the event.
    record_into(&runtime, &[event]).await;
}

/// Without a configured runtime the gateway path is a silent no-op.
#[tokio::test]
async fn record_without_a_handle_continues_silently() {
    if handle().is_some() {
        eprintln!("SKIP record_without_a_handle_continues_silently: handle installed");
        return;
    }
    record_all(&[AuditEvent::new(
        "voice_join:1:2:none:3:at".to_owned(),
        AuditKind::VoiceJoin,
        guild_str(),
        AT.to_owned(),
    )])
    .await;
}
