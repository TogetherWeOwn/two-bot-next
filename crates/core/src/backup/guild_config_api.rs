//! Discord transport for guild-config snapshot/restore.
//!
//! Port of legacy `src/discord/guildConfigApi.ts`: guild/roles/channels/
//! emojis reads, CDN emoji fetch, identity + restore-permission preflights,
//! and the counted `write()` used by the restore planner's apply path.
//!
//! `api_base` / `cdn_base` are test seams (legacy `GUILD_CONFIG_API_BASE` /
//! `GUILD_CONFIG_CDN_BASE`): they accept loopback URLs only, so tests drive
//! a fake Discord in-process while production can only ever point at
//! `discord.com` / `cdn.discordapp.com`. `GUILD_CONFIG_API_BASE` is honoured
//! for the same reason legacy honoured it — a staging run against a mock is
//! how the restore path is rehearsed without touching Discord.

use serde_json::{Map, Value};
use thiserror::Error;

use super::guild_config_restore::RestorePlan;
use super::http::{self, HttpError, HttpMethod};

/// API transport failure. Discord error bodies surface truncated.
#[derive(Debug, Error)]
pub enum GuildConfigApiError {
    #[error("not a URL: {0:?}")]
    BadBase(String),
    #[error("{0} is a test seam and only accepts loopback. Got host {1:?}.")]
    NonLoopbackBase(String, String),
    #[error("http: {0}")]
    Http(#[from] HttpError),
    #[error("{0}")]
    Discord(String),
}

/// Checked test-seam base: loopback only. Production default otherwise.
pub fn checked_base(
    raw: Option<&str>,
    name: &str,
    production: &str,
) -> Result<String, GuildConfigApiError> {
    let Some(raw) = raw else {
        return Ok(production.to_owned());
    };
    let raw = raw.trim_end_matches('/');
    let host = raw
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if host != "127.0.0.1" && host != "localhost" && host != "::1" {
        // Strip brackets for IPv6 display.
        let shown = host.trim_start_matches('[').trim_end_matches(']');
        let _ = shown;
        return Err(GuildConfigApiError::NonLoopbackBase(
            name.to_owned(),
            host.to_owned(),
        ));
    }
    if !raw.starts_with("https://") && !raw.starts_with("http://") {
        return Err(GuildConfigApiError::BadBase(raw.to_owned()));
    }
    Ok(raw.to_owned())
}

/// Discord REST/CDN client for one guild. Counts writes for restore evidence.
#[derive(Debug)]
pub struct GuildConfigDiscordApi {
    pub api_base: String,
    pub cdn_base: String,
    pub token: String,
    pub application_id: String,
    pub guild_id: String,
    pub writes: u64,
    pub timeout_secs: u64,
}

impl GuildConfigDiscordApi {
    pub fn new(
        api_base: Option<&str>,
        cdn_base: Option<&str>,
        token: String,
        application_id: String,
        guild_id: String,
    ) -> Result<Self, GuildConfigApiError> {
        Ok(Self {
            api_base: checked_base(
                api_base,
                "GUILD_CONFIG_API_BASE",
                "https://discord.com/api/v10",
            )?,
            cdn_base: checked_base(
                cdn_base,
                "GUILD_CONFIG_CDN_BASE",
                "https://cdn.discordapp.com",
            )?,
            token,
            application_id,
            guild_id,
            writes: 0,
            timeout_secs: 30,
        })
    }

    fn auth_header(&self) -> (String, String) {
        ("authorization".to_owned(), format!("Bot {}", self.token))
    }

    /// GET with Discord 429 handling: honour `retry_after` (capped at 30 s),
    /// up to 5 attempts — port of the legacy `request()` retry loop.
    pub async fn request_json(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Option<Value>), GuildConfigApiError> {
        let url = format!("{}{}", self.api_base, path);
        let method = method
            .parse::<HttpMethod>()
            .map_err(|_| GuildConfigApiError::Discord(format!("bad method {method:?}")))?;
        for _ in 0..5 {
            let mut headers = vec![
                self.auth_header(),
                ("content-type".to_owned(), "application/json".to_owned()),
            ];
            if body.is_none() {
                headers.pop();
            }
            let payload = body.clone().map(|b| b.to_string().into_bytes());
            let res =
                http::request(method.clone(), &url, headers, payload, self.timeout_secs).await?;
            let status = res.status.as_u16();
            if status != 429 {
                return Ok((status, res.json()));
            }
            let retry_after = res
                .json()
                .as_ref()
                .and_then(|b| b.get("retry_after"))
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            if !retry_after.is_finite() || retry_after < 0.0 {
                return Ok((status, res.json()));
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(retry_after.min(30.0))).await;
        }
        Ok((429, None))
    }

