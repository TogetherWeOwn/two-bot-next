//! Fresh, fail-closed onboarding destination checks through the shared executor.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;

use crate::ActionExecutor;

const ADMINISTRATOR: u64 = 1 << 3;
const VIEW_CHANNEL: u64 = 1 << 10;
const SEND_MESSAGES: u64 = 1 << 11;

fn snowflake(value: &Value) -> Option<u64> {
    value.as_str()?.parse().ok().filter(|id| *id != 0)
}

/// No usable REST evidence was obtained. This is not a proven permission denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessUnavailable;

/// One live member/role snapshot. Build another after role writes; never infer
/// permissions from the old interaction payload or only @everyone's overwrite.
#[derive(Debug)]
pub struct MemberAccess {
    guild_id: u64,
    user_id: u64,
    owner: bool,
    permissions: u64,
    pub role_ids: Vec<String>,
}

impl MemberAccess {
    fn from_json(
        guild_id: u64,
        user_id: u64,
        guild: &Value,
        roles: &Value,
        member: &Value,
    ) -> Option<Self> {
        if snowflake(&guild["id"])? != guild_id || snowflake(&member["user"]["id"])? != user_id {
            return None;
        }
        let owner = snowflake(&guild["owner_id"])? == user_id;
        let mut role_permissions = HashMap::new();
        for role in roles.as_array()? {
            let id = snowflake(&role["id"])?;
            let permissions = role["permissions"].as_str()?.parse::<u64>().ok()?;
            if role_permissions.insert(id, permissions).is_some() {
                return None;
            }
        }
        let mut permissions = *role_permissions.get(&guild_id)?;
        let mut role_ids = Vec::new();
        for role in member["roles"].as_array()? {
            let id = snowflake(role)?;
            permissions |= role_permissions.get(&id)?;
            role_ids.push(id.to_string());
        }
        Some(Self {
            guild_id,
            user_id,
            owner,
            permissions,
            role_ids,
        })
    }

