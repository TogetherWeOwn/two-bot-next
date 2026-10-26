//! Supplemental import-review regressions (test-author role).
//!
//! Dispatched real-handler coverage that pins the complete private review
//! without touching production code or the protected acceptance file:
//!
//! - the review attachment parses as one exact JSON envelope (schema,
//!   guild, count, hash, all six proposed sections with complete
//!   before/after objects, skipped facts kept separate);
//! - two under-byte-cap preview bodies (80-ASCII keys, astral-heavy keys)
//!   stay inside the message budget with the summary first, the full hint
//!   last, and exact shown/hidden counts;
//! - a Confirm under a narrowed inventory re-reviews with the exact newly
//!   skipped IDs and the applicable change intact, writing nothing;
//! - a compare-and-swap loss between the Confirm read and the first write
//!   serves a fresh review (or a truthful over-cap refusal) with matching
//!   hashes, pending state and no overwrite.

#![cfg(test)]

use super::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn attachments_of(response: &InteractionResponse) -> Option<Vec<Attachment>> {
    response
        .data
        .as_ref()
        .and_then(|data| data.attachments.clone())
}

fn attachment_of(response: &InteractionResponse) -> Attachment {
    let attachments = attachments_of(response).expect("preview carries a review attachment");
    assert_eq!(attachments.len(), 1, "exactly one review attachment");
    attachments.into_iter().next().unwrap()
}

fn assert_ephemeral(response: &InteractionResponse) {
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL),
        "response is ephemeral"
    );
}

fn assert_mentions_disabled(response: &InteractionResponse) {
    let mentions = response
        .data
        .as_ref()
        .and_then(|data| data.allowed_mentions.as_ref())
        .expect("preview disables mentions");
    assert!(mentions.parse.is_empty());
    assert!(mentions.users.is_empty());
    assert!(mentions.roles.is_empty());
}

fn assert_no_buttons(response: &InteractionResponse) {
    assert!(
        response
            .data
            .as_ref()
            .and_then(|data| data.components.as_ref())
            .is_none(),
        "no actionable controls: {:?}",
        response_text(response)
    );
}

fn confirm_hash(custom_id: &str) -> String {
    let action = parse_voice_custom_id(custom_id);
    assert!(
        matches!(action, Some(VoiceAction::ImportConfirm { .. })),
        "confirm parses: {custom_id:?}"
    );
    match action {
        Some(VoiceAction::ImportConfirm { hash, .. }) => hash,
        _ => String::new(),
    }
}

fn assert_safe_versioned_filename(filename: &str, guild: u64) {
    assert!(
        filename.contains("diff"),
        "filename names the diff: {filename:?}"
    );
    assert!(
        filename.contains("v1"),
        "filename is versioned: {filename:?}"
    );
    assert!(
        filename.ends_with(".json") || filename.ends_with(".txt"),
        "filename is plain-text or JSON: {filename:?}"
    );
    assert!(
        filename.contains(&guild.to_string()),
        "filename binds the guild: {filename:?}"
    );
    assert!(
        !filename.contains('/') && !filename.contains('\\') && !filename.contains(".."),
        "filename has no path separators: {filename:?}"
    );
    assert!(filename.len() <= 100, "filename is bounded: {filename:?}");
    assert!(
        filename
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "filename is safe chars only: {filename:?}"
    );
}

/// Test-only persistence double that injects one concurrent mutation between
/// the Confirm snapshot read and the first compare-and-swap write: the first
/// `config_apply` installs the scripted `fresh` configuration and answers
/// `Conflict` without writing the candidate, so the handler must take its
/// real CAS-loss branch (fresh snapshot, fresh review) instead of
/// overwriting. Everything else delegates to the shared `Store` double.
struct RaceShared {
    fresh: Mutex<VoiceConfiguration>,
    raced: AtomicBool,
    applies: AtomicUsize,
}

struct RaceStore {
    inner: Store,
    race: Arc<RaceShared>,
}

