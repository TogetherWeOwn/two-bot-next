//! V11 configuration codec, built from `docs/voice-rooms.md` only.
//!
//! The caller supplies a trusted guild inventory. Successful decoding returns a
//! complete candidate, never an application effect. Permissions, template
//! compilation, diff/confirmation and transactional persistence belong to the
//! integration layer. No Discord or database types are used here.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use serde::de::{value::MapAccessDeserializer, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const VOICE_CONFIG_VERSION: u32 = 1;

/// Snowflakes are decimal strings on the wire to preserve all 64 bits in JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceConfiguration {
    pub version: u32,
    pub guild_id: String,
    #[serde(deserialize_with = "object_list")]
    pub creators: Vec<CreatorConfiguration>,
    /// Templates for permanent voice/stage channels, not temporary rooms.
    #[serde(deserialize_with = "object_list")]
    pub templates: Vec<ChannelTemplates>,
    #[serde(deserialize_with = "object_list")]
    pub aliases: Vec<GameAlias>,
    #[serde(deserialize_with = "object_list")]
    pub lists: Vec<RandomList>,
    #[serde(deserialize_with = "nullable_object")]
    pub logging: Option<LoggingConfiguration>,
    #[serde(deserialize_with = "object")]
    pub settings: GuildSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatorConfiguration {
    pub channel_id: String,
    pub name_template: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub status_template: Option<String>,
    pub default_limit: u16,
    pub always_private: bool,
    pub text_channels: bool,
    pub position: RoomPosition,
    pub first_number: u32,
    pub group_by_category: bool,
    /// A declarative source only; does not grant or evaluate permissions.
    #[serde(deserialize_with = "object")]
    pub permission_source: PermissionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomPosition {
    Above,
    Below,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PermissionSource {
    Creator {},
    Category {},
    Channel { channel_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelTemplates {
    pub channel_id: String,
    pub name_template: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub status_template: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameAlias {
    pub game: String,
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RandomList {
    pub name: String,
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfiguration {
    pub channel_id: String,
    pub detail: LogDetail,
    pub mention_member_ids: Vec<String>,
    pub mention_role_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogDetail {
    Errors,
    Lifecycle,
    Verbose,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuildSettings {
    pub creation_enabled: bool,
    pub unique_names: bool,
    pub no_game_label: String,
    pub force_single_game: bool,
    pub count_members_without_activity: bool,
    /// IANA name interpretation is deferred to the runtime's timezone provider.
    pub time_zone: String,
    pub text_channel_name: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub text_viewer_role_id: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub command_role_id: Option<String>,
    #[serde(deserialize_with = "object_list")]
    pub command_roles: Vec<CommandRoles>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRoles {
    pub command: String,
    pub role_ids: Vec<String>,
}

/// Inventory must come from the caller, never from the uploaded configuration.
/// Include foreign entries if available to distinguish unknown from cross-guild
/// references. A guild's @everyone role uses the guild ID as its role ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildInventory {
    pub guild_id: String,
    pub channels: BTreeMap<String, ChannelReference>,
    pub roles: BTreeMap<String, String>,
    pub members: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReference {
    pub guild_id: String,
    pub kind: ChannelKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Voice,
    Stage,
    Text,
    Category,
}

/// Errors identify a field, not raw uploaded content (safe to surface in a diff).
#[derive(Debug, Error, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceConfigError {
    #[error("malformed configuration JSON at line {line}, column {column}")]
    Malformed { line: usize, column: usize },
    #[error("unsupported voice configuration version {0}")]
    UnsupportedVersion(u32),
    #[error("configuration belongs to a different guild")]
    GuildMismatch,
    #[error("invalid configuration field {field}: {reason}")]
    Invalid { field: String, reason: &'static str },
    #[error("unknown {kind} reference at {field}")]
    UnknownReference { field: String, kind: &'static str },
    #[error("cross-guild {kind} reference at {field}")]
    CrossGuildReference { field: String, kind: &'static str },
}

impl From<serde_json::Error> for VoiceConfigError {
    fn from(error: serde_json::Error) -> Self {
        Self::Malformed {
            line: error.line(),
            column: error.column(),
        }
    }
}

/// Strict, all-or-nothing decoding. Unknown references are reported as errors;
/// the command layer may explicitly remove unknown-channel entries for a new
/// preview, then revalidate before confirmation. This function never skips them.
pub fn import_configuration(
    json: &[u8],
    inventory: &GuildInventory,
) -> Result<VoiceConfiguration, VoiceConfigError> {
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    let config = object(&mut deserializer)?;
    deserializer.end()?;
    validate_configuration(&config, inventory)?;
    Ok(config)
}

/// Export uses the same validation contract as import and retains list order,
/// strings and optional values. It does not include runtime room/owner state.
pub fn export_configuration(
    config: &VoiceConfiguration,
    inventory: &GuildInventory,
) -> Result<Vec<u8>, VoiceConfigError> {
    validate_configuration(config, inventory)?;
    Ok(serde_json::to_vec_pretty(config)?)
}

pub fn validate_configuration(
    config: &VoiceConfiguration,
    inventory: &GuildInventory,
) -> Result<(), VoiceConfigError> {
    if config.version != VOICE_CONFIG_VERSION {
        return Err(VoiceConfigError::UnsupportedVersion(config.version));
    }
    snowflake(&config.guild_id, "guild_id")?;
    snowflake(&inventory.guild_id, "inventory.guild_id")?;
    if config.guild_id != inventory.guild_id {
        return Err(VoiceConfigError::GuildMismatch);
    }

    let mut channels = BTreeSet::new();
    for (i, creator) in config.creators.iter().enumerate() {
        let path = format!("creators[{i}]");
        channel(
            &creator.channel_id,
            &format!("{path}.channel_id"),
            inventory,
            &[ChannelKind::Voice],
        )?;
        unique(
            &mut channels,
            &creator.channel_id,
            &format!("{path}.channel_id"),
        )?;
        if creator.default_limit > 99 {
            return Err(invalid(&format!("{path}.default_limit"), "must be 0..=99"));
        }
        if creator.first_number == 0 {
            return Err(invalid(&format!("{path}.first_number"), "must be positive"));
        }
        if let PermissionSource::Channel { channel_id } = &creator.permission_source {
            channel(
                channel_id,
                &format!("{path}.permission_source.channel_id"),
                inventory,
                &[
                    ChannelKind::Voice,
                    ChannelKind::Stage,
                    ChannelKind::Text,
                    ChannelKind::Category,
                ],
            )?;
        }
    }
    for (i, template) in config.templates.iter().enumerate() {
        let path = format!("templates[{i}].channel_id");
        channel(
            &template.channel_id,
            &path,
            inventory,
            &[ChannelKind::Voice, ChannelKind::Stage],
        )?;
        unique(&mut channels, &template.channel_id, &path)?;
    }
    let mut games = BTreeSet::new();
    for (i, alias) in config.aliases.iter().enumerate() {
        let path = format!("aliases[{i}]");
        nonempty(&alias.game, &format!("{path}.game"))?;
        nonempty(&alias.alias, &format!("{path}.alias"))?;
        unique(&mut games, &alias.game, &format!("{path}.game"))?;
    }
    let mut lists = BTreeSet::new();
    for (i, list) in config.lists.iter().enumerate() {
        let path = format!("lists[{i}]");
        nonempty(&list.name, &format!("{path}.name"))?;
        unique(&mut lists, &list.name, &format!("{path}.name"))?;
        if list.choices.is_empty() {
            return Err(invalid(&format!("{path}.choices"), "must contain a choice"));
        }
        for (j, choice) in list.choices.iter().enumerate() {
            nonempty(choice, &format!("{path}.choices[{j}]"))?;
        }
    }
    if let Some(logging) = &config.logging {
        channel(
            &logging.channel_id,
            "logging.channel_id",
            inventory,
            &[ChannelKind::Text],
        )?;
        references(
            &logging.mention_member_ids,
            "logging.mention_member_ids",
            "member",
            &inventory.members,
            inventory,
        )?;
        references(
            &logging.mention_role_ids,
            "logging.mention_role_ids",
            "role",
            &inventory.roles,
            inventory,
        )?;
    }
    let settings = &config.settings;
    literal_name(&settings.no_game_label, "settings.no_game_label")?;
    literal_name(&settings.text_channel_name, "settings.text_channel_name")?;
    nonempty(&settings.time_zone, "settings.time_zone")?;
    for (field, id) in [
        (
            "settings.text_viewer_role_id",
            &settings.text_viewer_role_id,
        ),
        ("settings.command_role_id", &settings.command_role_id),
    ] {
        if let Some(id) = id {
            reference(id, field, "role", &inventory.roles, inventory)?;
        }
    }
    let mut commands = BTreeSet::new();
    for (i, restriction) in settings.command_roles.iter().enumerate() {
        let path = format!("settings.command_roles[{i}]");
        nonempty(&restriction.command, &format!("{path}.command"))?;
        unique(
            &mut commands,
            &restriction.command,
            &format!("{path}.command"),
        )?;
        references(
            &restriction.role_ids,
            &format!("{path}.role_ids"),
            "role",
            &inventory.roles,
            inventory,
        )?;
    }
    Ok(())
}

// Serde's derived struct decoder also accepts positional arrays. Gate each
// uploaded DTO through map access without buffering into Value, which would
// collapse duplicate keys before the derived decoder can reject them.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a configuration object")
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }

        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<T, D::Error> {
    Object::deserialize(deserializer).map(|object| object.0)
}

fn object_list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    Vec::<Object<T>>::deserialize(deserializer)
        .map(|objects| objects.into_iter().map(|object| object.0).collect())
}

fn nullable_object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::<Object<T>>::deserialize(deserializer).map(|object| object.map(|object| object.0))
}

fn invalid(field: &str, reason: &'static str) -> VoiceConfigError {
    VoiceConfigError::Invalid {
        field: field.to_owned(),
        reason,
    }
}

fn snowflake(id: &str, field: &str) -> Result<(), VoiceConfigError> {
    if id.starts_with('0')
        || !id.bytes().all(|c| c.is_ascii_digit())
        || !id.parse::<u64>().is_ok_and(|n| n > 0)
    {
        return Err(invalid(
            field,
            "must be a canonical nonzero decimal u64 string",
        ));
    }
    Ok(())
}

fn nonempty(value: &str, field: &str) -> Result<(), VoiceConfigError> {
    if value.trim().is_empty() {
        return Err(invalid(field, "must not be blank"));
    }
    Ok(())
}

fn literal_name(value: &str, field: &str) -> Result<(), VoiceConfigError> {
    nonempty(value, field)?;
    if value.chars().count() > 100 {
        return Err(invalid(field, "must be at most 100 characters"));
    }
    Ok(())
}

fn unique<'a>(
    seen: &mut BTreeSet<&'a str>,
    value: &'a str,
    field: &str,
) -> Result<(), VoiceConfigError> {
    if !seen.insert(value) {
        return Err(invalid(field, "duplicate entry"));
    }
    Ok(())
}

fn reference(
    id: &str,
    field: &str,
    kind: &'static str,
    known: &BTreeMap<String, String>,
    inventory: &GuildInventory,
) -> Result<(), VoiceConfigError> {
    snowflake(id, field)?;
    let guild = known
        .get(id)
        .ok_or_else(|| VoiceConfigError::UnknownReference {
            field: field.to_owned(),
            kind,
        })?;
    if guild != &inventory.guild_id {
        return Err(VoiceConfigError::CrossGuildReference {
            field: field.to_owned(),
            kind,
        });
    }
    Ok(())
}

fn references(
    ids: &[String],
    field: &str,
    kind: &'static str,
    known: &BTreeMap<String, String>,
    inventory: &GuildInventory,
) -> Result<(), VoiceConfigError> {
    let mut seen = BTreeSet::new();
    for (i, id) in ids.iter().enumerate() {
        let path = format!("{field}[{i}]");
        reference(id, &path, kind, known, inventory)?;
        unique(&mut seen, id, &path)?;
    }
    Ok(())
}

fn channel(
    id: &str,
    field: &str,
    inventory: &GuildInventory,
    allowed: &[ChannelKind],
) -> Result<(), VoiceConfigError> {
    snowflake(id, field)?;
    let channel = inventory
        .channels
        .get(id)
        .ok_or_else(|| VoiceConfigError::UnknownReference {
            field: field.to_owned(),
            kind: "channel",
        })?;
    if channel.guild_id != inventory.guild_id {
        return Err(VoiceConfigError::CrossGuildReference {
            field: field.to_owned(),
            kind: "channel",
        });
    }
    if !allowed.contains(&channel.kind) {
        return Err(invalid(field, "channel has the wrong kind"));
    }
    Ok(())
}
