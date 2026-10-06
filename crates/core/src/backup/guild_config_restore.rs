//! Guild-config restore planner (TOG-3513 follow-through).
//!
//! Port of legacy `src/redesign/guildConfigRestore.ts`: `planRestore` diffs
//! a sealed snapshot against the live guild into an ordered operation list
//! (roles → role positions → categories → channels → channel positions →
//! overwrites → settings → emojis); `apply_restore_plan` executes it through
//! [`super::guild_config_api::GuildConfigDiscordApi`], resolving the
//! id references Discord assigns at create time; `remap_snapshot_ids` rewrites
//! the snapshot into post-restore ids so the caller can prove the hashes match.
//!
//! Snapshots are `serde_json::Map` (the file shape), not typed structs: the
//! restore must tolerate fields Discord adds between capture and restore.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use thiserror::Error;

use super::guild_config::{canonical_snapshot, config_hash, GUILD_CONFIG_FIELDS};

/// Planner refusal: ambiguous targets, unknown references, unrestorable emoji.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct RestorePlanError(pub String);

/// A permission overwrite in restore-internal form.
#[derive(Debug, Clone)]
pub struct GuildOverwrite {
    pub id: String,
    pub overwrite_type: i64,
    pub allow: String,
    pub deny: String,
}

fn parse_overwrite(v: &Value) -> GuildOverwrite {
    GuildOverwrite {
        id: v.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
        overwrite_type: v.get("type").and_then(Value::as_i64).unwrap_or(0),
        allow: v
            .get("allow")
            .and_then(Value::as_str)
            .unwrap_or("0")
            .to_owned(),
        deny: v
            .get("deny")
            .and_then(Value::as_str)
            .unwrap_or("0")
            .to_owned(),
    }
}

fn overwrite_value(o: &GuildOverwrite, guild_id: &str) -> Value {
    let id = if o.overwrite_type == 0 && o.id != guild_id {
        serde_json::json!({"restoreReference": "role", "sourceId": o.id})
    } else {
        Value::String(o.id.clone())
    };
    serde_json::json!({"id": id, "type": o.overwrite_type, "allow": o.allow, "deny": o.deny})
}

/// One restore operation: a labelled Discord write with reference placeholders.
#[derive(Debug, Clone)]
pub struct RestoreOperation {
    pub label: String,
    pub method: String, // "POST" | "PATCH"
    pub path: RestorePath,
    pub body: Value,
    pub capture_id: Option<(String, String)>, // (resource, source_id)
}

/// A Discord path: literal, or a channel created earlier in the same plan.
#[derive(Debug, Clone)]
pub enum RestorePath {
    Literal(String),
    Channel(String), // source channel id
}

/// A channel needing overwrite restoration (for the permission preflight).
#[derive(Debug, Clone)]
pub struct OverwriteTarget {
    pub current_id: Option<String>,
    pub name: String,
    pub action_permission_overwrites: Vec<GuildOverwrite>,
    pub desired_overwrites: Vec<GuildOverwrite>,
    pub permission_ceiling_overwrites: Vec<GuildOverwrite>,
}

/// Operation counts, mirroring the legacy plan shape.
#[derive(Debug, Clone, Default)]
pub struct RestoreCounts {
    pub roles: u64,
    pub channels: u64,
    pub overwrites: u64,
    pub settings: u64,
    pub emojis: u64,
    pub operations: u64,
}

/// Known id mappings at plan time (snapshot id → live id).
#[derive(Debug, Clone, Default)]
pub struct RestoreIdMaps {
    pub roles: BTreeMap<String, String>,
    pub channels: BTreeMap<String, String>,
    pub emojis: BTreeMap<String, String>,
}

/// The full restore plan.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub counts: RestoreCounts,
    pub known_ids: RestoreIdMaps,
    pub overwrite_roles: Vec<(String, String, i64)>, // (id, name, position)
    pub overwrite_targets: Vec<OverwriteTarget>,
    pub operations: Vec<RestoreOperation>,
}

fn role_body(role: &Map<String, Value>) -> Value {
    let mut body = Map::new();
    for field in ["name", "color", "hoist", "permissions", "mentionable"] {
        if let Some(v) = role.get(field) {
            body.insert(field.to_owned(), v.clone());
        }
    }
    Value::Object(body)
}

fn same(a: &Value, b: &Value) -> bool {
    config_hash(a) == config_hash(b)
}

fn snapshot_roles(snapshot: &Map<String, Value>) -> Vec<Map<String, Value>> {
    snapshot
        .get("roles")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default()
}

fn snapshot_channels(snapshot: &Map<String, Value>) -> Vec<Map<String, Value>> {
    snapshot
        .get("channels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default()
}

fn overwrites_of(channel: &Map<String, Value>) -> Vec<GuildOverwrite> {
    channel
        .get("permission_overwrites")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_overwrite).collect())
        .unwrap_or_default()
}

fn str_field<'a>(obj: &'a Map<String, Value>, field: &str) -> &'a str {
    obj.get(field).and_then(Value::as_str).unwrap_or("")
}

fn channel_core_body(channel: &Map<String, Value>, parent_id: Value) -> Value {
    let mut body = Map::new();
    for field in [
        "name",
        "topic",
        "nsfw",
        "bitrate",
        "user_limit",
        "rate_limit_per_user",
    ] {
        if let Some(v) = channel.get(field) {
            body.insert(field.to_owned(), v.clone());
        }
    }
    body.insert(
        "type".to_owned(),
        channel.get("type").cloned().unwrap_or(Value::Null),
    );
    body.insert("parent_id".to_owned(), parent_id);
    Value::Object(body)
}

fn shape_error(path: &str, expected: &str) -> RestorePlanError {
    RestorePlanError(format!("{path} must be {expected}."))
}

fn nonempty_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn decimal_mask(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|s| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u128>().is_ok()
    })
}

fn validate_guild_id<'a>(
    input: &'a Map<String, Value>,
    label: &str,
) -> Result<&'a str, RestorePlanError> {
    let id = input.get("guildId").and_then(Value::as_str).filter(|s| {
        !s.is_empty()
            && s.bytes().all(|b| b.is_ascii_digit())
            && s.parse::<u64>().is_ok_and(|id| id != 0)
    });
    id.ok_or_else(|| {
        shape_error(
            &format!("{label}.guildId"),
            "a nonzero decimal guild ID string",
        )
    })
}

