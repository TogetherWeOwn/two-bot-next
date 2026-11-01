//! Executor snapshot + roles reads for `/lockdown-preview`
//! ([`docs/lockdown-preview-spec.md`](../../../../docs/lockdown-preview-spec.md),
//! option B, build slice 3).
//!
//! The preview command (later slice) needs, once each: the invoking channel's
//! document with fully validated `permission_overwrites`, the guild role list,
//! and the saved lockdown record from the store. This module adds the two
//! Discord reads; the saved record already has
//! `ChannelModerationStore::get_lockdown`. The pure planner in
//! `two_bot_core::lockdown_preview` owns the completeness gates (250 roles,
//! 100 overwrites, `@everyone` presence, duplicate refusal) and the digest.
//!
//! Both reads are single paced attempts with no retry, fail closed as
//! [`DiscordError::Rejected`] on any malformed row (mirroring
//! `read_everyone_overwrite`: an unreadable document must refuse, never read
//! as "absent"), and leave transport errors untouched. No command, component,
//! pending store, audit row or rate limiter lives here.

use serde_json::Value;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker},
    Id,
};

use two_bot_core::lockdown_preview::{OverwriteSnapshot, RoleSnapshot};

use super::{snowflake, ActionExecutor, DiscordError};

/// Validated channel identity plus every overwrite row, exactly as read.
/// The `@everyone` row is the `kind == 0, id == guild_id` entry when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewChannel {
    pub channel_type: u64,
    pub parent_id: Option<u64>,
    pub overwrites: Vec<OverwriteSnapshot>,
}

fn rejected(context: &str) -> DiscordError {
    DiscordError::Rejected(format!("unreadable lockdown preview snapshot: {context}"))
}

fn decimal_bits(value: &Value, field: &str) -> Result<u64, DiscordError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| rejected(&format!("non-numeric {field} mask")))
}

/// Strict snowflake: canonical decimal string, nonzero, fits `u64`.
/// Rejects empty, non-numeric, non-canonical (`"02222"`), zero and
/// overflowing ids, like the `@everyone` read the mutation path depends on.
fn snapshot_id<T>(value: &Value) -> Result<u64, DiscordError> {
    let text = value
        .as_str()
        .ok_or_else(|| rejected("non-string snowflake"))?;
    let id: Id<T> = snowflake(text).map_err(|_| rejected("invalid snowflake"))?;
    if text != id.get().to_string() {
        return Err(rejected("non-canonical snowflake"));
    }
    Ok(id.get())
}

/// Validate one already-fetched channel document against the caller the
/// preview was invoked with. Pure so tests pin it without a socket.
fn parse_preview_channel(
    channel_id: &str,
    guild_id: &str,
    doc: &Value,
) -> Result<PreviewChannel, DiscordError> {
    let channel: Id<ChannelMarker> =
        snowflake(channel_id).map_err(|_| rejected("invalid channel id"))?;
    let target: Id<GuildMarker> = snowflake(guild_id).map_err(|_| rejected("invalid guild id"))?;
    let doc = doc
        .as_object()
        .ok_or_else(|| rejected("body is not an object"))?;
    let doc_channel: Id<ChannelMarker> = doc
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| rejected("missing channel id"))
        .and_then(|id| snowflake(id).map_err(|_| rejected("invalid channel id")))?;
    let doc_guild: Id<GuildMarker> = doc
        .get("guild_id")
        .and_then(Value::as_str)
        .ok_or_else(|| rejected("missing guild id"))
        .and_then(|id| snowflake(id).map_err(|_| rejected("invalid guild id")))?;
    if doc_channel != channel || doc_guild != target {
        return Err(rejected("channel is not in the invoking guild"));
    }
    let channel_type = doc.get("type").and_then(Value::as_u64).ok_or_else(|| {
        DiscordError::Rejected(
            "lockdown preview supports text and announcement channels only".to_owned(),
        )
    })?;
    if !matches!(channel_type, 0 | 5) {
        return Err(DiscordError::Rejected(
            "lockdown preview supports text and announcement channels only".to_owned(),
        ));
    }
    let parent_id = match doc.get("parent_id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(snapshot_id::<ChannelMarker>(value)?),
    };
    let overwrites = doc
        .get("permission_overwrites")
        .and_then(Value::as_array)
        .ok_or_else(|| rejected("missing permission_overwrites"))?;
    let mut rows = Vec::with_capacity(overwrites.len());
    for (index, entry) in overwrites.iter().enumerate() {
        let row = entry
            .as_object()
            .ok_or_else(|| rejected(&format!("permission_overwrites[{index}] is not an object")))?;
        let kind = row
            .get("type")
            .and_then(Value::as_u64)
            .filter(|kind| *kind <= 1)
            .ok_or_else(|| {
                rejected(&format!(
                    "permission_overwrites[{index}] has a non-role/member type"
                ))
            })?;
        // Every row carries a readable identity before it can count as "not
        // @everyone": a malformed row must refuse, not silently drop a deny.
        // Member-overwrite ids are user snowflakes; `GuildMarker` is only the
        // phantom the shared validator needs, as in `read_everyone_overwrite`.
        let id = row
            .get("id")
            .ok_or_else(|| rejected(&format!("permission_overwrites[{index}] has no id")))
            .and_then(|value| {
                snapshot_id::<GuildMarker>(value).map_err(|_| {
                    rejected(&format!("permission_overwrites[{index}] has an invalid id"))
                })
            })?;
        let allow = decimal_bits(entry, "allow").map_err(|_| {
            rejected(&format!(
                "permission_overwrites[{index}] has a non-numeric allow mask"
            ))
        })?;
        let deny = decimal_bits(entry, "deny").map_err(|_| {
            rejected(&format!(
                "permission_overwrites[{index}] has a non-numeric deny mask"
            ))
        })?;
        rows.push(OverwriteSnapshot {
            kind: kind as u8,
            id,
            allow,
            deny,
        });
    }
    Ok(PreviewChannel {
        channel_type,
        parent_id,
        overwrites: rows,
    })
}