    /// The token really is the staging application, in the staging guild.
    /// Runs before any capture or write.
    pub async fn assert_identity(&self) -> Result<(), GuildConfigApiError> {
        let (status, body) = self.request_json("GET", "/users/@me", None).await?;
        let id = body
            .as_ref()
            .and_then(|b| b.get("id"))
            .and_then(Value::as_str);
        if status != 200 || id != Some(self.application_id.as_str()) {
            return Err(GuildConfigApiError::Discord(format!(
                "Discord did not authenticate as expected application {}: HTTP {status}.",
                self.application_id
            )));
        }
        let (status, body) = self.request_json("GET", "/users/@me/guilds", None).await?;
        let inside = body
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|guilds| {
                guilds
                    .iter()
                    .any(|g| g.get("id").and_then(Value::as_str) == Some(self.guild_id.as_str()))
            });
        if status != 200 || !inside {
            return Err(GuildConfigApiError::Discord(format!(
                "Application {} is not in guild {}.",
                self.application_id, self.guild_id
            )));
        }
        Ok(())
    }

    /// Restore permission preflight: the bot must hold the Discord
    /// permissions the plan needs (Manage Guild / Channels / Roles /
    /// Guild Expressions as applicable), sit above overwritten roles, and
    /// own every permission bit the desired overwrites grant. Administrator
    /// bypasses the checks, as on Discord.
    pub async fn assert_restore_permissions(
        &self,
        snapshot: &Map<String, Value>,
        plan: &RestorePlan,
    ) -> Result<(), GuildConfigApiError> {
        let (status, body) = self
            .request_json(
                "GET",
                &format!("/guilds/{}/members/{}", self.guild_id, self.application_id),
                None,
            )
            .await?;
        if status != 200 || body.is_none() {
            return Err(GuildConfigApiError::Discord(format!(
                "Could not read Owen's guild member for permission preflight: HTTP {status}."
            )));
        }
        let member = body.unwrap_or(Value::Null);
        let held: Vec<String> = member
            .get("roles")
            .and_then(Value::as_array)
            .map(|roles| {
                roles
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        let roles = snapshot
            .get("roles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut held_ids: Vec<&str> = vec![self.guild_id.as_str()];
        held_ids.extend(held.iter().map(String::as_str));
        let held_roles: Vec<&Value> = roles
            .iter()
            .filter(|r| {
                r.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| held_ids.contains(&id))
            })
            .collect();
        let permissions: u128 = held_roles
            .iter()
            .map(|r| {
                r.get("permissions")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u128>().ok())
                    .unwrap_or(0)
            })
            .fold(0, |a, b| a | b);
        let administrator = permissions & (1 << 3) != 0;
        let needs_manage_roles = plan.counts.roles > 0 || plan.counts.overwrites > 0;
        let mut required: Vec<(&str, u128)> = Vec::new();
        if plan.counts.settings > 0 {
            required.push(("Manage Guild", 1 << 5));
        }
        if plan.counts.channels > 0 || plan.counts.overwrites > 0 {
            required.push(("Manage Channels", 1 << 4));
        }
        if needs_manage_roles {
            required.push(("Manage Roles", 1 << 28));
        }
        if plan.counts.emojis > 0 {
            required.push(("Manage Guild Expressions", 1 << 30));
        }
        let missing: Vec<&str> = if administrator {
            vec![]
        } else {
            required
                .iter()
                .filter(|(_, bit)| permissions & bit == 0)
                .map(|(name, _)| *name)
                .collect()
        };
        if !missing.is_empty() {
            return Err(GuildConfigApiError::Discord(format!(
                "Restore permission preflight failed: missing {}.",
                missing.join(", ")
            )));
        }
        let owner_id = snapshot
            .get("guild")
            .and_then(|g| g.get("owner_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if needs_manage_roles && owner_id != self.application_id {
            let bot_position: i64 = held_roles
                .iter()
                .filter_map(|r| r.get("position").and_then(Value::as_i64))
                .max()
                .unwrap_or(-1);
            let targets: Vec<&Value> = if plan.counts.roles > 0 {
                roles
                    .iter()
                    .filter(|r| {
                        !r.get("managed").and_then(Value::as_bool).unwrap_or(false)
                            && r.get("id").and_then(Value::as_str) != Some(self.guild_id.as_str())
                    })
                    .collect()
            } else {
                // Overwrite-only plans: resolve role details from the snapshot.
                plan.overwrite_roles
                    .iter()
                    .filter_map(|t| {
                        roles
                            .iter()
                            .find(|r| r.get("id").and_then(Value::as_str) == Some(t.0.as_str()))
                    })
                    .collect()
            };
            let blocked: Vec<String> = targets
                .iter()
                .filter(|r| r.get("position").and_then(Value::as_i64).unwrap_or(0) >= bot_position)
                .map(|r| {
                    format!(
                        "{} ({})",
                        r.get("name").and_then(Value::as_str).unwrap_or("?"),
                        r.get("position").and_then(Value::as_i64).unwrap_or(0)
                    )
                })
                .collect();
            if !blocked.is_empty() {
                return Err(GuildConfigApiError::Discord(format!(
                    "Restore hierarchy preflight failed: Owen role position {bot_position} is not above overwrite target {}.",
                    blocked.join(", ")
                )));
            }
        }
        if !administrator {
            let mut blocked_targets = Vec::new();
            for target in &plan.overwrite_targets {
                let ceiling = effective_permissions(
                    permissions,
                    &held_ids,
                    &self.guild_id,
                    &self.application_id,
                    &target.permission_ceiling_overwrites,
                );
                let action = effective_permissions(
                    permissions,
                    &held_ids,
                    &self.guild_id,
                    &self.application_id,
                    &target.action_permission_overwrites,
                );
                let mut missing_channel = Vec::new();
                if action & (1 << 4) == 0 {
                    missing_channel.push("Manage Channels");
                }
                if action & (1 << 28) == 0 {
                    missing_channel.push("Manage Roles");
                }
                let requested: u128 = target
                    .desired_overwrites
                    .iter()
                    .map(|o| {
                        o.allow.parse::<u128>().unwrap_or(0) | o.deny.parse::<u128>().unwrap_or(0)
                    })
                    .fold(0, |a, b| a | b);
                let unowned = requested & !ceiling;
                if !missing_channel.is_empty() || unowned != 0 {
                    blocked_targets.push(format!(
                        "{} (missing {}; unowned mask {unowned})",
                        target.name,
                        if missing_channel.is_empty() {
                            "none".to_owned()
                        } else {
                            missing_channel.join(", ")
                        }
                    ));
                }
            }
            if !blocked_targets.is_empty() {
                return Err(GuildConfigApiError::Discord(format!(
                    "Restore channel permission preflight failed: {}.",
                    blocked_targets.join("; ")
                )));
            }
        }
        Ok(())
    }

    /// Download an unmanaged emoji's image from the CDN as a data URI.
    /// Honours `GUILD_CONFIG_CDN_BASE`; managed (external) emoji and nameless
    /// entries carry no image.
    pub async fn capture_emoji_image(
        &self,
        emoji: &Map<String, Value>,
    ) -> Result<Option<String>, GuildConfigApiError> {
        let managed = emoji
            .get("managed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let name = emoji.get("name").and_then(Value::as_str);
        let id = emoji.get("id").and_then(Value::as_str).unwrap_or("");
        if managed || name.is_none() {
            return Ok(None);
        }
        let animated = emoji
            .get("animated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let extension = if animated { "gif" } else { "png" };
        let url = format!("{}/emojis/{id}.{extension}", self.cdn_base);
        let res = http::get(&url, vec![], 30).await?;
        if res.status.as_u16() != 200 {
            return Err(GuildConfigApiError::Discord(format!(
                "Could not download emoji {}: HTTP {}.",
                name.unwrap_or("?"),
                res.status.as_u16()
            )));
        }
        let content_type = res
            .header("content-type")
            .and_then(|ct| ct.split(';').next())
            .unwrap_or(if animated { "image/gif" } else { "image/png" })
            .to_owned();
        if !content_type.starts_with("image/") {
            return Err(GuildConfigApiError::Discord(format!(
                "Emoji {} returned non-image content type {content_type}.",
                name.unwrap_or("?")
            )));
        }
        Ok(Some(format!(
            "data:{content_type};base64,{}",
            base64_encode(&res.body)
        )))
    }

    /// Capture the full guild config: guild, roles, channels, emojis (+ CDN images).
    pub async fn capture(&self) -> Result<Map<String, Value>, GuildConfigApiError> {
        let guild_path = format!("/guilds/{}", self.guild_id);
        let roles_path = format!("{guild_path}/roles");
        let channels_path = format!("{guild_path}/channels");
        let emojis_path = format!("{guild_path}/emojis");
        let (guild, roles, channels, emojis) = tokio::join!(
            self.request_json("GET", &guild_path, None),
            self.request_json("GET", &roles_path, None),
            self.request_json("GET", &channels_path, None),
            self.request_json("GET", &emojis_path, None),
        );
        let (status, guild) = guild?;
        if status != 200 || guild.is_none() {
            return Err(GuildConfigApiError::Discord(format!(
                "Could not read guild {}: HTTP {status}.",
                self.guild_id
            )));
        }
        let (status, roles) = roles?;
        if status != 200 || roles.is_none() {
            return Err(GuildConfigApiError::Discord(format!(
                "Could not read guild roles: HTTP {status}."
            )));
        }
        let (status, channels) = channels?;
        if status != 200 || channels.is_none() {
            return Err(GuildConfigApiError::Discord(format!(
                "Could not read guild channels: HTTP {status}."
            )));
        }
        let (status, emojis) = emojis?;
        if status != 200 || emojis.is_none() {
            return Err(GuildConfigApiError::Discord(format!(
                "Could not read guild emojis: HTTP {status}."
            )));
        }
        let mut captured = Vec::new();
        for emoji in emojis
            .unwrap_or(Value::Null)
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let mut obj = emoji.as_object().cloned().unwrap_or_default();
            let image = self.capture_emoji_image(&obj).await?;
            obj.insert(
                "image".to_owned(),
                image.map(Value::String).unwrap_or(Value::Null),
            );
            captured.push(Value::Object(obj));
        }
        let mut snapshot = Map::new();
        snapshot.insert("version".to_owned(), Value::from(1));
        snapshot.insert(
            "generatedAt".to_owned(),
            Value::String(super::guild_config::unix_now_iso()),
        );
        snapshot.insert(
            "applicationId".to_owned(),
            Value::String(self.application_id.clone()),
        );
        snapshot.insert("guildId".to_owned(), Value::String(self.guild_id.clone()));
        snapshot.insert("guild".to_owned(), guild.unwrap_or(Value::Null));
        snapshot.insert("roles".to_owned(), roles.unwrap_or(Value::Null));
        snapshot.insert("channels".to_owned(), channels.unwrap_or(Value::Null));
        snapshot.insert("emojis".to_owned(), Value::Array(captured));
        Ok(snapshot)
    }

    /// One counted Discord write. Counts for restore evidence.
    pub async fn write(
        &mut self,
        method: &str,
        path: &str,
        body: Value,
    ) -> Result<Option<Value>, GuildConfigApiError> {
        let (status, response) = self.request_json(method, path, Some(body)).await?;
        if !(200..300).contains(&status) {
            return Err(GuildConfigApiError::Discord(format!(
                "Discord write {method} {path} failed: HTTP {status} {}",
                response.map(|b| b.to_string()).unwrap_or_default()
            )));
        }
        self.writes += 1;
        Ok(response)
    }
}

fn effective_permissions(
    base: u128,
    held_ids: &[&str],
    guild_id: &str,
    application_id: &str,
    overwrites: &[super::guild_config_restore::GuildOverwrite],
) -> u128 {
    let mut effective = base;
    if let Some(everyone) = overwrites
        .iter()
        .find(|o| o.overwrite_type == 0 && o.id == guild_id)
    {
        let allow = everyone.allow.parse::<u128>().unwrap_or(0);
        let deny = everyone.deny.parse::<u128>().unwrap_or(0);
        effective = (effective & !deny) | allow;
    }
    let mut role_allow = 0u128;
    let mut role_deny = 0u128;
    for o in overwrites
        .iter()
        .filter(|o| o.overwrite_type == 0 && o.id != guild_id && held_ids.contains(&o.id.as_str()))
    {
        role_allow |= o.allow.parse::<u128>().unwrap_or(0);
        role_deny |= o.deny.parse::<u128>().unwrap_or(0);
    }
    effective = (effective & !role_deny) | role_allow;
    if let Some(member) = overwrites
        .iter()
        .find(|o| o.overwrite_type == 1 && o.id == application_id)
    {
        let allow = member.allow.parse::<u128>().unwrap_or(0);
        let deny = member.deny.parse::<u128>().unwrap_or(0);
        effective = (effective & !deny) | allow;
    }
    effective
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bases_default_to_discord_and_pin_tests_to_loopback() {
        let api =
            GuildConfigDiscordApi::new(None, None, "t".to_owned(), "a".to_owned(), "g".to_owned())
                .unwrap();
        assert_eq!(api.api_base, "https://discord.com/api/v10");
        assert_eq!(api.cdn_base, "https://cdn.discordapp.com");
        GuildConfigDiscordApi::new(
            Some("http://127.0.0.1:9"),
            Some("http://localhost:9"),
            "t".to_owned(),
            "a".to_owned(),
            "g".to_owned(),
        )
        .expect("loopback seams allowed");
        let err = GuildConfigDiscordApi::new(
            Some("https://evil.example.com"),
            None,
            "t".to_owned(),
            "a".to_owned(),
            "g".to_owned(),
        )
        .expect_err("non-loopback api base refused");
        assert!(
            matches!(err, GuildConfigApiError::NonLoopbackBase(_, _)),
            "{err}"
        );
    }
}