impl RoomPersistence for RaceStore {
    async fn creators(&self, guild: Snowflake) -> Result<Vec<CreatorChannel>, StoreError> {
        self.inner.creators(guild).await
    }
    async fn rooms(&self, guild: Snowflake) -> Result<Vec<VoiceRoom>, StoreError> {
        self.inner.rooms(guild).await
    }
    async fn add_creator(&self, creator: &CreatorChannel) -> Result<(), StoreError> {
        self.inner.add_creator(creator).await
    }
    async fn creator_for(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> Result<Option<CreatorChannel>, StoreError> {
        self.inner.creator_for(guild, channel).await
    }
    async fn persist(&self, room: &VoiceRoom) -> Result<(), StoreError> {
        self.inner.persist(room).await
    }
    async fn pending_owner_grants(&self, guild: Snowflake) -> Result<Vec<Snowflake>, StoreError> {
        self.inner.pending_owner_grants(guild).await
    }
    async fn prepare_owner_grants(
        &self,
        room: &VoiceRoom,
        previous_owner_id: Snowflake,
    ) -> Result<OwnerGrantIntent, StoreError> {
        self.inner
            .prepare_owner_grants(room, previous_owner_id)
            .await
    }
    async fn update_ownership(&self, room: &VoiceRoom, revision: &str) -> Result<bool, StoreError> {
        self.inner.update_ownership(room, revision).await
    }
    async fn custom_names(&self, guild: Snowflake) -> Result<Vec<(Snowflake, String)>, StoreError> {
        self.inner.custom_names(guild).await
    }
    async fn save_custom_name(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        custom_name: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.inner
            .save_custom_name(guild, channel, custom_name)
            .await
    }
    async fn access_controls(&self, guild: Snowflake) -> Result<AccessControls, StoreError> {
        self.inner.access_controls(guild).await
    }
    async fn save_access_controls(
        &self,
        guild: Snowflake,
        controls: &AccessControls,
    ) -> Result<(), StoreError> {
        self.inner.save_access_controls(guild, controls).await
    }
    async fn logging_settings(&self, guild: Snowflake) -> Result<LoggingSettings, StoreError> {
        self.inner.logging_settings(guild).await
    }
    async fn save_logging_settings(
        &self,
        guild: Snowflake,
        settings: &LoggingSettings,
    ) -> Result<(), StoreError> {
        self.inner.save_logging_settings(guild, settings).await
    }
    async fn config_snapshot(&self, guild: Snowflake) -> Result<VoiceConfiguration, StoreError> {
        self.inner.config_snapshot(guild).await
    }
    async fn config_apply(
        &self,
        guild: Snowflake,
        config: &VoiceConfiguration,
        expected: &VoiceConfiguration,
    ) -> Result<(), StoreError> {
        if !self.race.raced.swap(true, Ordering::SeqCst) {
            *self.inner.config.lock().unwrap() = self.race.fresh.lock().unwrap().clone();
            self.race.applies.fetch_add(1, Ordering::SeqCst);
            return Err(StoreError::Conflict);
        }
        self.race.applies.fetch_add(1, Ordering::SeqCst);
        self.inner.config_apply(guild, config, expected).await
    }
    async fn forget(&self, guild: Snowflake, channel: Snowflake) -> Result<(), StoreError> {
        self.inner.forget(guild, channel).await
    }
    async fn claim_create(
        &self,
        guild: Snowflake,
        user: Snowflake,
        leaving: &[Snowflake],
        config: &CreateAdmissionConfig,
        now_secs: i64,
    ) -> Result<CreateClaim, StoreError> {
        self.inner
            .claim_create(guild, user, leaving, config, now_secs)
            .await
    }
    async fn bind_create_channel(
        &self,
        guild: Snowflake,
        reservation_id: &str,
        channel: Snowflake,
    ) -> Result<(), StoreError> {
        self.inner
            .bind_create_channel(guild, reservation_id, channel)
            .await
    }
    async fn orphaned_create_channels(
        &self,
        guild: Snowflake,
    ) -> Result<Vec<(String, Snowflake)>, StoreError> {
        self.inner.orphaned_create_channels(guild).await
    }
    async fn persist_create(
        &self,
        reservation_id: &str,
        room: &VoiceRoom,
    ) -> Result<(), StoreError> {
        self.inner.persist_create(reservation_id, room).await
    }
    async fn settle_create(&self, reservation_id: &str) -> Result<bool, StoreError> {
        self.inner.settle_create(reservation_id).await
    }
    async fn companions(&self, guild: Snowflake) -> Result<Vec<TextCompanion>, StoreError> {
        self.inner.companions(guild).await
    }
    async fn add_companion(&self, companion: &TextCompanion) -> Result<bool, StoreError> {
        self.inner.add_companion(companion).await
    }
    async fn remove_companion(
        &self,
        guild: Snowflake,
        room: Snowflake,
    ) -> Result<Option<TextCompanion>, StoreError> {
        self.inner.remove_companion(guild, room).await
    }
    async fn record_kick_audit(&self, rows: &[KickAuditRow]) -> Result<(), StoreError> {
        self.inner.record_kick_audit(rows).await
    }
    async fn privacy(
        &self,
        guild: Snowflake,
    ) -> Result<BTreeMap<Snowflake, PrivacyRecord>, StoreError> {
        self.inner.privacy(guild).await
    }
    async fn save_privacy(
        &self,
        guild: Snowflake,
        room: Snowflake,
        record: &PrivacyRecord,
    ) -> Result<bool, StoreError> {
        self.inner.save_privacy(guild, room, record).await
    }
    async fn join_grants(
        &self,
        guild: Snowflake,
    ) -> Result<BTreeMap<Snowflake, BTreeSet<Snowflake>>, StoreError> {
        self.inner.join_grants(guild).await
    }
    async fn save_join_grant(
        &self,
        guild: Snowflake,
        room: Snowflake,
        member: Snowflake,
    ) -> Result<JoinGrantIntent, StoreError> {
        self.inner.save_join_grant(guild, room, member).await
    }
    async fn remove_join_grant(
        &self,
        guild: Snowflake,
        room: Snowflake,
        member: Snowflake,
    ) -> Result<(), StoreError> {
        self.inner.remove_join_grant(guild, room, member).await
    }
}

fn race_harness(
    trace: Trace,
    config: VoiceConfiguration,
    downloads: Vec<Result<Vec<u8>, RoomHttpError>>,
    fresh: VoiceConfiguration,
) -> (
    VoiceRuntime<RaceStore, Http>,
    Arc<Mutex<VoiceConfiguration>>,
    Arc<RaceShared>,
) {
    let shared = Arc::new(Mutex::new(config));
    let queue: Arc<Mutex<VecDeque<Result<Vec<u8>, RoomHttpError>>>> =
        Arc::new(Mutex::new(downloads.into_iter().collect()));
    let race = Arc::new(RaceShared {
        fresh: Mutex::new(fresh),
        raced: AtomicBool::new(false),
        applies: AtomicUsize::new(0),
    });
    let runtime = VoiceRuntime::new(
        {
            let shared = shared.clone();
            let queue = queue.clone();
            let race = race.clone();
            let trace = trace.clone();
            move || {
                let mut store = Store::new(trace.clone());
                store.config = shared.clone();
                store.snapshot_uses_creator_rows = false;
                let mut http = Http::new(trace.clone());
                http.download_results = queue.clone();
                (
                    RaceStore {
                        inner: store,
                        race: race.clone(),
                    },
                    http,
                )
            }
        },
        Duration::from_millis(10),
        true,
    );
    (runtime, shared, race)
}

async fn handle_race_capture(
    runtime: &VoiceRuntime<RaceStore, Http>,
    interaction: &Interaction,
    inventory: Option<&GuildInventory>,
) -> (bool, Option<InteractionResponse>) {
    if runtime.live_actor(GUILD).is_none() {
        assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    }
    let seen = Arc::new(Mutex::new(None::<InteractionResponse>));
    let writer = seen.clone();
    let owned = handle_voice_interaction_with(runtime, interaction, None, inventory, |response| {
        *writer.lock().unwrap() = Some(response);
        async {}
    })
    .await;
    let response = seen.lock().unwrap().clone();
    (owned, response)
}

fn creator_row(channel: &str, limit: u16, private: bool) -> config_codec::CreatorConfiguration {
    config_codec::CreatorConfiguration {
        channel_id: channel.to_owned(),
        name_template: "{game}".to_owned(),
        status_template: None,
        default_limit: limit,
        always_private: private,
        text_channels: false,
        position: config_codec::RoomPosition::Above,
        first_number: 1,
        group_by_category: false,
        permission_source: config_codec::PermissionSource::Creator {},
    }
}

fn template_row(channel: &str, name: &str) -> config_codec::ChannelTemplates {
    config_codec::ChannelTemplates {
        channel_id: channel.to_owned(),
        name_template: name.to_owned(),
        status_template: None,
    }
}

/// The review attachment is one exact JSON envelope: schema, guild, count
/// and hash plus all six proposed sections with complete before/after
/// objects, while skipped input stays in its own section. Escaping and
/// astral values must round-trip byte-identically; unchanged entries must
/// be absent.
#[tokio::test]
async fn review_attachment_carries_exact_schema_and_complete_sections() {
    let trace = Trace::default();
    let mut current = empty_config();
    let before_alias = config_codec::GameAlias {
        game: "changed-game".to_owned(),
        alias: "before-\u{1F680}-tail".to_owned(),
    };
    let removed_alias = config_codec::GameAlias {
        game: "removed-game".to_owned(),
        alias: "removed-value".to_owned(),
    };
    current.creators = vec![creator_row("200", 0, false)];
    current.templates = vec![template_row("600", "Lobby")];
    current.aliases = vec![
        config_codec::GameAlias {
            game: "chess".to_owned(),
            alias: "Chess".to_owned(),
        },
        before_alias.clone(),
        removed_alias.clone(),
    ];
    current.lists = vec![
        config_codec::RandomList {
            name: "maps".to_owned(),
            choices: vec!["a".to_owned(), "b".to_owned()],
        },
        config_codec::RandomList {
            name: "old-list".to_owned(),
            choices: vec!["old-1 \u{1F3AE}".to_owned()],
        },
        config_codec::RandomList {
            name: "edit-list".to_owned(),
            choices: vec!["e1".to_owned()],
        },
    ];
    current.logging = Some(config_codec::LoggingConfiguration {
        channel_id: "500".to_owned(),
        detail: config_codec::LogDetail::Errors,
        mention_member_ids: vec!["300".to_owned()],
        mention_role_ids: vec!["700".to_owned()],
    });

    let added_alias = config_codec::GameAlias {
        game: "added-game".to_owned(),
        alias: "Say \"hi\" \\\\ ok".to_owned(),
    };
    let after_alias = config_codec::GameAlias {
        game: "changed-game".to_owned(),
        alias: "after-\u{1F3AE}-tail-\u{1D4B3}".to_owned(),
    };
    let mut incoming = current.clone();
    incoming.creators = vec![creator_row("200", 5, true), creator_row("999", 0, false)];
    incoming.templates = vec![template_row("600", "Lobby v2")];
    incoming.aliases = vec![
        config_codec::GameAlias {
            game: "chess".to_owned(),
            alias: "Chess".to_owned(),
        },
        after_alias.clone(),
        added_alias.clone(),
    ];
    incoming.lists = vec![
        config_codec::RandomList {
            name: "maps".to_owned(),
            choices: vec!["a".to_owned(), "b".to_owned()],
        },
        config_codec::RandomList {
            name: "new-list".to_owned(),
            choices: vec!["new-1 \u{1F680}".to_owned(), "x".to_owned()],
        },
        config_codec::RandomList {
            name: "edit-list".to_owned(),
            choices: vec!["e1".to_owned(), "e2-\u{1D4B3}".to_owned()],
        },
    ];
    incoming.logging = Some(config_codec::LoggingConfiguration {
        channel_id: "500".to_owned(),
        detail: config_codec::LogDetail::Verbose,
        mention_member_ids: Vec::new(),
        mention_role_ids: vec!["700".to_owned()],
    });
    incoming.settings.unique_names = true;
    incoming.settings.text_channel_name = "lobby-chat".to_owned();

    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), current.clone(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(text.starts_with("Import preview: 10 changes"), "{text:?}");
    assert_ephemeral(&preview);
    assert_mentions_disabled(&preview);
    let (confirm_id, cancel_id) = preview_buttons(&preview);
    let hash = confirm_hash(&confirm_id);
    assert_eq!(hash.len(), two_bot_core::voice_config_diff::DIFF_HASH_CHARS);
    match parse_voice_custom_id(&cancel_id) {
        Some(VoiceAction::ImportCancel {
            hash: cancel_hash, ..
        }) => {
            assert_eq!(cancel_hash, hash);
        }
        other => panic!("cancel parses: {other:?}"),
    }

    let attachment = attachment_of(&preview);
    assert_safe_versioned_filename(&attachment.filename, GUILD);
    assert!(!attachment.file.is_empty());
    assert!(attachment.file.len() <= two_bot_core::voice_config_diff::FULL_REVIEW_BYTE_LIMIT);
    let parsed: serde_json::Value =
        serde_json::from_slice(&attachment.file).expect("artifact parses as JSON");

    let changed_creator = creator_row("200", 5, true);
    let changed_template = template_row("600", "Lobby v2");
    let expected = serde_json::json!({
        "schema": "voice-config-review.v1",
        "guild_id": GUILD.to_string(),
        "hash": hash,
        "change_count": 10,
        "proposed": {
            "creators": {
                "added": [],
                "removed": [],
                "changed": [{
                    "key": "200",
                    "before": serde_json::to_value(creator_row("200", 0, false)).unwrap(),
                    "after": serde_json::to_value(&changed_creator).unwrap(),
                    "fields": ["default_limit", "always_private"],
                }],
            },
            "templates": {
                "added": [],
                "removed": [],
                "changed": [{
                    "key": "600",
                    "before": serde_json::to_value(template_row("600", "Lobby")).unwrap(),
                    "after": serde_json::to_value(&changed_template).unwrap(),
                    "fields": ["name_template"],
                }],
            },
            "aliases": {
                "added": [serde_json::to_value(&added_alias).unwrap()],
                "removed": [serde_json::to_value(&removed_alias).unwrap()],
                "changed": [{
                    "key": "changed-game",
                    "before": serde_json::to_value(&before_alias).unwrap(),
                    "after": serde_json::to_value(&after_alias).unwrap(),
                    "fields": ["alias"],
                }],
            },
            "lists": {
                "added": [serde_json::to_value(&incoming.lists[1]).unwrap()],
                "removed": [serde_json::to_value(&current.lists[1]).unwrap()],
                "changed": [{
                    "key": "edit-list",
                    "before": serde_json::to_value(&current.lists[2]).unwrap(),
                    "after": serde_json::to_value(&incoming.lists[2]).unwrap(),
                    "fields": ["choices"],
                }],
            },
            "logging": {
                "added": [],
                "removed": [],
                "changed": [{
                    "key": "logging",
                    "before": serde_json::to_value(current.logging.as_ref().unwrap()).unwrap(),
                    "after": serde_json::to_value(incoming.logging.as_ref().unwrap()).unwrap(),
                    "fields": ["detail", "mention_member_ids"],
                }],
            },
            "settings": {
                "key": "settings",
                "before": serde_json::to_value(&current.settings).unwrap(),
                "after": serde_json::to_value(&incoming.settings).unwrap(),
                "fields": ["unique_names", "text_channel_name"],
            },
        },
        "skipped_unknown_channels": ["999"],
    });
    assert_eq!(parsed, expected, "review envelope is exact");

    // Unchanged entries stay absent from every section.
    let proposed = parsed.get("proposed").expect("proposed");
    for section in ["aliases", "lists", "creators", "templates", "logging"] {
        let rendered = serde_json::to_string(proposed.get(section).expect("section")).unwrap();
        assert!(
            !rendered.contains("\"chess\""),
            "{section} omits unchanged alias"
        );
        assert!(
            !rendered.contains("\"maps\""),
            "{section} omits unchanged list"
        );
    }
    // No writes before Confirm.
    assert_eq!(*shared.lock().unwrap(), current);
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "config_apply"));
}