// A valid seal proves integrity, not shape. Validate both inputs before the
// forgiving legacy accessors can turn malformed data into empty/default values.
// Absent optional collections retain their legacy meaning; present ones must
// have the documented shape. Unknown additive fields are intentionally ignored.
fn validate_input_shape(input: &Map<String, Value>, label: &str) -> Result<(), RestorePlanError> {
    let guild = input
        .get("guild")
        .and_then(Value::as_object)
        .ok_or_else(|| shape_error(&format!("{label}.guild"), "an object"))?;
    for field in GUILD_CONFIG_FIELDS
        .iter()
        .filter(|f| f.ends_with("_channel_id"))
    {
        if let Some(value) = guild.get(*field) {
            if !value.is_null() && !nonempty_string(Some(value)) {
                return Err(shape_error(
                    &format!("{label}.guild.{field}"),
                    "null or a nonempty ID string",
                ));
            }
        }
    }
    for collection in ["roles", "channels", "emojis"] {
        let Some(value) = input.get(collection) else {
            continue;
        };
        let rows = value
            .as_array()
            .ok_or_else(|| shape_error(&format!("{label}.{collection}"), "an array"))?;
        for (index, value) in rows.iter().enumerate() {
            let path = format!("{label}.{collection}[{index}]");
            let row = value
                .as_object()
                .ok_or_else(|| shape_error(&path, "an object"))?;
            if !nonempty_string(row.get("id")) {
                return Err(shape_error(&format!("{path}.id"), "a nonempty ID string"));
            }
            // Discord can return a null name for unavailable managed emojis.
            if collection != "emojis" && !nonempty_string(row.get("name")) {
                return Err(shape_error(&format!("{path}.name"), "a nonempty string"));
            }
            // Unvalidated emoji names reach the planner: "" plans a create
            // that Discord rejects after earlier writes land, while a
            // non-string name is silently skipped. Null stays valid for
            // unavailable managed emojis.
            if collection == "emojis"
                && row
                    .get("name")
                    .is_none_or(|name| !name.is_null() && !nonempty_string(Some(name)))
            {
                return Err(shape_error(
                    &format!("{path}.name"),
                    "null or a nonempty string",
                ));
            }
            for field in ["managed", "hoist", "mentionable"] {
                if let Some(value) = row.get(field) {
                    if !value.is_boolean() {
                        return Err(shape_error(&format!("{path}.{field}"), "a boolean"));
                    }
                }
            }
            if let Some(value) = row.get("position") {
                if value.as_i64().is_none() {
                    return Err(shape_error(&format!("{path}.position"), "an integer"));
                }
            }
            if collection == "channels" {
                if row
                    .get("type")
                    .and_then(Value::as_i64)
                    .is_none_or(|t| t < 0)
                {
                    return Err(shape_error(
                        &format!("{path}.type"),
                        "a nonnegative integer",
                    ));
                }
                if let Some(value) = row.get("parent_id") {
                    if !value.is_null() && !nonempty_string(Some(value)) {
                        return Err(shape_error(
                            &format!("{path}.parent_id"),
                            "null or a nonempty ID string",
                        ));
                    }
                }
                if let Some(value) = row.get("permission_overwrites") {
                    let path = format!("{path}.permission_overwrites");
                    let overwrites = value
                        .as_array()
                        .ok_or_else(|| shape_error(&path, "an array"))?;
                    for (index, value) in overwrites.iter().enumerate() {
                        let path = format!("{path}[{index}]");
                        let overwrite = value
                            .as_object()
                            .ok_or_else(|| shape_error(&path, "an object"))?;
                        if !nonempty_string(overwrite.get("id")) {
                            return Err(shape_error(&format!("{path}.id"), "a nonempty ID string"));
                        }
                        if !matches!(overwrite.get("type").and_then(Value::as_i64), Some(0 | 1)) {
                            return Err(shape_error(
                                &format!("{path}.type"),
                                "integer 0 (role) or 1 (member)",
                            ));
                        }
                        for field in ["allow", "deny"] {
                            if !decimal_mask(overwrite.get(field)) {
                                return Err(shape_error(
                                    &format!("{path}.{field}"),
                                    "an unsigned decimal permission mask string",
                                ));
                            }
                        }
                    }
                }
            }
            if collection == "emojis" {
                if let Some(value) = row.get("roles") {
                    let path = format!("{path}.roles");
                    let roles = value
                        .as_array()
                        .ok_or_else(|| shape_error(&path, "an array"))?;
                    for (index, value) in roles.iter().enumerate() {
                        if !nonempty_string(Some(value)) {
                            return Err(shape_error(
                                &format!("{path}[{index}]"),
                                "a nonempty role ID string",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Diff a snapshot against the live guild. Refuses guild mismatch, ambiguous
/// duplicate channels, references to unknown roles, and unrestorable emoji —
/// before any write, so a bad plan is a refusal, not a half-applied restore.
pub fn plan_restore(
    snapshot: &Map<String, Value>,
    current: &Map<String, Value>,
) -> Result<RestorePlan, RestorePlanError> {
    let fail = |m: String| RestorePlanError(m);
    let snapshot_guild = validate_guild_id(snapshot, "Snapshot")?;
    let current_guild = validate_guild_id(current, "Target")?;
    if snapshot_guild != current_guild {
        return Err(fail(format!(
            "Snapshot guild {snapshot_guild} does not match target guild {current_guild}."
        )));
    }
    validate_input_shape(snapshot, "Snapshot")?;
    validate_input_shape(current, "Target")?;
    let current_guild_owned = current_guild.to_owned();

    let mut role_ids: BTreeMap<String, String> =
        BTreeMap::from([(snapshot_guild.to_owned(), current_guild_owned.clone())]);
    let mut channel_ids: BTreeMap<String, String> = BTreeMap::new();
    let mut emoji_ids: BTreeMap<String, String> = BTreeMap::new();

    let mut role_ops: Vec<RestoreOperation> = Vec::new();
    let mut role_position_ops: Vec<RestoreOperation> = Vec::new();
    let mut category_ops: Vec<RestoreOperation> = Vec::new();
    let mut channel_ops: Vec<RestoreOperation> = Vec::new();
    let mut channel_position_ops: Vec<RestoreOperation> = Vec::new();
    let mut overwrite_ops: Vec<RestoreOperation> = Vec::new();
    let mut settings_ops: Vec<RestoreOperation> = Vec::new();
    let mut emoji_ops: Vec<RestoreOperation> = Vec::new();
    let mut counts = RestoreCounts::default();
    let mut overwrite_role_ids: Vec<String> = Vec::new();

    let snapshot_role_list = snapshot_roles(snapshot);
    let current_roles = snapshot_roles(current);
    let snapshot_channel_list = snapshot_channels(snapshot);
    let current_channels = snapshot_channels(current);
    let snapshot_roles = &snapshot_role_list;
    let snapshot_channels = &snapshot_channel_list;

    // Reserve ALL surviving compatible identities before any name fallback,
    // including identities processed later (names/parents may have drifted).
    for role in snapshot_roles {
        if current_roles.iter().any(|actual| {
            str_field(actual, "id") == str_field(role, "id")
                && is_managed(actual) == is_managed(role)
        }) {
            role_ids.insert(
                str_field(role, "id").to_owned(),
                str_field(role, "id").to_owned(),
            );
        }
    }
    for channel in snapshot_channels {
        if current_channels.iter().any(|actual| {
            str_field(actual, "id") == str_field(channel, "id")
                && actual.get("type") == channel.get("type")
        }) {
            channel_ids.insert(
                str_field(channel, "id").to_owned(),
                str_field(channel, "id").to_owned(),
            );
        }
    }
    let snapshot_roles_by_id: BTreeMap<&str, &Map<String, Value>> = snapshot_roles
        .iter()
        .map(|r| (str_field(r, "id"), r))
        .collect();

    // @everyone is editable, but must never be created or positioned.
    // PATCH permissions: https://docs.discord.com/developers/resources/guild#modify-guild-role
    if let Some(everyone) = snapshot_roles
        .iter()
        .find(|r| str_field(r, "id") == snapshot_guild)
    {
        let actual = current_roles
            .iter()
            .find(|r| str_field(r, "id") == current_guild)
            .ok_or_else(|| fail("Target guild has no @everyone role.".to_owned()))?;
        if everyone.get("permissions") != actual.get("permissions") {
            role_ops.push(RestoreOperation {
                label: "patch role @everyone".to_owned(),
                method: "PATCH".to_owned(),
                path: RestorePath::Literal(format!("/guilds/{current_guild_owned}/roles/{current_guild_owned}")),
                body: serde_json::json!({"permissions": everyone.get("permissions").cloned().unwrap_or(Value::Null)}),
                capture_id: None,
            });
            counts.roles += 1;
        }
    }
    let mut source_roles: Vec<&Map<String, Value>> = snapshot_roles
        .iter()
        .filter(|r| {
            !r.get("managed").and_then(Value::as_bool).unwrap_or(false)
                && str_field(r, "id") != snapshot_guild
        })
        .collect();
    source_roles.sort_by_key(|r| r.get("position").and_then(Value::as_i64).unwrap_or(0));
    let mut role_positions = Vec::new();
    for role in &source_roles {
        let name = str_field(role, "name");
        let source_id = str_field(role, "id");
        let actual = if let Some(id) = role_ids.get(source_id) {
            current_roles.iter().find(|r| str_field(r, "id") == id)
        } else {
            let candidates: Vec<_> = current_roles
                .iter()
                .filter(|r| {
                    !is_managed(r)
                        && str_field(r, "id") != current_guild
                        && str_field(r, "name") == name
                })
                .collect();
            unique_unclaimed(&candidates, &role_ids, &format!("role {name}"))?
        };
        if actual.is_none() || actual.is_some_and(|r| role.get("position") != r.get("position")) {
            role_positions.push(serde_json::json!({
                "id": {"restoreReference": "role", "sourceId": source_id},
                "position": role.get("position").cloned().unwrap_or(Value::Null),
            }));
        }
        let Some(actual) = actual else {
            role_ops.push(RestoreOperation {
                label: format!("create role {name}"),
                method: "POST".to_owned(),
                path: RestorePath::Literal(format!("/guilds/{current_guild_owned}/roles")),
                body: role_body(role),
                capture_id: Some(("role".to_owned(), str_field(role, "id").to_owned())),
            });
            counts.roles += 1;
            continue;
        };
        role_ids.insert(
            str_field(role, "id").to_owned(),
            str_field(actual, "id").to_owned(),
        );
        if !same(&role_body(role), &role_body(actual)) {
            role_ops.push(RestoreOperation {
                label: format!("patch role {name}"),
                method: "PATCH".to_owned(),
                path: RestorePath::Literal(format!(
                    "/guilds/{current_guild_owned}/roles/{}",
                    str_field(actual, "id")
                )),
                body: role_body(role),
                capture_id: None,
            });
            counts.roles += 1;
        }
    }
    if !role_positions.is_empty() {
        role_position_ops.push(RestoreOperation {
            label: "restore role positions".to_owned(),
            method: "PATCH".to_owned(),
            path: RestorePath::Literal(format!("/guilds/{current_guild_owned}/roles")),
            body: Value::Array(role_positions),
            capture_id: None,
        });
        counts.roles += 1;
    }

    // Categories first (channels need their parents).
    let mut channel_positions_differ = false;
    let mut source_channel_positions: Vec<Value> = Vec::new();
    let mut source_categories: Vec<&Map<String, Value>> = snapshot_channels
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_i64) == Some(4))
        .collect();
    source_categories.sort_by_key(|c| c.get("position").and_then(Value::as_i64).unwrap_or(0));
    for category in &source_categories {
        let name = str_field(category, "name");
        let actual = existing_candidate(category, None, &current_channels, None, &channel_ids)?;
        // TOG-9970 finding 1: resolve the full saved overwrite set BEFORE
        // creation, so a private category is restricted from its first
        // instant — never initially public with a later repair. Role
        // references stay symbolic; role creates precede channel creates in
        // op order, so they resolve at apply time. The separate overwrite
        // PATCH below stays as an idempotent repair.
        let expected = overwrites_of(category);
        let create_overwrites = Value::Array(
            expected
                .iter()
                .map(|o| overwrite_value(o, snapshot_guild))
                .collect(),
        );
        if let Some(actual) = actual {
            channel_ids.insert(
                str_field(category, "id").to_owned(),
                str_field(actual, "id").to_owned(),
            );
            channel_positions_differ = channel_positions_differ
                || category.get("position").and_then(Value::as_i64)
                    != actual.get("position").and_then(Value::as_i64);
            if category.get("name") != actual.get("name") {
                // Categories support name/position/overwrites, not parent_id.
                // https://docs.discord.com/developers/resources/channel#modify-channel
                category_ops.push(RestoreOperation {
                    label: format!("patch category {name}"),
                    method: "PATCH".to_owned(),
                    path: RestorePath::Channel(str_field(category, "id").to_owned()),
                    body: serde_json::json!({"name": name}),
                    capture_id: None,
                });
                counts.channels += 1;
            }
        } else {
            category_ops.push(RestoreOperation {
                label: format!("create category {name}"),
                method: "POST".to_owned(),
                path: RestorePath::Literal(format!("/guilds/{current_guild_owned}/channels")),
                body: serde_json::json!({
                    "name": name,
                    "type": 4,
                    "position": category.get("position").cloned().unwrap_or(Value::Null),
                    "permission_overwrites": create_overwrites,
                }),
                capture_id: Some(("channel".to_owned(), str_field(category, "id").to_owned())),
            });
            counts.channels += 1;
            channel_positions_differ = true;
        }
        source_channel_positions.push(serde_json::json!({
            "id": {"restoreReference": "channel", "sourceId": str_field(category, "id")},
            "position": category.get("position").cloned().unwrap_or(Value::Null),
        }));

        if overwrites_differ(&expected, actual, &role_ids) {
            for o in &expected {
                if o.overwrite_type == 0
                    && o.id != snapshot_guild
                    && !overwrite_role_ids.contains(&o.id)
                {
                    overwrite_role_ids.push(o.id.clone());
                }
            }
            overwrite_ops.push(RestoreOperation {
                label: format!("restore overwrites {name}"),
                method: "PATCH".to_owned(),
                path: RestorePath::Channel(str_field(category, "id").to_owned()),
                body: serde_json::json!({
                    "permission_overwrites": expected.iter().map(|o| overwrite_value(o, snapshot_guild)).collect::<Vec<_>>(),
                }),
                capture_id: None,
            });
            counts.overwrites += expected.len().max(1) as u64;
        }
    }

    // Non-category channels.
    let mut source_noncat: Vec<&Map<String, Value>> = snapshot_channels
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_i64) != Some(4))
        .collect();
    source_noncat.sort_by_key(|c| c.get("position").and_then(Value::as_i64).unwrap_or(0));
    for channel in &source_noncat {
        let name = str_field(channel, "name");
        let parent = channel
            .get("parent_id")
            .and_then(Value::as_str)
            .and_then(|pid| snapshot_channels.iter().find(|c| str_field(c, "id") == pid));
        let actual_parent_id: Option<String> = parent
            .and_then(|p| channel_ids.get(str_field(p, "id")))
            .cloned();
        let actual = existing_candidate(
            channel,
            parent,
            &current_channels,
            actual_parent_id.as_deref(),
            &channel_ids,
        )?;
        let target_parent = parent.map(|p| {
            serde_json::json!({"restoreReference": "channel", "sourceId": str_field(p, "id")})
        }).unwrap_or(Value::Null);
        let expected_known = channel_core_body(
            channel,
            actual_parent_id
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        let target_body = channel_core_body(channel, target_parent);

        // TOG-9970 finding 1: full saved overwrite set goes into the create
        // POST too — a private channel is restricted from its first instant.
        // Expected value first; the overwrite PATCH below stays as repair.
        let expected = overwrites_of(channel);
        let create_overwrites = Value::Array(
            expected
                .iter()
                .map(|o| overwrite_value(o, snapshot_guild))
                .collect(),
        );
        if let Some(actual) = actual {
            channel_ids.insert(
                str_field(channel, "id").to_owned(),
                str_field(actual, "id").to_owned(),
            );
            channel_positions_differ = channel_positions_differ
                || channel.get("position").and_then(Value::as_i64)
                    != actual.get("position").and_then(Value::as_i64);
            let actual_body = channel_core_body(
                actual,
                actual.get("parent_id").cloned().unwrap_or(Value::Null),
            );
            if (parent.is_some() && actual_parent_id.is_none())
                || !same(&expected_known, &actual_body)
            {
                channel_ops.push(RestoreOperation {
                    label: format!("patch channel {name}"),
                    method: "PATCH".to_owned(),
                    path: RestorePath::Channel(str_field(channel, "id").to_owned()),
                    body: target_body.clone(),
                    capture_id: None,
                });
                counts.channels += 1;
            }
        } else {
            let mut create_body = target_body.clone();
            create_body["position"] = channel.get("position").cloned().unwrap_or(Value::Null);
            create_body["permission_overwrites"] = create_overwrites;
            channel_ops.push(RestoreOperation {
                label: format!("create channel {name}"),
                method: "POST".to_owned(),
                path: RestorePath::Literal(format!("/guilds/{current_guild_owned}/channels")),
                body: create_body,
                capture_id: Some(("channel".to_owned(), str_field(channel, "id").to_owned())),
            });
            counts.channels += 1;
            channel_positions_differ = true;
        }
        // Parent moves (including moves to root/new categories) are already
        // carried by the per-channel core PATCH above. Never put parent_id in
        // the bulk position PATCH: Discord rejects unchanged-parent batches.
        source_channel_positions.push(serde_json::json!({
            "id": {"restoreReference": "channel", "sourceId": str_field(channel, "id")},
            "position": channel.get("position").cloned().unwrap_or(Value::Null),
        }));

        if overwrites_differ(&expected, actual, &role_ids) {
            for o in &expected {
                if o.overwrite_type == 0
                    && o.id != snapshot_guild
                    && !overwrite_role_ids.contains(&o.id)
                {
                    overwrite_role_ids.push(o.id.clone());
                }
            }
            overwrite_ops.push(RestoreOperation {
                label: format!("restore overwrites {name}"),
                method: "PATCH".to_owned(),
                path: RestorePath::Channel(str_field(channel, "id").to_owned()),
                body: serde_json::json!({
                    "permission_overwrites": expected.iter().map(|o| overwrite_value(o, snapshot_guild)).collect::<Vec<_>>(),
                }),
                capture_id: None,
            });
            counts.overwrites += expected.len().max(1) as u64;
        }
    }

    if channel_positions_differ {
        channel_position_ops.push(RestoreOperation {
            label: "restore channel positions".to_owned(),
            method: "PATCH".to_owned(),
            path: RestorePath::Literal(format!("/guilds/{current_guild_owned}/channels")),
            body: Value::Array(source_channel_positions),
            capture_id: None,
        });
        counts.channels += 1;
    }

    // Guild settings (channel-id fields as references).
    let snapshot_guild_obj = snapshot.get("guild").and_then(Value::as_object);
    let current_guild_obj = current.get("guild").and_then(Value::as_object);
    let guild_fields: Vec<&str> = GUILD_CONFIG_FIELDS
        .iter()
        .filter(|f| !f.ends_with("_channel_id"))
        .copied()
        .collect();
    let mut guild_body = Map::new();
    let mut known_guild_body = Map::new();
    for field in &guild_fields {
        if let Some(v) = snapshot_guild_obj.and_then(|g| g.get(*field)) {
            guild_body.insert((*field).to_owned(), v.clone());
            known_guild_body.insert((*field).to_owned(), v.clone());
        }
    }
    let mut unresolved_guild_channel = false;
    for field in [
        "system_channel_id",
        "rules_channel_id",
        "public_updates_channel_id",
        "afk_channel_id",
    ] {
        match snapshot_guild_obj.and_then(|g| g.get(field)) {
            Some(Value::String(source_id)) => {
                guild_body.insert(
                    field.to_owned(),
                    serde_json::json!({"restoreReference": "channel", "sourceId": source_id}),
                );
                match channel_ids.get(source_id) {
                    Some(known) => {
                        known_guild_body.insert(field.to_owned(), Value::String(known.clone()));
                    }
                    None => {
                        known_guild_body.insert(field.to_owned(), Value::Null);
                        unresolved_guild_channel = true;
                    }
                }
            }
            Some(Value::Null) | None => {
                guild_body.insert(field.to_owned(), Value::Null);
                known_guild_body.insert(field.to_owned(), Value::Null);
            }
            Some(other) => {
                guild_body.insert(field.to_owned(), other.clone());
                known_guild_body.insert(field.to_owned(), other.clone());
            }
        }
    }
    let mut current_guild_body = Map::new();
    for (field, _) in &known_guild_body {
        if let Some(v) = current_guild_obj.and_then(|g| g.get(field)) {
            current_guild_body.insert(field.clone(), v.clone());
        }
    }
    if unresolved_guild_channel
        || !same(
            &Value::Object(known_guild_body),
            &Value::Object(current_guild_body),
        )
    {
        settings_ops.push(RestoreOperation {
            label: "restore guild settings".to_owned(),
            method: "PATCH".to_owned(),
            path: RestorePath::Literal(format!("/guilds/{current_guild_owned}")),
            body: Value::Object(guild_body),
            capture_id: None,
        });
        counts.settings += 1;
    }

    // Emojis.
    let snapshot_emojis: Vec<Map<String, Value>> = snapshot
        .get("emojis")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default();
    let current_emojis: Vec<Map<String, Value>> = current
        .get("emojis")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default();
    // TOG-9970 finding 6: reserve compatible surviving unmanaged emoji IDs
    // first — identity is the ID, not the name. A surviving emoji renamed
    // after capture patches its original ID; name fallback below is only
    // for genuinely missing identities.
    for emoji in snapshot_emojis
        .iter()
        .filter(|e| !e.get("managed").and_then(Value::as_bool).unwrap_or(false))
    {
        let source_id = str_field(emoji, "id");
        if current_emojis.iter().any(|actual| {
            str_field(actual, "id") == source_id
                && !actual
                    .get("managed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }) {
            emoji_ids.insert(source_id.to_owned(), source_id.to_owned());
        }
    }
    for emoji in snapshot_emojis.iter().filter(|e| {
        !e.get("managed").and_then(Value::as_bool).unwrap_or(false)
            && e.get("name").and_then(Value::as_str).is_some()
    }) {
        let name = str_field(emoji, "name");
        let source_id = str_field(emoji, "id");
        let roles: Vec<Value> = emoji
            .get("roles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|r| serde_json::json!({"restoreReference": "role", "sourceId": r}))
                    .collect()
            })
            .unwrap_or_default();
        let known_roles: Vec<Value> = emoji
            .get("roles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|r| {
                        Value::String(role_ids.get(r).cloned().unwrap_or_else(|| r.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let has_created_ref = emoji
            .get("roles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .any(|r| !role_ids.contains_key(r))
            })
            .unwrap_or(false);
        // Identity first: a surviving ID wins over any name. Name fallback
        // applies only when the ID is genuinely gone, and only to a unique,
        // unclaimed unmanaged live emoji — swapped/duplicate names must not
        // crosswire or merge distinct identities.
        let actual: Option<&Map<String, Value>> = match emoji_ids.get(source_id).and_then(|live| {
            current_emojis
                .iter()
                .find(|a| !is_managed(a) && str_field(a, "id") == live)
        }) {
            Some(actual) => Some(actual),
            None => {
                let candidates: Vec<&Map<String, Value>> = current_emojis
                    .iter()
                    .filter(|a| {
                        !is_managed(a) && a.get("name").and_then(Value::as_str) == Some(name)
                    })
                    .collect();
                match candidates.as_slice() {
                    [] => None,
                    [one] => {
                        if emoji_ids.values().any(|id| id == str_field(one, "id")) {
                            return Err(fail(format!(
                                "Target emoji {name} is already claimed by another source; restore would not be injective."
                            )));
                        }
                        Some(*one)
                    }
                    _ => {
                        return Err(fail(format!(
                            "Target has multiple emoji {name} candidates; restore is ambiguous."
                        )));
                    }
                }
            }
        };
        match actual {
            None => {
                emoji_ops.push(RestoreOperation {
                    label: format!("create emoji {name}"),
                    method: "POST".to_owned(),
                    path: RestorePath::Literal(format!("/guilds/{current_guild_owned}/emojis")),
                    body: serde_json::json!({
                        "name": name,
                        "image": emoji_image(emoji)?,
                        "roles": roles,
                    }),
                    capture_id: Some(("emoji".to_owned(), str_field(emoji, "id").to_owned())),
                });
                counts.emojis += 1;
            }
            Some(actual) => {
                emoji_ids.insert(
                    str_field(emoji, "id").to_owned(),
                    str_field(actual, "id").to_owned(),
                );
                let actual_roles: Vec<Value> = actual
                    .get("roles")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if has_created_ref
                    || !same(
                        &serde_json::json!({"name": name, "roles": known_roles}),
                        &serde_json::json!({
                            "name": actual.get("name").cloned().unwrap_or(Value::Null),
                            "roles": actual_roles,
                        }),
                    )
                {
                    emoji_ops.push(RestoreOperation {
                        label: format!("patch emoji {name}"),
                        method: "PATCH".to_owned(),
                        path: RestorePath::Literal(format!(
                            "/guilds/{current_guild_owned}/emojis/{}",
                            str_field(actual, "id")
                        )),
                        body: serde_json::json!({"name": name, "roles": roles}),
                        capture_id: None,
                    });
                    counts.emojis += 1;
                }
            }
        }
    }
    for emoji in snapshot_emojis
        .iter()
        .filter(|e| e.get("managed").and_then(Value::as_bool).unwrap_or(false))
    {
        if let Some(actual) = current_emojis.iter().find(|c| {
            c.get("managed").and_then(Value::as_bool).unwrap_or(false)
                && str_field(c, "id") == str_field(emoji, "id")
        }) {
            emoji_ids.insert(
                str_field(emoji, "id").to_owned(),
                str_field(actual, "id").to_owned(),
            );
        }
    }

    let mut overwrite_roles = Vec::new();
    for role_id in &overwrite_role_ids {
        let Some(role) = snapshot_roles_by_id.get(role_id.as_str()) else {
            return Err(fail(format!(
                "Snapshot overwrite references unknown role {role_id}."
            )));
        };
        overwrite_roles.push((
            str_field(role, "id").to_owned(),
            str_field(role, "name").to_owned(),
            role.get("position").and_then(Value::as_i64).unwrap_or(0),
        ));
    }

    let known_role_id = |source: &str| {
        if source == snapshot_guild {
            current_guild_owned.clone()
        } else {
            role_ids
                .get(source)
                .cloned()
                .unwrap_or_else(|| source.to_owned())
        }
    };
    let known_overwrites = |ows: &[GuildOverwrite]| {
        ows.iter()
            .map(|o| GuildOverwrite {
                id: if o.overwrite_type == 0 {
                    known_role_id(&o.id)
                } else {
                    o.id.clone()
                },
                ..o.clone()
            })
            .collect::<Vec<_>>()
    };
    let overwrite_targets: Vec<OverwriteTarget> = snapshot_channels
        .iter()
        .filter(|c| {
            overwrite_ops
                .iter()
                .any(|op| matches!(&op.path, RestorePath::Channel(id) if id == str_field(c, "id")))
        })
        .map(|channel| {
            let current_id = channel_ids.get(str_field(channel, "id")).cloned();
            let current_channel = current_id
                .as_deref()
                .and_then(|id| current_channels.iter().find(|c| str_field(c, "id") == id));
            let source_parent = channel
                .get("parent_id")
                .and_then(Value::as_str)
                .and_then(|pid| snapshot_channels.iter().find(|c| str_field(c, "id") == pid));
            let current_parent_id = source_parent.and_then(|p| channel_ids.get(str_field(p, "id")));
            let current_parent = current_parent_id.and_then(|pid| {
                current_channels
                    .iter()
                    .find(|c| str_field(c, "id") == pid.as_str())
            });
            let desired_parent_ows = source_parent
                .map(|p| known_overwrites(&overwrites_of(p)))
                .unwrap_or_default();
            let synced_to_parent = match (current_channel, current_parent) {
                (Some(cc), Some(cp)) => same_overwrites(&overwrites_of(cc), &overwrites_of(cp)),
                _ => false,
            };
            let action_ows = match (source_parent, current_id.as_deref(), synced_to_parent) {
                (Some(_), _, true) => desired_parent_ows.clone(),
                (_, Some(_), _) => current_channel.map(overwrites_of).unwrap_or_default(),
                _ => Vec::new(),
            };
            OverwriteTarget {
                current_id,
                name: str_field(channel, "name").to_owned(),
                action_permission_overwrites: action_ows,
                desired_overwrites: known_overwrites(&overwrites_of(channel)),
                permission_ceiling_overwrites: desired_parent_ows,
            }
        })
        .collect();

    let mut operations = Vec::new();
    operations.extend(role_ops);
    operations.extend(role_position_ops);
    operations.extend(category_ops);
    operations.extend(channel_ops);
    operations.extend(channel_position_ops);
    operations.extend(overwrite_ops);
    operations.extend(settings_ops);
    operations.extend(emoji_ops);
    counts.operations = operations.len() as u64;

    let plan = RestorePlan {
        counts,
        known_ids: RestoreIdMaps {
            roles: role_ids,
            channels: channel_ids,
            emojis: emoji_ids,
        },
        overwrite_roles,
        overwrite_targets,
        operations,
    };
    let available = validate_ordered_dependencies(&plan)?;
    validate_snapshot_dependencies(snapshot, &available)?;
    Ok(plan)
}

fn is_managed(resource: &Map<String, Value>) -> bool {
    resource
        .get("managed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn unique_unclaimed<'a>(
    candidates: &[&'a Map<String, Value>],
    known: &BTreeMap<String, String>,
    target: &str,
) -> Result<Option<&'a Map<String, Value>>, RestorePlanError> {
    if candidates.len() > 1 {
        return Err(RestorePlanError(format!(
            "Target has multiple {target} candidates; restore is ambiguous."
        )));
    }
    if let Some(candidate) = candidates.first() {
        if known.values().any(|id| id == str_field(candidate, "id")) {
            return Err(RestorePlanError(format!(
                "Target {target} is already claimed by another source; restore would not be injective."
            )));
        }
    }
    Ok(candidates.first().copied())
}

fn existing_candidate<'a>(
    channel: &Map<String, Value>,
    parent: Option<&Map<String, Value>>,
    current: &'a [Map<String, Value>],
    actual_parent_id: Option<&str>,
    known: &BTreeMap<String, String>,
) -> Result<Option<&'a Map<String, Value>>, RestorePlanError> {
    let channel_type = channel.get("type").and_then(Value::as_i64);
    // Identity wins even with duplicate/swapped names or moved parents.
    if let Some(id) = known.get(str_field(channel, "id")) {
        return Ok(current.iter().find(|c| str_field(c, "id") == id));
    }
    if parent.is_some() && actual_parent_id.is_none() {
        return Ok(None);
    }
    let name = str_field(channel, "name");
    let candidates: Vec<_> = current
        .iter()
        .filter(|c| {
            c.get("type").and_then(Value::as_i64) == channel_type && str_field(c, "name") == name
        })
        .collect();
    let exact: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|c| c.get("parent_id").and_then(Value::as_str) == actual_parent_id)
        .collect();
    unique_unclaimed(
        if exact.is_empty() {
            &candidates
        } else {
            &exact
        },
        known,
        &format!("channel {name}"),
    )
}

fn emoji_image(emoji: &Map<String, Value>) -> Result<String, RestorePlanError> {
    let image = emoji.get("image").and_then(Value::as_str).unwrap_or("");
    // data:image/(png|gif|jpg|jpeg);base64,...
    let (mime, data) = image.split_once(";base64,").ok_or_else(|| {
        RestorePlanError(format!(
            "Snapshot emoji {} has no restorable image data URI.",
            emoji
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(str_field(emoji, "id"))
        ))
    })?;
    if !matches!(
        mime,
        "data:image/png" | "data:image/gif" | "data:image/jpg" | "data:image/jpeg"
    ) {
        return Err(RestorePlanError(format!(
            "Snapshot emoji {} has no restorable image data URI.",
            emoji
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(str_field(emoji, "id"))
        )));
    }
    if data.is_empty()
        || !data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return Err(RestorePlanError(format!(
            "Snapshot emoji {} has no restorable image data URI.",
            emoji
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(str_field(emoji, "id"))
        )));
    }
    Ok(image.to_owned())
}

fn literal_overwrite(o: &GuildOverwrite) -> Value {
    serde_json::json!({"id": o.id, "type": o.overwrite_type, "allow": o.allow, "deny": o.deny})
}

fn normalized_overwrites(overwrites: &[GuildOverwrite]) -> Value {
    let mut values: Vec<_> = overwrites.iter().map(literal_overwrite).collect();
    values.sort_by_key(super::guild_config::stable);
    Value::Array(values)
}

fn same_overwrites(left: &[GuildOverwrite], right: &[GuildOverwrite]) -> bool {
    same(&normalized_overwrites(left), &normalized_overwrites(right))
}

fn overwrites_differ(
    expected: &[GuildOverwrite],
    actual: Option<&Map<String, Value>>,
    roles: &BTreeMap<String, String>,
) -> bool {
    let mut mapped = Vec::new();
    for overwrite in expected {
        let mut overwrite = overwrite.clone();
        if overwrite.overwrite_type == 0 {
            let Some(id) = roles.get(&overwrite.id) else {
                // This role must be created earlier; placeholders belong only
                // in operations, never in the literal-live-ID comparison.
                return true;
            };
            overwrite.id.clone_from(id);
        }
        mapped.push(overwrite);
    }
    !same_overwrites(&mapped, &actual.map(overwrites_of).unwrap_or_default())
}

// Replay the complete operation graph without writes. Symbolic IDs become
// available only AFTER a supported create, so forward references also refuse.
fn validate_ordered_dependencies(plan: &RestorePlan) -> Result<RestoreIdMaps, RestorePlanError> {
    let mut available = plan.known_ids.clone();
    for op in &plan.operations {
        resolve_path(&op.path, &available.channels)?;
        resolve_value(&op.body, &available.roles, &available.channels)?;
        if let Some((resource, source)) = &op.capture_id {
            let (table, collection) = match resource.as_str() {
                "role" => (&mut available.roles, "roles"),
                "channel" => (&mut available.channels, "channels"),
                "emoji" => (&mut available.emojis, "emojis"),
                _ => {
                    return Err(RestorePlanError(format!(
                        "Unsupported restore create resource {resource}."
                    )));
                }
            };
            let supported_path = match &op.path {
                RestorePath::Literal(path) => {
                    let parts: Vec<_> = path.split('/').collect();
                    matches!(parts.as_slice(), ["", "guilds", guild, endpoint] if !guild.is_empty() && *endpoint == collection)
                }
                RestorePath::Channel(_) => false,
            };
            if op.method != "POST"
                || !supported_path
                || source.is_empty()
                || table.contains_key(source)
            {
                return Err(RestorePlanError(format!(
                    "Invalid or duplicate restore create for {resource} {source}."
                )));
            }
            table.insert(source.clone(), source.clone());
        }
    }
    Ok(available)
}

// Check even references in unchanged or managed resources, not just emitted
// operations: a matching literal string is not proof the dependency exists.
fn validate_snapshot_dependencies(
    snapshot: &Map<String, Value>,
    available: &RestoreIdMaps,
) -> Result<(), RestorePlanError> {
    let roles = snapshot_roles(snapshot);
    let channels = snapshot_channels(snapshot);
    let require_role = |id: &str| -> Result<(), RestorePlanError> {
        if !roles.iter().any(|r| str_field(r, "id") == id) {
            return Err(RestorePlanError(format!(
                "Snapshot references unknown role {id}."
            )));
        }
        if !available.roles.contains_key(id) {
            return Err(RestorePlanError(format!(
                "Restore dependency role {id} cannot be restored (missing managed role)."
            )));
        }
        Ok(())
    };
    let require_channel = |id: &str| -> Result<(), RestorePlanError> {
        if !channels.iter().any(|c| str_field(c, "id") == id) {
            return Err(RestorePlanError(format!(
                "Snapshot references unknown channel {id}."
            )));
        }
        if !available.channels.contains_key(id) {
            return Err(RestorePlanError(format!(
                "Restore dependency channel {id} cannot be restored."
            )));
        }
        Ok(())
    };
    for role in roles.iter().filter(|r| is_managed(r)) {
        require_role(str_field(role, "id"))?;
    }
    for channel in &channels {
        if let Some(id) = channel.get("parent_id").and_then(Value::as_str) {
            require_channel(id)?;
            if !channels.iter().any(|c| {
                str_field(c, "id") == id && c.get("type").and_then(Value::as_i64) == Some(4)
            }) {
                return Err(RestorePlanError(format!(
                    "Snapshot parent channel {id} is not a category."
                )));
            }
        }
        for overwrite in overwrites_of(channel) {
            if overwrite.overwrite_type == 0 {
                require_role(&overwrite.id)?;
            }
        }
    }
    for field in GUILD_CONFIG_FIELDS
        .iter()
        .filter(|f| f.ends_with("_channel_id"))
    {
        if let Some(id) = snapshot
            .get("guild")
            .and_then(|g| g.get(*field))
            .and_then(Value::as_str)
        {
            require_channel(id)?;
        }
    }
    for emoji in snapshot
        .get("emojis")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for id in emoji
            .get("roles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = id.as_str().ok_or_else(|| {
                RestorePlanError("Snapshot emoji has a non-string role reference.".to_owned())
            })?;
            require_role(id)?;
        }
    }
    Ok(())
}

/// Resolve `{"restoreReference","sourceId"}` placeholders against created ids.
pub fn resolve_value(
    value: &Value,
    roles: &BTreeMap<String, String>,
    channels: &BTreeMap<String, String>,
) -> Result<Value, RestorePlanError> {
    match value {
        Value::Array(items) => Ok(Value::Array(
            items
                .iter()
                .map(|i| resolve_value(i, roles, channels))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Object(map) => {
            if map.contains_key("restoreReference") || map.contains_key("sourceId") {
                let (Some(Value::String(resource)), Some(Value::String(source))) =
                    (map.get("restoreReference"), map.get("sourceId"))
                else {
                    return Err(RestorePlanError("Malformed restore reference.".to_owned()));
                };
                let table = match resource.as_str() {
                    "role" => roles,
                    "channel" => channels,
                    _ => {
                        return Err(RestorePlanError(format!(
                            "Unsupported restore reference resource {resource}."
                        )));
                    }
                };
                return table
                    .get(source)
                    .cloned()
                    .map(Value::String)
                    .ok_or_else(|| {
                        RestorePlanError(format!(
                            "Restore dependency {resource} {source} has not been created."
                        ))
                    });
            }
            Ok(Value::Object(
                map.iter()
                    .map(|(k, v)| Ok((k.clone(), resolve_value(v, roles, channels)?)))
                    .collect::<Result<Map<_, _>, RestorePlanError>>()?,
            ))
        }
        _ => Ok(value.clone()),
    }
}

/// Resolve an operation path against created channel ids.
pub fn resolve_path(
    path: &RestorePath,
    channels: &BTreeMap<String, String>,
) -> Result<String, RestorePlanError> {
    match path {
        RestorePath::Literal(literal) => Ok(literal.clone()),
        RestorePath::Channel(source) => channels
            .get(source)
            .map(|id| format!("/channels/{id}"))
            .ok_or_else(|| {
                RestorePlanError(format!(
                    "Restore dependency channel {source} has not been created."
                ))
            }),
    }
}

/// Execute a plan through the Discord API, resolving id references as
/// creates return their new Discord ids. Returns the full id map
/// (plan-known ids plus created ones) for [`remap_snapshot_ids`].
pub async fn apply_restore_plan(
    api: &mut super::guild_config_api::GuildConfigDiscordApi,
    plan: &RestorePlan,
) -> Result<RestoreIdMaps, RestorePlanError> {
    // Plans are public/mutable: defend the write boundary as well as planning.
    validate_ordered_dependencies(plan)?;
    let mut roles = plan.known_ids.roles.clone();
    let mut channels = plan.known_ids.channels.clone();
    let mut emojis = plan.known_ids.emojis.clone();
    for op in &plan.operations {
        let path = resolve_path(&op.path, &channels)?;
        let body = resolve_value(&op.body, &roles, &channels)?;
        let result = api
            .write(&op.method, &path, body)
            .await
            .map_err(|e| RestorePlanError(e.to_string()))?;
        if let Some((resource, source)) = &op.capture_id {
            let id = result
                .as_ref()
                .and_then(|b| b.get("id"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| RestorePlanError(format!("{} returned no Discord id.", op.label)))?;
            match resource.as_str() {
                "role" => {
                    roles.insert(source.clone(), id.to_owned());
                }
                "channel" => {
                    channels.insert(source.clone(), id.to_owned());
                }
                _ => {
                    emojis.insert(source.clone(), id.to_owned());
                }
            }
        }
    }
    Ok(RestoreIdMaps {
        roles,
        channels,
        emojis,
    })
}

/// Rewrite a snapshot into post-restore ids, so the caller can prove the
/// post-restore capture hashes equal to the (remapped) source.
#[must_use]
pub fn remap_snapshot_ids(
    snapshot: &Map<String, Value>,
    ids: &RestoreIdMaps,
) -> Map<String, Value> {
    let role_id = |source: &str| {
        ids.roles
            .get(source)
            .cloned()
            .unwrap_or_else(|| source.to_owned())
    };
    let channel_id = |source: &str| {
        ids.channels
            .get(source)
            .cloned()
            .unwrap_or_else(|| source.to_owned())
    };
    let emoji_id = |source: &str| {
        ids.emojis
            .get(source)
            .cloned()
            .unwrap_or_else(|| source.to_owned())
    };
    let mut out = snapshot.clone();
    if let Some(guild) = out.get_mut("guild").and_then(Value::as_object_mut) {
        for field in [
            "system_channel_id",
            "rules_channel_id",
            "public_updates_channel_id",
            "afk_channel_id",
        ] {
            if let Some(Value::String(source)) = guild.get(field).cloned() {
                guild.insert(field.to_owned(), Value::String(channel_id(&source)));
            }
        }
    }
    if let Some(roles) = out.get_mut("roles").and_then(Value::as_array_mut) {
        for role in roles.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(id) = role.get("id").and_then(Value::as_str).map(str::to_owned) {
                role.insert("id".to_owned(), Value::String(role_id(&id)));
            }
        }
    }
    if let Some(channels) = out.get_mut("channels").and_then(Value::as_array_mut) {
        for channel in channels.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(id) = channel.get("id").and_then(Value::as_str).map(str::to_owned) {
                channel.insert("id".to_owned(), Value::String(channel_id(&id)));
            }
            if let Some(Value::String(parent)) = channel.get("parent_id").cloned() {
                channel.insert("parent_id".to_owned(), Value::String(channel_id(&parent)));
            }
            if let Some(ows) = channel
                .get_mut("permission_overwrites")
                .and_then(Value::as_array_mut)
            {
                for ow in ows.iter_mut().filter_map(Value::as_object_mut) {
                    let ow_type = ow.get("type").and_then(Value::as_i64).unwrap_or(0);
                    if let Some(id) = ow.get("id").and_then(Value::as_str).map(str::to_owned) {
                        let mapped = if ow_type == 0 {
                            role_id(&id)
                        } else {
                            channel_id(&id)
                        };
                        ow.insert("id".to_owned(), Value::String(mapped));
                    }
                }
            }
        }
    }
    if let Some(emojis) = out.get_mut("emojis").and_then(Value::as_array_mut) {
        for emoji in emojis.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(id) = emoji.get("id").and_then(Value::as_str).map(str::to_owned) {
                emoji.insert("id".to_owned(), Value::String(emoji_id(&id)));
            }
            if let Some(roles) = emoji.get_mut("roles").and_then(Value::as_array_mut) {
                for role in roles.iter_mut() {
                    if let Some(id) = role.as_str().map(str::to_owned) {
                        *role = Value::String(role_id(&id));
                    }
                }
            }
        }
    }
    out
}

/// Hash equality: post-restore capture vs remapped source.
#[must_use]
pub fn snapshots_equal(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    config_hash(&canonical_snapshot(left)) == config_hash(&canonical_snapshot(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::guild_config::{
        seal_snapshot, LIVE_GUILD_ID, STAGING_BOT_APPLICATION_ID, TWO_STAGING_GUILD_ID,
    };

    fn snapshot() -> Map<String, Value> {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "generatedAt": "2026-09-24T00:00:00.000Z",
            "applicationId": STAGING_BOT_APPLICATION_ID,
            "guildId": TWO_STAGING_GUILD_ID,
            // A real Discord guild object always carries every documented
            // field (nulls included); the planner compares key-for-key, so
            // the fixture must too.
            "guild": {
                "name": "TWO Staging",
                "description": "d",
                "verification_level": 2,
                "default_message_notifications": 0,
                "explicit_content_filter": 2,
                "afk_timeout": 300,
                "preferred_locale": "en-US",
                "premium_progress_bar_enabled": false,
                "system_channel_flags": 0,
                "system_channel_id": "ch1",
                "rules_channel_id": null,
                "public_updates_channel_id": null,
                "afk_channel_id": null
            },
            "roles": [
                {"id": TWO_STAGING_GUILD_ID, "name": "@everyone", "managed": false, "color": 0, "hoist": false, "permissions": "0", "mentionable": false, "position": 0},
                {"id": "r-mod", "name": "Moderator", "managed": false, "color": 0, "hoist": true, "permissions": "8", "mentionable": false, "position": 1}
            ],
            "channels": [
                {"id": "cat1", "name": "COMMUNITY", "type": 4, "parent_id": null, "position": 0, "permission_overwrites": []},
                {"id": "ch1", "name": "general", "type": 0, "parent_id": "cat1", "position": 0,
                 "topic": null, "nsfw": false,
                 "permission_overwrites": [{"id": TWO_STAGING_GUILD_ID, "type": 0, "allow": "1024", "deny": "0"}]}
            ],
            "emojis": [],
        }))
        .unwrap()
    }

    fn live_like_snapshot() -> Map<String, Value> {
        // Same ids, one drifted channel name + one drifted role colour.
        let mut live = snapshot();
        live["channels"].as_array_mut().unwrap()[1]["name"] =
            Value::String("general-renamed".to_owned());
        live["roles"].as_array_mut().unwrap()[1]["color"] = Value::from(1);
        live.remove("integrity");
        live
    }

    #[test]
    fn plan_is_empty_for_identical_snapshots() {
        let snap = seal_snapshot(snapshot());
        let plan = plan_restore(&snap, &snap).expect("identical");
        for op in &plan.operations {
            eprintln!("UNEXPECTED OP: {} {} {}", op.label, op.method, op.body);
        }
        assert_eq!(plan.counts.operations, 0);
    }

    #[test]
    fn rename_matches_by_id_not_by_name_no_duplicate_create() {
        // TOG-3513 live finding: a renamed channel matches by Discord id, so
        // the plan patches (not creates) it.
        let snap = seal_snapshot(snapshot());
        let plan = plan_restore(&snap, &live_like_snapshot()).expect("plan");
        assert!(
            plan.operations
                .iter()
                .all(|op| !op.label.starts_with("create channel")),
            "must not duplicate the renamed channel: {:?}",
            plan.operations.iter().map(|o| &o.label).collect::<Vec<_>>()
        );
        assert!(
            plan.operations
                .iter()
                .any(|op| op.label == "patch channel general"),
            "renamed channel is patched back: {:?}",
            plan.operations.iter().map(|o| &o.label).collect::<Vec<_>>()
        );
        assert!(plan
            .operations
            .iter()
            .any(|op| op.label == "patch role Moderator"));
    }

    #[test]
    fn guild_mismatch_is_refused_before_anything_else() {
        let snap = seal_snapshot(snapshot());
        let mut other = snap.clone();
        other["guildId"] = Value::String(LIVE_GUILD_ID.to_owned());
        let err = plan_restore(&snap, &other).expect_err("guild mismatch");
        assert!(err.0.contains("does not match target guild"), "{err}");
    }

    #[test]
    fn duplicate_live_channels_are_ambiguous_not_merged() {
        let snap = seal_snapshot(snapshot());
        // Two live channels with the same name+parent (different Discord ids):
        // the restore target is ambiguous and the plan is refused.
        let mut live = snapshot();
        live.remove("integrity");
        live["channels"].as_array_mut().unwrap()[1]["id"] =
            Value::String("ch1-replacement".to_owned());
        let mut dup = live["channels"].as_array().unwrap()[1].clone();
        dup["id"] = Value::String("ch1-dup".to_owned());
        live["channels"].as_array_mut().unwrap().push(dup);
        let err = plan_restore(&snap, &live).expect_err("ambiguous");
        assert!(err.0.contains("ambiguous"), "{err}");
    }

    #[test]
    fn unknown_role_reference_is_refused() {
        let mut snap = snapshot();
        snap["channels"].as_array_mut().unwrap()[1]["permission_overwrites"] =
            serde_json::json!([{"id": "r-ghost", "type": 0, "allow": "0", "deny": "0"}]);
        let sealed = seal_snapshot(snap);
        let err = plan_restore(&sealed, &live_like_snapshot()).expect_err("ghost role");
        assert!(err.0.contains("unknown role r-ghost"), "{err}");
    }

    #[test]
    fn remap_rewrites_ids_and_hash_proves_convergence() {
        let snap = seal_snapshot(snapshot());
        let ids = RestoreIdMaps {
            roles: [("r-mod".to_owned(), "r-new".to_owned())].into(),
            channels: [
                ("cat1".to_owned(), "cat9".to_owned()),
                ("ch1".to_owned(), "ch9".to_owned()),
            ]
            .into(),
            emojis: BTreeMap::new(),
        };
        let remapped = remap_snapshot_ids(&snap, &ids);
        assert_eq!(remapped["roles"].as_array().unwrap()[1]["id"], "r-new");
        assert_eq!(remapped["guild"]["system_channel_id"], "ch9");
        // Hash of remapped == hash of a capture that already converged.
        assert!(snapshots_equal(&remapped, &remapped));
        assert!(!snapshots_equal(&snap, &remapped));
    }
}