/// Validate one already-fetched guild role list. Pure so tests pin it
/// without a socket. Duplicate and `@everyone`-presence gates belong to the
/// pure planner, which sees every row returned here.
fn parse_preview_roles(doc: &Value) -> Result<Vec<RoleSnapshot>, DiscordError> {
    let roles = doc
        .as_array()
        .ok_or_else(|| rejected("role list is not an array"))?;
    roles
        .iter()
        .enumerate()
        .map(|(index, role)| {
            let row = role
                .as_object()
                .ok_or_else(|| rejected(&format!("roles[{index}] is not an object")))?;
            let id = row
                .get("id")
                .ok_or_else(|| rejected(&format!("roles[{index}] has no id")))
                .and_then(snapshot_id::<GuildMarker>)?;
            let position = row
                .get("position")
                .and_then(Value::as_i64)
                .ok_or_else(|| rejected(&format!("roles[{index}] has a non-numeric position")))?;
            let permissions = row
                .get("permissions")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|text| text.parse::<u64>().ok())
                .ok_or_else(|| rejected(&format!("roles[{index}] has non-numeric permissions")))?;
            Ok(RoleSnapshot {
                id,
                position,
                permissions,
            })
        })
        .collect()
}

impl ActionExecutor {
    /// One paced channel GET (`GET /channels/{c}`), strictly validated.
    /// Spec section 3 step 2: at most one channel read per preview; any
    /// failed or malformed read refuses the whole preview.
    pub async fn read_preview_channel(
        &self,
        channel_id: &str,
        guild_id: &str,
    ) -> Result<PreviewChannel, DiscordError> {
        let channel: Id<ChannelMarker> =
            snowflake(channel_id).map_err(|_| rejected("invalid channel id"))?;
        let req = Self::request_of(self.inner.factory.channel(channel))?;
        let res = self.call_once_raw_paced(req, &[200]).await?;
        let doc: Value =
            serde_json::from_slice(&res.body).map_err(|_| rejected("channel body is not JSON"))?;
        parse_preview_channel(channel_id, guild_id, &doc)
    }