    /// A fresh REST read, bounded as a group so a rate-limit loop cannot hold
    /// the feature worker indefinitely. Missing/malformed data never grants access.
    pub async fn load(
        executor: &ActionExecutor,
        guild_id: u64,
        user_id: u64,
    ) -> Result<Option<Self>, AccessUnavailable> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut evidence = Vec::new();
            for path in [
                format!("/guilds/{guild_id}"),
                format!("/guilds/{guild_id}/roles"),
                format!("/guilds/{guild_id}/members/{user_id}"),
            ] {
                let Some(value) = executor
                    .get_json_checked(&path)
                    .await
                    .map_err(|_| AccessUnavailable)?
                else {
                    return Ok(None);
                };
                evidence.push(value);
            }
            Ok(Self::from_json(
                guild_id,
                user_id,
                &evidence[0],
                &evidence[1],
                &evidence[2],
            ))
        })
        .await
        .map_err(|_| AccessUnavailable)?
    }

    fn channel_permissions(&self, channel: &Value) -> Option<u64> {
        if snowflake(&channel["guild_id"])? != self.guild_id {
            return None;
        }
        // Guild forum/media visibility uses the same overwrites. Whether the
        // executor can post a plain message is a separate check in permits.
        // DMs and threads still require different membership evidence.
        if !matches!(channel["type"].as_u64()?, 0 | 2 | 5 | 13 | 15 | 16) {
            return None;
        }
        let mut everyone = (0, 0);
        let mut roles = (0, 0);
        let mut member = (0, 0);
        let role_ids: HashSet<_> = self.role_ids.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        for overwrite in channel["permission_overwrites"].as_array()? {
            let id = snowflake(&overwrite["id"])?;
            let kind = overwrite["type"].as_u64()?;
            if kind > 1 || !seen.insert((kind, id)) {
                return None;
            }
            let allow = overwrite["allow"].as_str()?.parse::<u64>().ok()?;
            let deny = overwrite["deny"].as_str()?.parse::<u64>().ok()?;
            if kind == 0 && id == self.guild_id {
                everyone = (allow, deny);
            } else if kind == 0 && role_ids.contains(id.to_string().as_str()) {
                roles.0 |= allow;
                roles.1 |= deny;
            } else if kind == 1 && id == self.user_id {
                member = (allow, deny);
            }
        }
        if self.owner || self.permissions & ADMINISTRATOR != 0 {
            return Some(u64::MAX);
        }
        let mut permissions = self.permissions;
        for (allow, deny) in [everyone, roles, member] {
            permissions = (permissions & !deny) | allow;
        }
        Some(permissions)
    }

    /// Validate the requested identity too: a proxy/cache returning another
    /// channel must not make a configured destination appear safe.
    pub async fn permits(
        &self,
        executor: &ActionExecutor,
        channel_id: &str,
        post: bool,
    ) -> Result<bool, AccessUnavailable> {
        let Some(expected) = channel_id.parse::<u64>().ok().filter(|id| *id != 0) else {
            return Ok(false);
        };
        let channel = tokio::time::timeout(
            Duration::from_secs(5),
            executor.get_json_checked(&format!("/channels/{expected}")),
        )
        .await
        .map_err(|_| AccessUnavailable)?
        .map_err(|_| AccessUnavailable)?;
        let Some(channel) = channel else {
            return Ok(false);
        };
        if snowflake(&channel["id"]) != Some(expected)
            || (post && !matches!(channel["type"].as_u64(), Some(0 | 2 | 5 | 13)))
        {
            return Ok(false);
        }
        let needed = VIEW_CHANNEL | if post { SEND_MESSAGES } else { 0 };
        Ok(self
            .channel_permissions(&channel)
            .is_some_and(|bits| bits & needed == needed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn access(roles: &[&str]) -> MemberAccess {
        MemberAccess::from_json(
            22,
            44,
            &json!({"id":"22","owner_id":"99"}),
            &json!([{"id":"22","permissions":"3072"},{"id":"55","permissions":"0"}]),
            &json!({"user":{"id":"44"},"roles":roles}),
        )
        .unwrap()
    }

    #[test]
    fn member_overwrite_wins_combined_role_and_everyone_overwrites() {
        let mut channel = json!({"id":"12","guild_id":"22","type":0,"permission_overwrites":[
            {"id":"22","type":0,"allow":"0","deny":"1024"},
            {"id":"55","type":0,"allow":"1024","deny":"0"}
        ]});
        assert_eq!(
            access(&[]).channel_permissions(&channel),
            Some(SEND_MESSAGES)
        );
        assert_eq!(access(&["55"]).channel_permissions(&channel), Some(3072));
        channel["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"44","type":1,"allow":"0","deny":"1024"}));
        assert_eq!(
            access(&["55"]).channel_permissions(&channel),
            Some(SEND_MESSAGES)
        );
    }

    #[test]
    fn malformed_foreign_and_dm_channels_fail_closed() {
        for channel in [
            json!({"guild_id":"23","type":0,"permission_overwrites":[]}),
            json!({"guild_id":"22","type":1,"permission_overwrites":[]}),
            json!({"guild_id":"22","type":0}),
            json!({"guild_id":"22","type":0,"permission_overwrites":[{"id":"22","type":0,"allow":"oops","deny":"0"}]}),
        ] {
            assert_eq!(access(&[]).channel_permissions(&channel), None);
        }
    }

    #[test]
    fn missing_role_or_wrong_member_is_not_permission_evidence() {
        assert!(MemberAccess::from_json(
            22,
            44,
            &json!({"id":"22","owner_id":"99"}),
            &json!([{"id":"22","permissions":"3072"}]),
            &json!({"user":{"id":"44"},"roles":["55"]})
        )
        .is_none());
        assert!(MemberAccess::from_json(
            22,
            44,
            &json!({"id":"22","owner_id":"99"}),
            &json!([{"id":"22","permissions":"3072"}]),
            &json!({"user":{"id":"45"},"roles":[]})
        )
        .is_none());
    }
}
