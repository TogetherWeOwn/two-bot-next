//! Auto-role and verify share a serialized, live-evidence grant path.

#[cfg(test)]
mod builder_tests;

use super::*;
use serde::Deserialize;
use serde_json::json;
use twilight_model::application::interaction::{InteractionData, InteractionType};
use twilight_model::channel::message::component::ComponentType;
use two_bot_core::audit::{AuditEvent, AuditKind};
use two_bot_core::audit_store::AuditStore;
use two_bot_core::onboarding_roles::{
    RoleConfig, JOIN_ROLES_KEY, MEMBER_ROLE_KEY, VERIFY_CUSTOM_ID,
};

#[derive(Deserialize)]
struct GrantRole {
    id: String,
    permissions: String,
    position: i64,
    managed: bool,
}

struct GrantPolicy {
    roles: Vec<GrantRole>,
    bot_roles: HashSet<String>,
    highest: i64,
    can_manage: bool,
}

impl GrantPolicy {
    fn refusal(&self, guild: &str, id: &str) -> Option<&'static str> {
        let Some(role) = self.roles.iter().find(|role| role.id == id) else {
            return Some("skipped:missing_role");
        };
        if id == guild {
            Some("skipped:everyone")
        } else if role.managed {
            Some("skipped:managed")
        } else if self.bot_roles.contains(id) {
            Some("skipped:bot_role")
        } else if role.position >= self.highest {
            Some("skipped:above_bot")
        } else if !self.can_manage {
            Some("refused:manage_roles")
        } else if role
            .permissions
            .parse::<u64>()
            .ok()
            .is_none_or(|bits| two_bot_core::self_roles::find_disallowed_permission(bits).is_some())
        {
            Some("refused:unsafe_permissions")
        } else {
            None
        }
    }
}

/// The shared command runtime owns defer; this handler owns fresh authorization,
/// settings and the public panel. Picker visibility is not authorization.
pub(crate) async fn admin_command(
    pool: &PgPool,
    executor: &ActionExecutor,
    guild_id: u64,
    interaction: &Interaction,
) -> Result<(), RuntimeError> {
    if interaction
        .guild_id
        .is_none_or(|guild| guild.get() != guild_id)
    {
        return Ok(());
    }
    let identity = executor
        .get_json_strict("/users/@me")
        .await
        .map_err(|_| RuntimeError::Identity)?
        .ok_or(RuntimeError::Identity)?;
    if identity["bot"].as_bool() != Some(true) {
        return Err(RuntimeError::Identity);
    }
    let bot_id = identity["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .ok_or(RuntimeError::Identity)?;
    let vars = CONFIG_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_owned(), value))
        })
        .collect();
    let runtime = OnboardingRuntime::new(pool.clone(), executor.clone(), &vars, guild_id, bot_id)?;
    runtime.configure(interaction).await
}