    /// One paced guild-roles GET (`GET /guilds/{g}/roles`), strictly
    /// validated. Spec section 3 step 2: at most one roles read per preview.
    /// The executor has no guild-roles read today; the only roles consumer
    /// is online preflight, which fails closed on an incomplete snapshot —
    /// this read does the same on any malformed row.
    pub async fn read_preview_roles(
        &self,
        guild_id: &str,
    ) -> Result<Vec<RoleSnapshot>, DiscordError> {
        let guild: Id<GuildMarker> =
            snowflake(guild_id).map_err(|_| rejected("invalid guild id"))?;
        let req = Self::request_of(self.inner.factory.roles(guild))?;
        let res = self.call_once_raw_paced(req, &[200]).await?;
        let doc: Value = serde_json::from_slice(&res.body)
            .map_err(|_| rejected("role list body is not JSON"))?;
        parse_preview_roles(&doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_wire::{MockRest, ScriptedResponse};
    use serde_json::json;

    const GUILD: &str = "2222";
    const CHANNEL: &str = "3333";

    fn channel_doc() -> Value {
        json!({
            "id": CHANNEL,
            "guild_id": GUILD,
            "type": 0,
            "parent_id": "4444",
            "permission_overwrites": [
                {"id": GUILD, "type": 0, "allow": "0", "deny": "2048"},
                {"id": "5555", "type": 0, "allow": "2048", "deny": "0"},
                {"id": "6666", "type": 1, "allow": "274877906944", "deny": "0"}
            ]
        })
    }

    fn role_list() -> Value {
        json!([
            {"id": GUILD, "position": 0, "permissions": "104324673"},
            {"id": "5555", "position": 3, "permissions": "8"},
            {"id": "7777", "position": 1, "permissions": "0"}
        ])
    }

    fn executor_for(mock: &MockRest) -> ActionExecutor {
        ActionExecutor::with_proxy("preview-reads-fixture".to_owned(), Some(mock.origin()))
            .expect("fixture executor")
    }

    #[test]
    fn channel_parse_keeps_every_overwrite_row() {
        let parsed = parse_preview_channel(CHANNEL, GUILD, &channel_doc()).expect("valid doc");
        assert_eq!(parsed.channel_type, 0);
        assert_eq!(parsed.parent_id, Some(4444));
        assert_eq!(
            parsed.overwrites,
            vec![
                OverwriteSnapshot {
                    kind: 0,
                    id: 2222,
                    allow: 0,
                    deny: 2048
                },
                OverwriteSnapshot {
                    kind: 0,
                    id: 5555,
                    allow: 2048,
                    deny: 0
                },
                OverwriteSnapshot {
                    kind: 1,
                    id: 6666,
                    allow: 274877906944,
                    deny: 0
                },
            ]
        );
    }

    #[test]
    fn channel_parse_accepts_announcement_without_parent() {
        let mut doc = channel_doc();
        doc["type"] = json!(5);
        doc.as_object_mut().expect("object").remove("parent_id");
        let parsed = parse_preview_channel(CHANNEL, GUILD, &doc).expect("valid doc");
        assert_eq!(parsed.channel_type, 5);
        assert_eq!(parsed.parent_id, None);
    }

    #[test]
    fn channel_parse_refuses_foreign_guild_wrong_type_and_malformed_rows() {
        let mut foreign = channel_doc();
        foreign["guild_id"] = json!("9999");
        assert!(parse_preview_channel(CHANNEL, GUILD, &foreign).is_err());

        for broken_type in [1, 2, 4, 10] {
            let mut doc = channel_doc();
            doc["type"] = json!(broken_type);
            assert!(
                parse_preview_channel(CHANNEL, GUILD, &doc).is_err(),
                "type {broken_type} must refuse"
            );
        }

        let mut missing_overwrites = channel_doc();
        missing_overwrites
            .as_object_mut()
            .expect("object")
            .remove("permission_overwrites");
        assert!(parse_preview_channel(CHANNEL, GUILD, &missing_overwrites).is_err());

        for broken in [
            json!({"id": GUILD, "type": 2, "allow": "0", "deny": "0"}),
            json!({"id": GUILD, "type": 0, "allow": "nope", "deny": "0"}),
            json!({"id": "0", "type": 0, "allow": "0", "deny": "0"}),
            json!({"id": "02222", "type": 0, "allow": "0", "deny": "0"}),
            json!({"id": GUILD, "allow": "0", "deny": "0"}),
        ] {
            let mut doc = channel_doc();
            doc["permission_overwrites"] = json!([broken]);
            assert!(
                parse_preview_channel(CHANNEL, GUILD, &doc).is_err(),
                "row {broken} must refuse"
            );
        }
    }

    #[test]
    fn roles_parse_keeps_positions_and_permissions() {
        let parsed = parse_preview_roles(&role_list()).expect("valid roles");
        assert_eq!(
            parsed,
            vec![
                RoleSnapshot {
                    id: 2222,
                    position: 0,
                    permissions: 104324673
                },
                RoleSnapshot {
                    id: 5555,
                    position: 3,
                    permissions: 8
                },
                RoleSnapshot {
                    id: 7777,
                    position: 1,
                    permissions: 0
                },
            ]
        );
    }

    #[test]
    fn roles_parse_refuses_non_array_and_malformed_rows() {
        assert!(parse_preview_roles(&json!({})).is_err());
        for broken in [
            json!([{"id": "5555", "position": 3}]),
            json!([{"id": "5555", "position": "3", "permissions": "0"}]),
            json!([{"id": "5555", "position": 3, "permissions": "nope"}]),
            json!([{"id": "0", "position": 3, "permissions": "0"}]),
        ] {
            assert!(
                parse_preview_roles(&broken).is_err(),
                "row {broken} must refuse"
            );
        }
    }

    #[tokio::test]
    async fn reads_cost_one_get_each() {
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(200, channel_doc()),
                ScriptedResponse::json(200, role_list()),
            ],
            ScriptedResponse::status(500),
        )
        .await;
        let executor = executor_for(&mock);
        let channel = executor
            .read_preview_channel(CHANNEL, GUILD)
            .await
            .expect("channel read");
        assert_eq!(channel.overwrites.len(), 3);
        let roles = executor
            .read_preview_roles(GUILD)
            .await
            .expect("roles read");
        assert_eq!(roles.len(), 3);
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, format!("/api/v10/channels/{CHANNEL}"));
        assert_eq!(requests[1].path, format!("/api/v10/guilds/{GUILD}/roles"));
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_wire_body_refuses_without_fallback() {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(
                200,
                json!({"id": CHANNEL, "guild_id": GUILD, "type": 0}),
            )],
            ScriptedResponse::status(500),
        )
        .await;
        let executor = executor_for(&mock);
        let error = executor
            .read_preview_channel(CHANNEL, GUILD)
            .await
            .expect_err("missing overwrites must refuse");
        assert!(matches!(error, DiscordError::Rejected(_)), "{error:?}");
        mock.shutdown().await;
    }
}