/// A settings-only change still renders an exact envelope: every other
/// section empty, the settings before/after complete, count one.
#[tokio::test]
async fn review_settings_only_change_is_exact() {
    let trace = Trace::default();
    let current = empty_config();
    let mut incoming = empty_config();
    incoming.settings.unique_names = true;
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), current.clone(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let (confirm_id, _) = preview_buttons(&preview);
    let hash = confirm_hash(&confirm_id);
    let parsed: serde_json::Value =
        serde_json::from_slice(&attachment_of(&preview).file).expect("artifact parses");
    let empty_section = serde_json::json!({"added": [], "removed": [], "changed": []});
    let expected = serde_json::json!({
        "schema": "voice-config-review.v1",
        "guild_id": GUILD.to_string(),
        "hash": hash,
        "change_count": 1,
        "proposed": {
            "creators": empty_section,
            "templates": empty_section,
            "aliases": empty_section,
            "lists": empty_section,
            "logging": empty_section,
            "settings": {
                "key": "settings",
                "before": serde_json::to_value(&current.settings).unwrap(),
                "after": serde_json::to_value(&incoming.settings).unwrap(),
                "fields": ["unique_names"],
            },
        },
        "skipped_unknown_channels": [],
    });
    assert_eq!(parsed, expected, "settings-only envelope is exact");
    assert_eq!(*shared.lock().unwrap(), current);
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "config_apply"));
}