impl OnboardingRuntime {
    async fn configure(&self, interaction: &Interaction) -> Result<(), RuntimeError> {
        use twilight_model::application::interaction::application_command::CommandOptionValue as OptionValue;
        use two_bot_core::onboarding_roles::valid_setting;
        let Some(actor) = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .filter(|user| !user.bot)
        else {
            return self
                .reply(interaction, "Only server members can configure onboarding.")
                .await;
        };
        let FreshRecipient::Eligible { roles: actor_roles } = self
            .current_recipient(self.guild_id, actor.id.get())
            .await?
        else {
            return self
                .reply(
                    interaction,
                    "You need the Manage Server permission to configure onboarding.",
                )
                .await;
        };
        let policy = self.role_policy().await?;
        let guild = self.guild_id.to_string();
        let guild_doc = self
            .executor
            .get_json_strict(&format!("/guilds/{guild}"))
            .await
            .map_err(|_| RuntimeError::Discord)?
            .ok_or(RuntimeError::Member)?;
        if guild_doc["id"].as_str() != Some(guild.as_str()) {
            return Err(RuntimeError::Member);
        }
        let mut permissions = 0u64;
        for id in std::iter::once(&guild).chain(actor_roles.iter()) {
            let role = policy
                .roles
                .iter()
                .find(|role| &role.id == id)
                .ok_or(RuntimeError::Role)?;
            permissions |= role
                .permissions
                .parse::<u64>()
                .map_err(|_| RuntimeError::Role)?;
        }
        if guild_doc["owner_id"].as_str() != Some(actor.id.to_string().as_str())
            && permissions & ((1 << 5) | (1 << 3)) == 0
        {
            return self
                .reply(
                    interaction,
                    "You need the Manage Server permission to configure onboarding.",
                )
                .await;
        }
        let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
            return Err(RuntimeError::Config);
        };
        if data.name != "onboarding" || data.options.len() != 1 {
            return Err(RuntimeError::Config);
        }
        let command = &data.options[0];
        let OptionValue::SubCommand(options) = &command.value else {
            return Err(RuntimeError::Config);
        };
        let change = match (command.name.as_str(), options.as_slice()) {
            ("join-roles", [option]) if option.name == "roles" => {
                let OptionValue::String(raw) = &option.value else {
                    return Err(RuntimeError::Config);
                };
                if raw.len() > 209 {
                    return Err(RuntimeError::Config);
                }
                let value = json!(raw.split(',').collect::<Vec<_>>());
                Some((JOIN_ROLES_KEY, Some(value)))
            }
            ("member-role", [option]) if option.name == "role" => {
                let OptionValue::Role(role) = &option.value else {
                    return Err(RuntimeError::Config);
                };
                Some((MEMBER_ROLE_KEY, Some(json!(role.to_string()))))
            }
            ("clear-join", []) => Some((JOIN_ROLES_KEY, None)),
            ("clear-member", []) => Some((MEMBER_ROLE_KEY, None)),
            ("panel", []) => None,
            _ => {
                return self
                    .reply(
                        interaction,
                        "Use /onboarding to choose a supported configuration action.",
                    )
                    .await
            }
        };
        if let Some((key, value)) = change {
            if let Some(value) = &value {
                if !valid_setting(&guild, key, value) {
                    return self.reply(interaction, "Use up to 10 distinct role ids, with no spaces, leading zeros or @everyone.").await;
                }
                let roles: Vec<_> = match value {
                    serde_json::Value::Array(roles) => {
                        roles.iter().filter_map(serde_json::Value::as_str).collect()
                    }
                    _ => value.as_str().into_iter().collect(),
                };
                if roles
                    .iter()
                    .any(|role| policy.refusal(&guild, role).is_some())
                {
                    return self.reply(interaction, "Choose ordinary roles below my highest role. I need Manage Roles; managed, bot and privileged roles cannot be used.").await;
                }
            }
            let clear = value.is_none();
            SettingsStore::new(&self.pool)
                .set(&guild, key, value, &format!("discord:{}", actor.id))
                .await
                .map_err(|_| RuntimeError::Settings)?;
            return self.reply(interaction, if clear { "Stored onboarding override cleared. Deployment configuration applies again." } else { "Onboarding roles saved. Existing members keep their roles." }).await;
        }
        let config = self.role_config().await?;
        let Some(role) = config.member else {
            return self
                .reply(
                    interaction,
                    "Set a member role with /onboarding member-role before posting the panel.",
                )
                .await;
        };
        if policy.refusal(&guild, &role).is_some() {
            return self.reply(interaction, "The member role is no longer grantable. Choose a safe role below my highest role.").await;
        }
        let channel = interaction
            .channel
            .as_ref()
            .ok_or(RuntimeError::Config)?
            .id
            .to_string();
        for member in [actor.id.get(), self.bot_id] {
            let access = MemberAccess::load(&self.executor, self.guild_id, member)
                .await
                .map_err(|_| RuntimeError::Member)?
                .ok_or(RuntimeError::Member)?;
            if !access
                .permits(&self.executor, &channel, true)
                .await
                .map_err(|_| RuntimeError::Member)?
            {
                return self
                    .reply(
                        interaction,
                        "Choose a channel that both you and I can view and send messages in.",
                    )
                    .await;
            }
        }
        self.executor.post_message_with_components(
            &channel,
            "Welcome! Press Verify to receive the member role.",
            &[json!({"type": 1, "components": [{"type": 2, "style": 3, "label": "Verify", "custom_id": VERIFY_CUSTOM_ID}]})],
            &interaction.id.to_string(),
        ).await.map_err(|_| RuntimeError::Discord)?;
        self.reply(interaction, "Verification panel posted.").await
    }
}

/// Strict button subtype, not a modal/select with the same custom id.
fn verify_button(interaction: &Interaction) -> bool {
    interaction.kind == InteractionType::MessageComponent
        && matches!(interaction.data.as_ref(), Some(InteractionData::MessageComponent(data))
            if data.custom_id == VERIFY_CUSTOM_ID && data.component_type == ComponentType::Button)
}

impl OnboardingRuntime {
    pub(super) fn accepts_verify(&self, interaction: &Interaction) -> bool {
        interaction
            .guild_id
            .is_some_and(|guild| guild.get() == self.guild_id)
            && verify_button(interaction)
            && interaction
                .member
                .as_ref()
                .and_then(|member| member.user.as_ref())
                .is_some_and(|user| !user.bot)
    }

