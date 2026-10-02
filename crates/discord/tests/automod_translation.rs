use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use twilight_model::{
    channel::Message,
    gateway::{
        event::Event,
        payload::incoming::{MessageCreate, MessageUpdate},
    },
    id::Id,
};
use two_bot_core::automod_runtime::{
    AutomodRuntime, AutomodScope, DeliveryKey, FunnelDisposition, Inspection, MessageDeliveryKind,
    STAGING_GUILD_ID,
};
use two_bot_core::{
    AutomodConfig, AutomodFilter, FactsSink, LevelOutcome, LevelingHook, MemStore, MemberJoinFact,
    MessageFact, RulesAcceptedFact, VoiceEndedFact, VoiceStartedFact,
};
use two_bot_discord::automod::{event_to_automod, with_fetched_message};
use two_bot_discord::{NoClassification, NoInvites, Pipeline};

#[derive(Clone, Default)]
struct MessageCalls(Arc<AtomicUsize>);

impl FactsSink for MessageCalls {
    fn record_member_join(&self, _: MemberJoinFact<'_>) {}
    fn record_rules_accepted(&self, _: RulesAcceptedFact<'_>) {}
    fn record_message(&self, _: MessageFact<'_>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn record_voice_started(&self, _: VoiceStartedFact<'_>) -> Option<String> {
        None
    }
    fn record_voice_ended(&self, _: VoiceEndedFact<'_>) {}
}

impl LevelingHook for MessageCalls {
    fn award_message(&self, _: u64, _: u64, _: &str, _: u64) -> LevelOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }
    fn award_voice(&self, _: u64, _: u64, _: u64, _: &str, _: u64) -> LevelOutcome {
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }
}

#[test]
fn shared_pipeline_captures_rejection_without_awards_and_skips_replay_and_edit() {
    let facts = MessageCalls::default();
    let xp = MessageCalls::default();
    let store = MemStore::new();
    let pipeline = Pipeline::new(
        store,
        Some(xp.clone()),
        Some(facts.clone()),
        NoInvites,
        NoClassification,
    );
    let store = pipeline.handlers().store();
    let mut runtime = runtime();
    let mut msg = message();
    msg.content = "blocked".into();
    let event = Event::MessageCreate(Box::new(MessageCreate(msg.clone())));
    let delivery = event_to_automod(&event, 1_790_726_400_000).unwrap();
    let Inspection::Matched(matched) = runtime.inspect(&delivery) else {
        panic!("expected match")
    };
    // Main's replay clock and automod's capture-only seam share one dispatch.
    pipeline.handle_at_with_message_disposition(&event, "2026-09-30T00:00:01.000Z", matched.funnel);
    let guild = msg.guild_id.unwrap().get();
    let author = msg.author.id.get();
    assert_eq!(facts.0.load(Ordering::SeqCst), 1);
    assert_eq!(xp.0.load(Ordering::SeqCst), 0);
    assert!(store.rows().is_empty());
    assert!(store.activity(guild, author).is_none());
    // InFlight/Replayed claims are cache-only; do not inspect or dispatch twice.
    pipeline.handle_with_message_disposition(&event, FunnelDisposition::None);
    assert_eq!(facts.0.load(Ordering::SeqCst), 1);
    // A different, clean create follows the ordinary funnel exactly once.
    msg.id = Id::new(msg.id.get() + 1);
    msg.content = "clean".into();
    let event = Event::MessageCreate(Box::new(MessageCreate(msg.clone())));
    let delivery = event_to_automod(&event, 1_790_726_401_000).unwrap();
    let Inspection::Accepted(disposition) = runtime.inspect(&delivery) else {
        panic!("expected accept")
    };
    pipeline.handle_with_message_disposition(&event, disposition);
    assert_eq!(facts.0.load(Ordering::SeqCst), 2);
    assert_eq!(xp.0.load(Ordering::SeqCst), 1);
    assert_eq!(store.rows().len(), 1);
    assert!(store.activity(guild, author).is_some());
    pipeline.handle_with_message_disposition(&event, FunnelDisposition::None);
    // Even an incorrectly supplied Accept on an edit cannot enter on_message.
    pipeline.handle_with_message_disposition(
        &Event::MessageUpdate(Box::new(MessageUpdate(msg))),
        FunnelDisposition::Accept,
    );
    assert_eq!(facts.0.load(Ordering::SeqCst), 2);
    assert_eq!(xp.0.load(Ordering::SeqCst), 1);
    assert_eq!(store.rows().len(), 1);
}

fn message() -> Message {
    serde_json::from_value(serde_json::json!({
        "id": "333333333333333333", "channel_id": "222222222222222222",
        "guild_id": STAGING_GUILD_ID, "type": 0, "content": "clean",
        "author": {"id": "444444444444444444", "username": "mock", "discriminator": "0", "avatar": null, "bot": false},
        "member": {"roles": ["555555555555555555"], "joined_at": null, "deaf": false, "mute": false, "flags": 0},
        "timestamp": "2026-09-30T00:00:00.000000+00:00", "edited_timestamp": null,
        "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "tts": false
    })).unwrap()
}

fn runtime() -> AutomodRuntime {
    let vars = HashMap::from([
        ("TWO_AUTOMOD".into(), "1".into()),
        ("TWO_AUTOMOD_BAD_WORDS".into(), "blocked".into()),
    ]);
    AutomodRuntime::new(
        AutomodConfig::from_map(&vars).unwrap(),
        AutomodScope {
            guild_id: STAGING_GUILD_ID.into(),
            live_approved: false,
        },
    )
}

#[test]
fn create_keeps_content_role_mention_attachment_and_message_time_facts() {
    let mut msg = message();
    msg.content = "blocked".into();
    let event = Event::MessageCreate(Box::new(MessageCreate(msg)));
    let delivery = event_to_automod(&event, 9_000_000).unwrap();
    assert_eq!(delivery.kind, MessageDeliveryKind::Create);
    let snapshot = delivery.snapshot.as_ref().unwrap();
    assert_eq!(snapshot.content, "blocked");
    assert_eq!(snapshot.role_ids, vec!["555555555555555555"]);
    assert_eq!(snapshot.observed_timestamp_ms, 1_790_726_400_000);
    assert!(
        matches!(runtime().inspect(&delivery), Inspection::Matched(m) if m.funnel == FunnelDisposition::CaptureOnly)
    );
}

#[test]
fn update_requests_fetch_then_reinspects_at_receipt_time_without_funnel() {
    let mut msg = message();
    msg.content = "blocked".into();
    let event = Event::MessageUpdate(Box::new(MessageUpdate(msg.clone())));
    let delivery = event_to_automod(&event, 1_790_726_401_000).unwrap();
    assert!(delivery.snapshot.is_none());
    assert!(DeliveryKey::from_delivery(&delivery, true).is_none());
    assert!(matches!(
        runtime().inspect(&delivery),
        Inspection::FetchMessage { .. }
    ));
    // REST has no guild/member: identity comes from the exact fetch request and
    // the authoritative member resolver, not missing-field defaults.
    msg.guild_id = None;
    msg.member = None;
    let complete = with_fetched_message(&delivery, &msg, &["555555555555555555".into()]).unwrap();
    assert_eq!(
        complete.snapshot.as_ref().unwrap().observed_timestamp_ms,
        delivery.observed_timestamp_ms
    );
    assert!(
        matches!(runtime().inspect(&complete), Inspection::Matched(m) if m.funnel == FunnelDisposition::None)
    );
    msg.id = Id::new(999);
    assert!(with_fetched_message(&delivery, &msg, &[]).is_none());
}

#[test]
fn missing_member_roles_requests_fetch_instead_of_bypassing_protection() {
    let mut msg = message();
    msg.member = None;
    let event = Event::MessageCreate(Box::new(MessageCreate(msg)));
    let delivery = event_to_automod(&event, 1_790_726_401_000).unwrap();
    assert!(delivery.snapshot.is_none());
    assert!(matches!(
        runtime().inspect(&delivery),
        Inspection::FetchMessage { .. }
    ));
}

#[test]
fn mock_attachment_is_blocked_and_reply_without_ping_is_not_mention_spam() {
    let mut value = serde_json::to_value(message()).unwrap();
    value["attachments"] = serde_json::json!([{
        "id": "888888888888888888", "filename": "Setup.EXE", "size": 1,
        "url": "http://mock.invalid/file", "proxy_url": "http://mock.invalid/file"
    }]);
    value["message_reference"] = serde_json::json!({"message_id": "777777777777777777"});
    let msg: Message = serde_json::from_value(value).unwrap();
    let delivery = event_to_automod(
        &Event::MessageCreate(Box::new(MessageCreate(msg))),
        1_790_726_401_000,
    )
    .unwrap();
    assert!(delivery
        .snapshot
        .as_ref()
        .unwrap()
        .mentioned_user_ids
        .is_empty());
    assert!(
        matches!(runtime().inspect(&delivery), Inspection::Matched(m) if m.filter == AutomodFilter::AttachmentType)
    );
}

#[test]
fn role_enrichment_must_not_replace_blocked_create_with_edited_content() {
    let mut original = message();
    original.content = "blocked".into();
    original.member = None;
    let mut mention = serde_json::to_value(&original.author).unwrap();
    mention["public_flags"] = serde_json::json!(0);
    original.mentions = vec![serde_json::from_value(mention).unwrap()];
    original.attachments = serde_json::from_value(serde_json::json!([{
        "id": "888888888888888888", "filename": "Setup.EXE", "size": 1,
        "url": "http://mock.invalid/file", "proxy_url": "http://mock.invalid/file"
    }]))
    .unwrap();
    let event = Event::MessageCreate(Box::new(MessageCreate(original.clone())));
    let delivery = event_to_automod(&event, 1_790_726_401_000).unwrap();
    assert!(matches!(
        runtime().inspect(&delivery),
        Inspection::FetchMessage { .. }
    ));
    assert!(DeliveryKey::from_delivery(&delivery, true).is_none());
    let mut fetched = original.clone();
    fetched.content = "clean".into();
    fetched.mentions.clear();
    fetched.attachments.clear();
    fetched.timestamp =
        twilight_model::util::Timestamp::parse("2026-09-30T00:00:03.000000+00:00").unwrap();
    fetched.edited_timestamp =
        Some(twilight_model::util::Timestamp::parse("2026-09-30T00:00:02.000000+00:00").unwrap());
    let complete =
        with_fetched_message(&delivery, &fetched, &["555555555555555555".into()]).unwrap();
    let facts = complete.snapshot.as_ref().unwrap();
    assert_eq!(facts.content, original.content);
    assert_eq!(
        facts.mentioned_user_ids,
        vec![original.author.id.to_string()]
    );
    assert_eq!(facts.attachment_names, vec!["Setup.EXE"]);
    assert_eq!(facts.role_ids, vec!["555555555555555555"]);
    assert_eq!(facts.observed_timestamp_ms, 1_790_726_400_000);
    assert_eq!(complete.edited_timestamp_ms, delivery.edited_timestamp_ms);
    assert!(
        matches!(runtime().inspect(&complete), Inspection::Matched(m)
        if m.filter == AutomodFilter::BadWords && m.funnel == FunnelDisposition::CaptureOnly)
    );
    fetched.author.id = Id::new(999);
    assert!(with_fetched_message(&delivery, &fetched, &[]).is_none());
}

#[test]
fn original_create_with_roles_is_capture_only_control() {
    let mut original = message();
    original.content = "blocked".into();
    let delivery = event_to_automod(
        &Event::MessageCreate(Box::new(MessageCreate(original))),
        1_790_726_401_000,
    )
    .unwrap();
    assert!(
        matches!(runtime().inspect(&delivery), Inspection::Matched(m)
        if m.filter == AutomodFilter::BadWords && m.funnel == FunnelDisposition::CaptureOnly)
    );
}

#[test]
fn raw_partial_edit_reaches_fetch_and_enrichment_without_full_decode() {
    use two_bot_discord::automod::{partial_edit_delivery, PartialEdit};
    // Minimal Discord MESSAGE_UPDATE: IDs + changed content, no author,
    // attachments or timestamps. Full-message decoding fails before any
    // enrichment seam could run.
    let payload = serde_json::json!({
        "op": 0, "s": 1, "t": "MESSAGE_UPDATE", "d": {
            "id": "333333333333333333", "channel_id": "222222222222222222",
            "guild_id": STAGING_GUILD_ID, "content": "blocked",
            "edited_timestamp": "2026-09-30T00:00:01.000000+00:00"
        }
    })
    .to_string();
    assert!(
        twilight_gateway::parse(payload.clone(), twilight_gateway::EventTypeFlags::all()).is_err(),
        "minimal edit unexpectedly survives full-message decoding"
    );
    let dispatch: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let edit = PartialEdit::from_dispatch(&dispatch["d"]).unwrap();
    assert_eq!(edit.message_id, "333333333333333333");
    assert_eq!(edit.guild_id.as_deref(), Some(STAGING_GUILD_ID));
    let delivery = partial_edit_delivery(&edit, 1_790_726_401_000);
    assert_eq!(delivery.kind, MessageDeliveryKind::Update);
    assert!(delivery.snapshot.is_none());
    assert!(delivery.edited_timestamp_ms.is_some());
    assert!(matches!(
        runtime().inspect(&delivery),
        Inspection::FetchMessage { .. }
    ));
    // Missing IDs reject the dispatch instead of defaulting to empty strings.
    assert!(PartialEdit::from_dispatch(&serde_json::json!({"channel_id": "1"})).is_none());
    assert!(PartialEdit::from_dispatch(&serde_json::json!({"id": "1"})).is_none());
    // Authoritative enrichment completes the same delivery like any fetch.
    let mut msg = message();
    msg.content = "blocked".into();
    let complete = with_fetched_message(&delivery, &msg, &["555555555555555555".into()]).unwrap();
    assert!(
        matches!(runtime().inspect(&complete), Inspection::Matched(m) if m.funnel == FunnelDisposition::None)
    );
}