/// Twenty distinct 80-ASCII game names with alias `A`: no twenty rows of
/// 96 chars fit the message budget, so exactly nineteen show with a `+1
/// more` trailer. The whole body stays in `(1700, 2000]` UTF-16 units with
/// the summary first and the full hint last; the attachment carries all
/// twenty complete aliases.
#[tokio::test]
async fn under_cap_ascii_preview_body_fits_budget_with_hint_and_hidden_count() {
    let trace = Trace::default();
    let mut incoming = empty_config();
    for i in 0..20 {
        incoming.aliases.push(config_codec::GameAlias {
            game: format!("game-{i:02}-{}", "x".repeat(72)),
            alias: "A".to_owned(),
        });
    }
    for alias in &incoming.aliases {
        assert_eq!(alias.game.len(), 80, "witness names are 80 ASCII bytes");
        assert!(alias.game.is_ascii());
    }
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(text.starts_with("Import preview: 20 changes"), "{text:?}");
    let units = text.encode_utf16().count();
    assert!(
        units > 1700 && units <= 2000,
        "body fits the message budget: {units}"
    );
    assert!(
        text.ends_with("Full review attached; read it before confirming."),
        "full hint trails: {text:?}"
    );
    let lines: Vec<&str> = text.lines().collect();
    let trailer = lines[lines.len() - 2];
    assert_eq!(trailer, "+1 more", "{text:?}");
    let entries = &lines[1..lines.len() - 2];
    assert_eq!(entries.len(), 19, "nineteen rows show: {text:?}");
    for entry in entries {
        assert!(
            matches!(entry.chars().next(), Some('+') | Some('-') | Some('~')),
            "{entry:?}"
        );
    }
    let (confirm_id, _) = preview_buttons(&preview);
    let hash = confirm_hash(&confirm_id);

    let attachment = attachment_of(&preview);
    assert!(attachment.file.len() <= two_bot_core::voice_config_diff::FULL_REVIEW_BYTE_LIMIT);
    let body = String::from_utf8(attachment.file).expect("artifact is UTF-8");
    for alias in &incoming.aliases {
        assert!(
            body.contains(&alias.game),
            "artifact keeps witness {}",
            alias.game
        );
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("artifact parses");
    let empty_section = serde_json::json!({"added": [], "removed": [], "changed": []});
    let expected = serde_json::json!({
        "schema": "voice-config-review.v1",
        "guild_id": GUILD.to_string(),
        "hash": hash,
        "change_count": 20,
        "proposed": {
            "creators": empty_section,
            "templates": empty_section,
            "aliases": {
                "added": serde_json::to_value(&incoming.aliases).unwrap(),
                "removed": [],
                "changed": [],
            },
            "lists": empty_section,
            "logging": empty_section,
            "settings": null,
        },
        "skipped_unknown_channels": [],
    });
    assert_eq!(parsed, expected, "attachment carries every complete alias");
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "config_apply"));
}

