//! Operator-side V11 voice configuration apply (`voice-config-apply`).
//!
//! The same decisions as the bot's `/import` preview and Confirm
//! (`plan_import_preview` / `plan_import_confirm` in `crates/bot`), for an
//! Operator who holds the database and bot credentials rather than a Discord
//! admin session: strict-decode the V11 document, report and skip entries on
//! channels the guild does not have, revalidate the remainder against a
//! trusted inventory read from Discord (never from the uploaded file), and
//! diff it against the stored configuration. Nothing here does I/O; the
//! binary supplies the inventory, the stored snapshot and the file bytes, and
//! writes only with `--apply` through `PgVoiceConfigStore::apply`, whose
//! compare-and-swap refuses if the guild changed after the snapshot.

use twilight_model::channel::{Channel, ChannelType};
use two_bot_core::voice_config::{
    decode_configuration, validate_configuration, ChannelKind, ChannelReference, GuildInventory,
    VoiceConfigError, VoiceConfiguration, MAX_IMPORT_BYTES,
};
use two_bot_core::voice_config_diff::{
    diff_configuration, diff_content_hash, render_preview, skip_unknown_channels,
};

/// Preview lines printed by the CLI. A terminal has no 2000-character
/// message limit, but the renderer still caps the body; this bounds lines.
pub const MAX_PREVIEW_LINES: usize = 200;

/// One planning outcome. Only `Changes` carries a candidate to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyPlan {
    /// Refused before any write: oversized or malformed file, or a candidate
    /// that fails revalidation (cross-guild, wrong-kind channel, bad template).
    Refuse { message: String },
    /// The stored configuration already equals the file (minus skipped
    /// unknown channels). Nothing to write.
    NoChanges {
        text: String,
        skipped_unknown_channels: Vec<String>,
    },
    /// Write `candidate` with `current` as the compare-and-swap expectation.
    Changes {
        candidate: VoiceConfiguration,
        hash: String,
        change_count: usize,
        text: String,
        skipped_unknown_channels: Vec<String>,
    },
}

/// Plan an apply of `bytes` over `current`, mirroring `/import`: the size cap
/// first, then the strict codec decode (never plain `serde_json`), then
/// unknown-channel skipping and revalidation of what remains. The diff runs
/// on the full upload so skipped channels are still reported.
#[must_use]
pub fn plan_apply(
    current: &VoiceConfiguration,
    bytes: &[u8],
    inventory: &GuildInventory,
) -> ApplyPlan {
    if bytes.len() > MAX_IMPORT_BYTES {
        return ApplyPlan::Refuse {
            message: format!(
                "file is too large ({} bytes; the limit is {MAX_IMPORT_BYTES} bytes); nothing was changed",
                bytes.len()
            ),
        };
    }
    let incoming = match decode_configuration(bytes) {
        Ok(config) => config,
        Err(VoiceConfigError::Malformed { line, column }) => {
            return ApplyPlan::Refuse {
                message: format!(
                    "malformed configuration JSON at line {line}, column {column}; nothing was changed"
                ),
            };
        }
        Err(error) => {
            return ApplyPlan::Refuse {
                message: format!("{error}; nothing was changed"),
            };
        }
    };
    let (remaining, skipped) = skip_unknown_channels(&incoming, inventory);
    if let Err(error) = validate_configuration(&remaining, inventory) {
        return ApplyPlan::Refuse {
            message: format!("{error} nothing was changed"),
        };
    }
    let diff = diff_configuration(current, &incoming, inventory);
    let text = render_preview(&diff, MAX_PREVIEW_LINES);
    let change_count = diff.change_count();
    if change_count == 0 {
        return ApplyPlan::NoChanges {
            text,
            skipped_unknown_channels: skipped,
        };
    }
    ApplyPlan::Changes {
        hash: diff_content_hash(current, &remaining),
        candidate: remaining,
        change_count,
        text,
        skipped_unknown_channels: skipped,
    }
}