    async fn role_config(&self) -> Result<RoleConfig, RuntimeError> {
        let snapshot = tokio::time::timeout(
            Duration::from_millis(1500),
            SettingsStore::new(&self.pool).load_snapshot(),
        )
        .await
        .map_err(|_| RuntimeError::Settings)?
        .map_err(|_| RuntimeError::Settings)?;
        let cache = SettingsCache::load(&snapshot);
        let guild = self.guild_id.to_string();
        Ok(RoleConfig::load(
            &guild,
            &self.deployment,
            cache.get(&guild, JOIN_ROLES_KEY),
            cache.get(&guild, MEMBER_ROLE_KEY),
        ))
    }

    async fn role_policy(&self) -> Result<GrantPolicy, RuntimeError> {
        let guild = self.guild_id.to_string();
        let body = self
            .executor
            .get_json_strict(&format!("/guilds/{guild}/roles"))
            .await
            .map_err(|_| RuntimeError::Discord)?
            .ok_or(RuntimeError::Role)?;
        let roles: Vec<GrantRole> = serde_json::from_value(body).map_err(|_| RuntimeError::Role)?;
        let mut seen = HashSet::new();
        if roles.is_empty()
            || roles.iter().any(|role| {
                role.id
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|id| id == 0 || id.to_string() != role.id)
                    || !seen.insert(role.id.as_str())
                    || role.permissions.parse::<u64>().is_err()
                    || role.position < 0
            })
            || !seen.contains(guild.as_str())
        {
            return Err(RuntimeError::Role);
        }
        let bot = self
            .executor
            .get_json_strict(&format!("/guilds/{guild}/members/{}", self.bot_id))
            .await
            .map_err(|_| RuntimeError::Discord)?
            .ok_or(RuntimeError::Role)?;
        if bot["user"]["id"].as_str() != Some(self.bot_id.to_string().as_str())
            || bot["user"]["bot"].as_bool() != Some(true)
        {
            return Err(RuntimeError::Role);
        }
        let bot_roles: HashSet<String> = bot["roles"]
            .as_array()
            .ok_or(RuntimeError::Role)?
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or(RuntimeError::Role))
            .collect::<Result<_, _>>()?;
        if bot_roles.iter().any(|id| !seen.contains(id.as_str())) {
            return Err(RuntimeError::Role);
        }
        let held: Vec<_> = roles
            .iter()
            .filter(|role| role.id == guild || bot_roles.contains(&role.id))
            .collect();
        let highest = held
            .iter()
            .map(|role| role.position)
            .max()
            .ok_or(RuntimeError::Role)?;
        let permissions = held
            .iter()
            .try_fold(0u64, |mask, role| {
                role.permissions.parse::<u64>().map(|bits| mask | bits)
            })
            .map_err(|_| RuntimeError::Role)?;
        Ok(GrantPolicy {
            roles,
            bot_roles,
            highest,
            can_manage: permissions & ((1 << 28) | (1 << 3)) != 0,
        })
    }

    async fn audit_role(
        &self,
        member: u64,
        role: Option<&str>,
        action: &str,
        source: &str,
        at: &str,
    ) -> Result<(), RuntimeError> {
        let mut event = AuditEvent::new(
            format!(
                "onboarding:{}:{member}:{source}:{at}:{}",
                self.guild_id,
                role.unwrap_or("none")
            ),
            AuditKind::OnboardingRoleGrant,
            self.guild_id.to_string(),
            at.to_owned(),
        );
        event.actor_id = Some(self.bot_id.to_string());
        event.target_id = Some(member.to_string());
        event.action = Some(action.to_owned());
        event.metadata_json = role.map_or_else(
            || "{}".to_owned(),
            |role| json!({"role_id": role}).to_string(),
        );
        AuditStore::new(&self.pool)
            .record(&event, None)
            .await
            .map_err(|_| RuntimeError::Store)?;
        Ok(())
    }

    /// A durable attempt must exist before Discord can change. If finalization
    /// fails, its attempted cause remains honest evidence of an uncertain grant.
    async fn settle_role_audit(
        &self,
        member: u64,
        role: &str,
        action: &str,
        source: &str,
        at: &str,
    ) -> Result<(), RuntimeError> {
        let entry = format!("onboarding:{}:{member}:{source}:{at}:{role}", self.guild_id);
        let result = sqlx::query("UPDATE operational_audit_log SET action = $2 WHERE entry_id = $1 AND event_kind = 'onboarding_role_grant'")
            .bind(entry).bind(action).execute(&self.pool).await.map_err(|_| RuntimeError::Store)?;
        if result.rows_affected() != 1 {
            return Err(RuntimeError::Store);
        }
        Ok(())
    }

    pub(super) async fn join_roles(
        &self,
        config: &OnboardingConfig,
        member: u64,
        bot: bool,
        pending: bool,
        at: &str,
    ) -> Result<(), RuntimeError> {
        let roles = self.role_config().await?;
        if roles.invalid_join {
            return self
                .audit_role(member, None, "refused:invalid_config", "join", at)
                .await;
        }
        if roles.join.is_empty() {
            return Ok(());
        }
        let refusal = if bot {
            Some("skipped:bot_member")
        } else if pending {
            Some("skipped:pending")
        } else if config.gates.dry_run {
            Some("skipped:dry_run")
        } else if config.gates.mode == OnboardingMode::Session {
            Some("skipped:session_mode")
        } else {
            None
        };
        if let Some(cause) = refusal {
            return self.audit_role(member, None, cause, "join", at).await;
        }
        self.grant_roles(member, &roles.join, false, "join", at)
            .await?;
        Ok(())
    }

    /// Returns true only for a newly granted verify role. Join ignores the value.
    async fn grant_roles(
        &self,
        member: u64,
        roles: &[String],
        verify: bool,
        source: &str,
        at: &str,
    ) -> Result<bool, RuntimeError> {
        let guild = self.guild_id.to_string();
        let mut lock = self.pool.begin().await.map_err(|_| RuntimeError::Store)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{guild}:{member}:onboarding_roles"))
            .execute(&mut *lock)
            .await
            .map_err(|_| RuntimeError::Store)?;
        let current = self.current_recipient(self.guild_id, member).await;
        let held = match current {
            Ok(FreshRecipient::Eligible { roles }) => roles,
            Ok(FreshRecipient::Ineligible) => {
                self.audit_role(member, None, "refused:ineligible_member", source, at)
                    .await?;
                return if verify {
                    Err(RuntimeError::Member)
                } else {
                    Ok(false)
                };
            }
            Err(error) => {
                self.audit_role(member, None, "refused:member_unavailable", source, at)
                    .await?;
                return Err(error);
            }
        };
        let policy = match self.role_policy().await {
            Ok(policy) => policy,
            Err(error) => {
                self.audit_role(member, None, "refused:policy_unavailable", source, at)
                    .await?;
                return Err(error);
            }
        };
        let mut granted = false;
        let mut refused = false;
        for role in roles {
            let cause = if let Some(cause) = policy.refusal(&guild, role) {
                refused = true;
                cause
            } else if held.contains(role) {
                if verify {
                    "verified:already"
                } else {
                    "skipped:already_held"
                }
            } else {
                let reason = if verify {
                    "onboarding verify gate: member verified"
                } else {
                    "onboarding auto-role: granted on join"
                };
                self.audit_role(member, Some(role), "attempted:grant", source, at)
                    .await?;
                self.settle_role_audit(member, role, "attempted:grant", source, at)
                    .await?;
                if self
                    .executor
                    .set_member_role(&guild, &member.to_string(), role, true, reason)
                    .await
                    .is_err()
                {
                    self.settle_role_audit(member, role, "failed:discord_unconfirmed", source, at)
                        .await?;
                    return Err(RuntimeError::Discord);
                }
                granted = true;
                self.settle_role_audit(
                    member,
                    role,
                    if verify {
                        "granted:verify"
                    } else {
                        "granted:join"
                    },
                    source,
                    at,
                )
                .await?;
                continue;
            };
            self.audit_role(member, Some(role), cause, source, at)
                .await?;
        }
        lock.commit().await.map_err(|_| RuntimeError::Store)?;
        if verify && refused {
            Err(RuntimeError::Role)
        } else {
            Ok(granted)
        }
    }

    pub(super) async fn verify(
        &self,
        config: &OnboardingConfig,
        interaction: &Interaction,
        at: &str,
    ) -> Result<(), RuntimeError> {
        if !self.accepts_verify(interaction) {
            return Ok(());
        }
        let Some(user) = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
        else {
            return Ok(());
        };
        let member = user.id.get();
        let source = interaction.id.to_string();
        let roles = self.role_config().await?;
        let Some(role) = roles.member else {
            self.audit_role(
                member,
                None,
                if roles.invalid_member {
                    "refused:invalid_config"
                } else {
                    "refused:unconfigured"
                },
                &source,
                at,
            )
            .await?;
            return self
                .reply(interaction, "Verification isn't set up on this server yet.")
                .await;
        };
        if config.gates.dry_run || config.gates.mode == OnboardingMode::Session {
            self.audit_role(
                member,
                Some(&role),
                "refused:role_writes_disabled",
                &source,
                at,
            )
            .await?;
            return self
                .reply(
                    interaction,
                    "Verification is paused on this server. Please try again later.",
                )
                .await;
        }
        let result = self.grant_roles(member, &[role], true, &source, at).await;
        let reply = match result {
            Ok(true) => "You are now verified. Welcome!",
            Ok(false) => "You are already verified.",
            Err(_) => "I couldn't confirm verification. Please try again or ask a server admin.",
        };
        self.reply(interaction, reply).await
    }
}