/// Twenty 80-char astral-heavy game names (two-digit prefix plus 78
/// rockets) with alias `A`: every entry row costs 174 UTF-16 units, so the
/// budget fits exactly ten rows with a `+10 more` trailer. The visible
/// preview must actually contain astral codepoints (units exceed chars);
/// the attachment carries all twenty complete keys.
#[tokio::test]
async fn under_cap_astral_preview_body_fits_budget_with_hint_and_hidden_count() {
    let trace = Trace::default();
    let mut incoming = empty_config();
    for i in 0..20 {
        incoming.aliases.push(config_codec::GameAlias {
            game: format!("{i:02}{}", "\u{1F680}".repeat(78)),
            alias: "A".to_owned(),
        });
    }
    for alias in &incoming.aliases {
        assert_eq!(alias.game.chars().count(), 80, "witness keys are 80 chars");
    }
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(text.starts_with("Import preview: 20 changes"), "{text:?}");
    let units = text.encode_utf16().count();
    assert!(
        units > 1700 && units <= 2000,
        "body fits the message budget: {units}"
    );
    assert!(
        text.chars().count() < units,
        "astral rows cost two units each"
    );
    assert!(
        text.contains("\u{1F680}"),
        "preview shows astral codepoints"
    );
    assert!(
        text.ends_with("Full review attached; read it before confirming."),
        "full hint trails: {text:?}"
    );
    let lines: Vec<&str> = text.lines().collect();
    let trailer = lines[lines.len() - 2];
    assert_eq!(trailer, "+10 more", "{text:?}");
    let entries = &lines[1..lines.len() - 2];
    assert_eq!(entries.len(), 10, "ten rows show: {text:?}");
    for entry in entries {
        assert!(
            matches!(entry.chars().next(), Some('+') | Some('-') | Some('~')),
            "{entry:?}"
        );
    }
    let (confirm_id, _) = preview_buttons(&preview);
    let hash = confirm_hash(&confirm_id);

    let attachment = attachment_of(&preview);
    assert!(attachment.file.len() <= two_bot_core::voice_config_diff::FULL_REVIEW_BYTE_LIMIT);
    let body = String::from_utf8(attachment.file).expect("artifact is UTF-8");
    for alias in &incoming.aliases {
        assert!(
            body.contains(&alias.game),
            "artifact keeps witness {}",
            alias.game
        );
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("artifact parses");
    let empty_section = serde_json::json!({"added": [], "removed": [], "changed": []});
    let expected = serde_json::json!({
        "schema": "voice-config-review.v1",
        "guild_id": GUILD.to_string(),
        "hash": hash,
        "change_count": 20,
        "proposed": {
            "creators": empty_section,
            "templates": empty_section,
            "aliases": {
                "added": serde_json::to_value(&incoming.aliases).unwrap(),
                "removed": [],
                "changed": [],
            },
            "lists": empty_section,
            "logging": empty_section,
            "settings": null,
        },
        "skipped_unknown_channels": [],
    });
    assert_eq!(
        parsed, expected,
        "attachment carries every complete astral key"
    );
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "config_apply"));
}