/// Trusted inventory from Discord REST reads, built like the bot's
/// `inventory_from_cache`: text, voice, stage and category channels of this
/// guild only (a channel reporting another guild is left out), plus the
/// guild's role and member IDs. Uploaded data never contributes.
#[must_use]
pub fn inventory_from_rest(
    guild_id: u64,
    channels: &[Channel],
    role_ids: impl IntoIterator<Item = u64>,
    member_ids: impl IntoIterator<Item = u64>,
) -> GuildInventory {
    let guild = guild_id.to_string();
    let channels = channels
        .iter()
        .filter(|channel| channel.guild_id.map(|id| id.get()) == Some(guild_id))
        .filter_map(|channel| {
            let kind = match channel.kind {
                ChannelType::GuildText => ChannelKind::Text,
                ChannelType::GuildVoice => ChannelKind::Voice,
                ChannelType::GuildStageVoice => ChannelKind::Stage,
                ChannelType::GuildCategory => ChannelKind::Category,
                _ => return None,
            };
            Some((
                channel.id.get().to_string(),
                ChannelReference {
                    guild_id: guild.clone(),
                    kind,
                },
            ))
        })
        .collect();
    let roles = role_ids
        .into_iter()
        .map(|id| (id.to_string(), guild.clone()))
        .collect();
    let members = member_ids
        .into_iter()
        .map(|id| (id.to_string(), guild.clone()))
        .collect();
    GuildInventory {
        guild_id: guild,
        channels,
        roles,
        members,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::voice_config::export_configuration;

    const GUILD: u64 = 1_545_644_954_272_137_297;
    const VOICE: u64 = 1_546_777_867_978_018_887;
    const CATEGORY: u64 = 1_546_777_865_000_000_001;
    const TEXT: u64 = 1_546_777_865_000_000_002;
    const OTHER_GUILD_VOICE: u64 = 1_546_777_865_000_000_003;

    fn channel(id: u64, guild: u64, kind: u8) -> Channel {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(),
            "guild_id": guild.to_string(),
            "type": kind,
            "name": format!("c{id}"),
            "position": 0,
            "permission_overwrites": [],
        }))
        .expect("channel fixture")
    }

    fn inventory() -> GuildInventory {
        inventory_from_rest(
            GUILD,
            &[
                channel(VOICE, GUILD, 2),
                channel(CATEGORY, GUILD, 4),
                channel(TEXT, GUILD, 0),
                channel(OTHER_GUILD_VOICE, GUILD + 1, 2),
            ],
            [GUILD],
            [42],
        )
    }

    /// The stored defaults for a never-configured guild, through the codec.
    fn empty_config() -> VoiceConfiguration {
        decode_configuration(&empty_document(GUILD)).expect("default document decodes")
    }

    fn empty_document(guild: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "guild_id": guild.to_string(),
            "creators": [],
            "templates": [],
            "aliases": [],
            "lists": [],
            "logging": null,
            "settings": {
                "creation_enabled": true,
                "unique_names": false,
                "no_game_label": "General",
                "force_single_game": false,
                "count_members_without_activity": false,
                "time_zone": "UTC",
                "text_channel_name": "voice-chat",
                "text_viewer_role_id": null,
                "command_role_id": null,
                "command_roles": []
            }
        }))
        .expect("document")
    }

    fn with_creator(channel_id: u64, name_template: &str) -> Vec<u8> {
        let mut document: serde_json::Value =
            serde_json::from_slice(&empty_document(GUILD)).expect("json");
        document["creators"] = serde_json::json!([{
            "channel_id": channel_id.to_string(),
            "name_template": name_template,
            "status_template": null,
            "default_limit": 0,
            "always_private": false,
            "text_channels": false,
            "position": "above",
            "first_number": 1,
            "group_by_category": false,
            "permission_source": {"kind": "creator"}
        }]);
        serde_json::to_vec(&document).expect("json")
    }

    #[test]
    fn inventory_keeps_only_this_guilds_supported_channels() {
        let inventory = inventory();
        assert_eq!(inventory.guild_id, GUILD.to_string());
        assert_eq!(
            inventory.channels.get(&VOICE.to_string()).map(|c| c.kind),
            Some(ChannelKind::Voice)
        );
        assert_eq!(
            inventory
                .channels
                .get(&CATEGORY.to_string())
                .map(|c| c.kind),
            Some(ChannelKind::Category)
        );
        assert!(!inventory
            .channels
            .contains_key(&OTHER_GUILD_VOICE.to_string()));
        assert!(inventory.roles.contains_key(&GUILD.to_string()));
        assert!(inventory.members.contains_key("42"));
    }

    #[test]
    fn oversized_file_is_refused_before_decoding() {
        let bytes = vec![b' '; MAX_IMPORT_BYTES + 1];
        assert!(matches!(
            plan_apply(&empty_config(), &bytes, &inventory()),
            ApplyPlan::Refuse { message } if message.contains("too large")
        ));
    }

    #[test]
    fn malformed_json_is_refused_with_position_only() {
        let plan = plan_apply(&empty_config(), b"{\"version\": 1,", &inventory());
        assert!(matches!(
            plan,
            ApplyPlan::Refuse { message } if message.starts_with("malformed configuration JSON at line")
        ));
    }

    #[test]
    fn positional_array_document_is_refused() {
        let plan = plan_apply(&empty_config(), b"[1, \"x\"]", &inventory());
        assert!(matches!(plan, ApplyPlan::Refuse { .. }));
    }

    #[test]
    fn document_for_another_guild_is_refused() {
        let plan = plan_apply(&empty_config(), &empty_document(GUILD + 1), &inventory());
        assert!(matches!(plan, ApplyPlan::Refuse { .. }), "{plan:?}");
    }

    #[test]
    fn creator_on_a_text_channel_is_refused() {
        let plan = plan_apply(&empty_config(), &with_creator(TEXT, "Room"), &inventory());
        assert!(matches!(plan, ApplyPlan::Refuse { .. }), "{plan:?}");
    }

    #[test]
    fn unknown_creator_channel_is_skipped_and_reported_never_applied() {
        let unknown = OTHER_GUILD_VOICE;
        let plan = plan_apply(
            &empty_config(),
            &with_creator(unknown, "Room"),
            &inventory(),
        );
        match plan {
            ApplyPlan::NoChanges {
                skipped_unknown_channels,
                text,
            } => {
                assert_eq!(skipped_unknown_channels, vec![unknown.to_string()]);
                assert!(text.contains("skipped unknown channel"), "{text}");
            }
            other => panic!("expected NoChanges, got {other:?}"),
        }
    }

    #[test]
    fn identical_document_is_no_changes() {
        let plan = plan_apply(&empty_config(), &empty_document(GUILD), &inventory());
        assert!(
            matches!(plan, ApplyPlan::NoChanges { ref skipped_unknown_channels, .. } if skipped_unknown_channels.is_empty())
        );
    }

    #[test]
    fn new_creator_plans_one_change_with_validated_candidate() {
        let current = empty_config();
        let plan = plan_apply(&current, &with_creator(VOICE, "Room ##"), &inventory());
        match plan {
            ApplyPlan::Changes {
                candidate,
                hash,
                change_count,
                text,
                skipped_unknown_channels,
            } => {
                assert_eq!(change_count, 1);
                assert!(skipped_unknown_channels.is_empty());
                assert_eq!(candidate.creators.len(), 1);
                assert_eq!(candidate.creators[0].channel_id, VOICE.to_string());
                assert_eq!(hash, diff_content_hash(&current, &candidate));
                assert!(validate_configuration(&candidate, &inventory()).is_ok());
                assert!(!text.is_empty());
            }
            other => panic!("expected Changes, got {other:?}"),
        }
    }

    #[test]
    fn export_round_trip_of_a_planned_candidate_is_no_changes() {
        let current = empty_config();
        let ApplyPlan::Changes { candidate, .. } =
            plan_apply(&current, &with_creator(VOICE, "Room ##"), &inventory())
        else {
            panic!("expected Changes");
        };
        let exported = export_configuration(&candidate, &inventory()).expect("export");
        assert!(matches!(
            plan_apply(&candidate, &exported, &inventory()),
            ApplyPlan::NoChanges { .. }
        ));
    }
}