/// Upload then Confirm while the inventory loses the preexisting creator
/// `101` and template `102`: the fresh review names exactly those newly
/// skipped IDs, keeps the applicable alias addition (and the forced
/// removals), matches pending/buttons/artifact hashes, and writes nothing.
/// A diff that ignores the narrowed inventory would keep `101`/`102` as
/// applicable additions instead of skipped facts.
#[tokio::test]
async fn confirm_reviews_freshly_skipped_channels_without_overwrite() {
    let trace = Trace::default();
    let creator101 = creator_row("101", 0, false);
    let template_before = template_row("102", "Lobby");
    let mut current = empty_config();
    current.creators = vec![creator101.clone()];
    current.templates = vec![template_before.clone()];
    current.aliases = vec![config_codec::GameAlias {
        game: "chess".to_owned(),
        alias: "Chess".to_owned(),
    }];
    let new_alias = config_codec::GameAlias {
        game: "new-game".to_owned(),
        alias: "New Game".to_owned(),
    };
    let mut incoming = current.clone();
    incoming.templates = vec![template_row("102", "Lobby v2")];
    incoming.aliases.push(new_alias.clone());

    let mut preview_inventory = config_inventory();
    for channel in ["101", "102"] {
        preview_inventory.channels.insert(
            channel.to_owned(),
            config_codec::ChannelReference {
                guild_id: GUILD.to_string(),
                kind: config_codec::ChannelKind::Voice,
            },
        );
    }
    let mut confirm_inventory = preview_inventory.clone();
    confirm_inventory.channels.remove("101");
    confirm_inventory.channels.remove("102");

    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), current.clone(), vec![Ok(bytes.clone())]);
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&preview_inventory)).await;
    let preview = preview.expect("preview");
    let (stale_id, _) = preview_buttons(&preview);
    let stale_hash = confirm_hash(&stale_id);

    let confirm = component_interaction(&stale_id, manager(), UPLOADER);
    let (owned, response) =
        handle_import_capture(&runtime, &confirm, Some(&confirm_inventory)).await;
    assert!(owned);
    let fresh = response.expect("fresh preview");
    let text = response_text(&fresh);
    assert!(text.starts_with("Import preview:"), "{text:?}");
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "config_apply"));
    assert_eq!(*shared.lock().unwrap(), current, "no premature write");

    let (fresh_id, fresh_cancel) = preview_buttons(&fresh);
    let fresh_hash = confirm_hash(&fresh_id);
    assert_ne!(fresh_hash, stale_hash);
    assert_eq!(
        fresh_hash.len(),
        two_bot_core::voice_config_diff::DIFF_HASH_CHARS
    );
    match parse_voice_custom_id(&fresh_cancel) {
        Some(VoiceAction::ImportCancel {
            hash: cancel_hash, ..
        }) => {
            assert_eq!(cancel_hash, fresh_hash);
        }
        other => panic!("cancel parses: {other:?}"),
    }
    assert_ephemeral(&fresh);
    assert_mentions_disabled(&fresh);

    let parsed: serde_json::Value =
        serde_json::from_slice(&attachment_of(&fresh).file).expect("fresh artifact parses");
    let empty_section = serde_json::json!({"added": [], "removed": [], "changed": []});
    let expected = serde_json::json!({
        "schema": "voice-config-review.v1",
        "guild_id": GUILD.to_string(),
        "hash": fresh_hash,
        "change_count": 3,
        "proposed": {
            "creators": {
                "added": [],
                "removed": [serde_json::to_value(&creator101).unwrap()],
                "changed": [],
            },
            "templates": {
                "added": [],
                "removed": [serde_json::to_value(&template_before).unwrap()],
                "changed": [],
            },
            "aliases": {
                "added": [serde_json::to_value(&new_alias).unwrap()],
                "removed": [],
                "changed": [],
            },
            "lists": empty_section,
            "logging": empty_section,
            "settings": null,
        },
        "skipped_unknown_channels": ["101", "102"],
    });
    assert_eq!(
        parsed, expected,
        "fresh review pins exact skipped IDs and contents"
    );

    let pending = runtime.pending_imports.lock().unwrap();
    assert!(pending
        .get(&(GUILD, UPLOADER, stale_hash.clone()))
        .is_none());
    let entry = pending
        .get(&(GUILD, UPLOADER, fresh_hash.clone()))
        .expect("fresh pending candidate");
    assert!(
        entry.candidate.creators.is_empty(),
        "pending drops skipped creator"
    );
    assert!(
        entry.candidate.templates.is_empty(),
        "pending drops skipped template"
    );
    assert!(entry.candidate.aliases.contains(&new_alias));
    assert_eq!(
        two_bot_core::voice_config_diff::diff_content_hash(&current, &entry.candidate),
        fresh_hash,
        "pending candidate matches buttons and artifact"
    );
}

/// A scripted persistence double mutates the stored configuration between
/// the Confirm snapshot read and the first compare-and-swap, failing that
/// write with `Conflict`: the handler must take its real CAS branch and
/// serve the exact fresh review (matching pending, buttons, candidate and
/// artifact hashes) without overwriting the fresh state.
#[tokio::test]
async fn confirm_cas_conflict_serves_fresh_review_without_overwrite() {
    let trace = Trace::default();
    let candidate = full_config();
    let mut fresh = full_config();
    fresh.aliases.push(config_codec::GameAlias {
        game: "go".to_owned(),
        alias: "Go".to_owned(),
    });
    let go_alias = fresh.aliases.last().unwrap().clone();
    let bytes = serde_json::to_vec(&candidate).unwrap();
    let (runtime, shared, race) = race_harness(
        trace.clone(),
        empty_config(),
        vec![Ok(bytes.clone())],
        fresh.clone(),
    );
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_race_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let (stale_id, _) = preview_buttons(&preview);
    let stale_hash = confirm_hash(&stale_id);
    assert!(
        !race.raced.load(Ordering::SeqCst),
        "no write raced before Confirm"
    );
    assert_eq!(
        race.applies.load(Ordering::SeqCst),
        0,
        "no CAS ran before Confirm"
    );
    assert_eq!(
        *shared.lock().unwrap(),
        empty_config(),
        "upload writes nothing"
    );

    let confirm = component_interaction(&stale_id, manager(), UPLOADER);
    let (owned, response) = handle_race_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    let fresh_preview = response.expect("fresh preview after CAS loss");
    let text = response_text(&fresh_preview);
    assert!(text.starts_with("Import preview:"), "{text:?}");
    assert!(!text.contains("Could not save the import"), "{text:?}");
    assert_eq!(
        race.applies.load(Ordering::SeqCst),
        1,
        "first CAS ran and lost"
    );
    assert!(
        !trace
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry == "config_apply"),
        "CAS loss writes nothing"
    );
    assert_eq!(
        *shared.lock().unwrap(),
        fresh,
        "fresh state is not overwritten"
    );

    let (fresh_id, fresh_cancel) = preview_buttons(&fresh_preview);
    let fresh_hash = confirm_hash(&fresh_id);
    assert_ne!(fresh_hash, stale_hash);
    match parse_voice_custom_id(&fresh_cancel) {
        Some(VoiceAction::ImportCancel {
            hash: cancel_hash, ..
        }) => {
            assert_eq!(cancel_hash, fresh_hash);
        }
        other => panic!("cancel parses: {other:?}"),
    }
    assert_ephemeral(&fresh_preview);
    assert_mentions_disabled(&fresh_preview);

    let parsed: serde_json::Value =
        serde_json::from_slice(&attachment_of(&fresh_preview).file).expect("parses");
    let empty_section = serde_json::json!({"added": [], "removed": [], "changed": []});
    let expected = serde_json::json!({
        "schema": "voice-config-review.v1",
        "guild_id": GUILD.to_string(),
        "hash": fresh_hash,
        "change_count": 1,
        "proposed": {
            "creators": empty_section,
            "templates": empty_section,
            "aliases": {
                "added": [],
                "removed": [serde_json::to_value(&go_alias).unwrap()],
                "changed": [],
            },
            "lists": empty_section,
            "logging": empty_section,
            "settings": null,
        },
        "skipped_unknown_channels": [],
    });
    assert_eq!(parsed, expected, "CAS fresh review pins full contents");
    assert!(text.contains("go"), "{text:?}");

    let pending = runtime.pending_imports.lock().unwrap();
    assert!(pending
        .get(&(GUILD, UPLOADER, stale_hash.clone()))
        .is_none());
    assert_eq!(pending.len(), 1);
    let entry = pending
        .get(&(GUILD, UPLOADER, fresh_hash.clone()))
        .expect("fresh pending candidate");
    assert_eq!(entry.candidate, candidate);
    assert_eq!(
        two_bot_core::voice_config_diff::diff_content_hash(&fresh, &entry.candidate),
        fresh_hash,
        "pending candidate matches buttons and artifact"
    );
}

/// The same CAS loss, but the fresh state makes the complete review exceed
/// its serialized-byte budget: the Confirm must refuse truthfully with no
/// attachment, no actionable controls, empty pending and no overwrite. (The
/// empty-review mutant is killed by the exact-attachment CAS test above;
/// this case is complementary, pinning refusal cleanup when no fresh review
/// can be built.)
#[tokio::test]
async fn confirm_cas_conflict_over_cap_refuses_without_state() {
    let trace = Trace::default();
    let mut candidate = empty_config();
    for i in 0..3 {
        candidate.aliases.push(config_codec::GameAlias {
            game: format!("game-{i:02}"),
            alias: format!("Alias {i:02}"),
        });
    }
    let mut huge = empty_config();
    for list in 0..20 {
        huge.lists.push(config_codec::RandomList {
            name: format!("list-{list:02}"),
            choices: (0..100)
                .map(|choice| format!("choice-{list:02}-{choice:03}-{}", "x".repeat(80)))
                .collect(),
        });
    }
    let bytes = serde_json::to_vec(&candidate).unwrap();
    let (runtime, shared, race) = race_harness(
        trace.clone(),
        empty_config(),
        vec![Ok(bytes.clone())],
        huge.clone(),
    );
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_race_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("under-cap preview");
    let (confirm_id, _) = preview_buttons(&preview);
    assert!(
        !race.raced.load(Ordering::SeqCst),
        "no write raced before Confirm"
    );
    assert_eq!(
        race.applies.load(Ordering::SeqCst),
        0,
        "no CAS ran before Confirm"
    );
    assert_eq!(
        *shared.lock().unwrap(),
        empty_config(),
        "upload writes nothing"
    );
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (owned, response) = handle_race_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    let refusal = response.expect("over-cap refusal");
    let text = response_text(&refusal);
    let lower = text.to_lowercase();
    assert!(
        lower.contains("reduce") || lower.contains("smaller") || lower.contains("too large"),
        "refusal names the correction: {text:?}"
    );
    assert!(
        lower.contains("retry") || lower.contains("again") || lower.contains("split"),
        "refusal names retry: {text:?}"
    );
    assert!(text.contains("Nothing was changed"), "{text:?}");
    assert_no_buttons(&refusal);
    assert!(
        attachments_of(&refusal).is_none(),
        "over-cap CAS review is empty"
    );
    assert!(response_text(&refusal).len() <= 2000);
    assert_eq!(
        race.applies.load(Ordering::SeqCst),
        1,
        "first CAS ran and lost"
    );
    assert!(
        runtime.pending_imports.lock().unwrap().is_empty(),
        "pending state cleaned on refusal"
    );
    assert_eq!(
        *shared.lock().unwrap(),
        huge,
        "fresh state is not overwritten"
    );
    assert!(!trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry == "config_apply"));
}
