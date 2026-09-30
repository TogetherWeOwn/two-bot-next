//! Onboarding orchestration; S3 retains ownership of join/gate/leave facts.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use twilight_model::gateway::event::Event;
use two_bot_core::onboarding::*;
use two_bot_core::onboarding_store::*;
use two_bot_core::settings::SettingsCache;
use two_bot_core::{ComponentHandler, ComponentOutcome, InteractionRouter, RouterGates};
use two_bot_cutover::settings::SettingsStore;
use two_bot_discord::onboarding_config::OnboardingConfig;
use two_bot_discord::onboarding_messages::{allowed_mentions, defer_ephemeral, picker_components};
use two_bot_discord::onboarding_permissions::MemberAccess;
use two_bot_discord::{route_interaction, ActionExecutor, RoutedInteraction};

use crate::gateway::GatewayPipeline;

const CONFIG_KEYS: &[&str] = &[
    "DISCORD_GUILD_ID",
    "TWO_ONBOARDING_MODE",
    "TWO_ONBOARDING_DRY_RUN",
    "DISCORD_LANDING_CHANNEL_IDS",
    "DISCORD_GOODBYE_CHANNEL_IDS",
    "DISCORD_ANCHOR_WELCOME_CHANNEL_ID",
    "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID",
    "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID",
];

/// Values own pre-pipeline state: cache updates/removals cannot erase the
/// pending transition or joined-at used by the asynchronous feature worker.
#[derive(Debug)]
pub enum OnboardingJob {
    Welcome {
        guild_id: u64,
        member_id: u64,
        bot: bool,
        pending: bool,
        trigger: MembershipTrigger,
        roles: Vec<String>,
    },
    Goodbye {
        guild_id: u64,
        username: String,
        bot: bool,
        joined_at_ms: Option<i64>,
    },
    Interaction(Box<Interaction>),
}

#[derive(Debug)]
pub enum RuntimeError {
    Config,
    Settings,
    Identity,
    Member,
    Discord,
    Store,
}

pub struct OnboardingRuntime {
    pool: PgPool,
    executor: ActionExecutor,
    deployment: HashMap<String, String>,
    guild_id: u64,
    bot_id: u64,
}

impl OnboardingRuntime {
    pub async fn from_env(
        pool: PgPool,
        executor: ActionExecutor,
        guild_id: u64,
    ) -> Result<Self, RuntimeError> {
        let vars = CONFIG_KEYS
            .iter()
            .filter_map(|key| {
                std::env::var(key)
                    .ok()
                    .map(|value| ((*key).to_owned(), value))
            })
            .collect();
        let identity =
            tokio::time::timeout(Duration::from_secs(5), executor.get_json("/users/@me"))
                .await
                .map_err(|_| RuntimeError::Identity)?
                .map_err(|_| RuntimeError::Identity)?
                .ok_or(RuntimeError::Identity)?;
        let bot_id = identity["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .ok_or(RuntimeError::Identity)?;
        if identity["bot"].as_bool() != Some(true) {
            return Err(RuntimeError::Identity);
        }
        Self::new(pool, executor, &vars, guild_id, bot_id)
    }

    /// GUILD_ID is the gateway identity. A conflicting legacy alias fails
    /// closed; never permit onboarding in a different guild from its shard.
    pub fn new(
        pool: PgPool,
        executor: ActionExecutor,
        vars: &HashMap<String, String>,
        guild_id: u64,
        bot_id: u64,
    ) -> Result<Self, RuntimeError> {
        let mut deployment: HashMap<String, String> = CONFIG_KEYS
            .iter()
            .filter_map(|key| {
                vars.get(*key)
                    .map(|value| ((*key).to_owned(), value.clone()))
            })
            .collect();
        if let Some(alias) = deployment.get("DISCORD_GUILD_ID") {
            if !alias.trim().is_empty() && alias.trim().parse::<u64>().ok() != Some(guild_id) {
                return Err(RuntimeError::Config);
            }
        }
        if guild_id == 0 || bot_id == 0 {
            return Err(RuntimeError::Identity);
        }
        deployment.insert("DISCORD_GUILD_ID".into(), guild_id.to_string());
        OnboardingConfig::from_map(&deployment).map_err(|_| RuntimeError::Config)?;
        Ok(Self {
            pool,
            executor,
            deployment,
            guild_id,
            bot_id,
        })
    }

    /// Refresh on each relevant event from a consistent MVCC snapshot. Hot
    /// store overrides win; deleted rows immediately fall back to deployment.
    /// Env-only mode and guild identity never come from stored rows.
    async fn config(&self) -> Result<OnboardingConfig, RuntimeError> {
        let snapshot = tokio::time::timeout(
            Duration::from_millis(1500),
            SettingsStore::new(&self.pool).load_snapshot(),
        )
        .await
        .map_err(|_| RuntimeError::Settings)?
        .map_err(|_| RuntimeError::Settings)?;
        let cache = SettingsCache::load(&snapshot);
        let stored = cache.env_snapshot(Some(&self.guild_id.to_string()));
        let mut vars = self.deployment.clone();
        for key in CONFIG_KEYS
            .iter()
            .filter(|key| !matches!(**key, "DISCORD_GUILD_ID" | "TWO_ONBOARDING_MODE"))
        {
            if let Some(value) = stored.get(*key) {
                vars.insert((*key).to_owned(), value.clone());
            }
        }
        OnboardingConfig::from_map(&vars)
            .map_err(|_| RuntimeError::Config)?
            .ok_or(RuntimeError::Config)
    }

    pub fn capture(&self, event: &Event, pipeline: &GatewayPipeline) -> Option<OnboardingJob> {
        match event {
            Event::MemberAdd(member) if member.guild_id.get() == self.guild_id => {
                Some(OnboardingJob::Welcome {
                    guild_id: member.guild_id.get(),
                    member_id: member.user.id.get(),
                    bot: member.user.bot,
                    pending: member.pending,
                    trigger: MembershipTrigger::Joined {
                        pending: member.pending,
                    },
                    roles: member.roles.iter().map(ToString::to_string).collect(),
                })
            }
            Event::MemberUpdate(member) if member.guild_id.get() == self.guild_id => {
                let was_pending = pipeline
                    .cache()
                    .member(member.guild_id, member.user.id)
                    .is_some_and(|old| old.pending());
                (was_pending && !member.pending).then(|| OnboardingJob::Welcome {
                    guild_id: member.guild_id.get(),
                    member_id: member.user.id.get(),
                    bot: member.user.bot,
                    pending: member.pending,
                    trigger: MembershipTrigger::GateCleared,
                    roles: member.roles.iter().map(ToString::to_string).collect(),
                })
            }
            Event::MemberRemove(member) if member.guild_id.get() == self.guild_id => {
                Some(OnboardingJob::Goodbye {
                    guild_id: member.guild_id.get(),
                    username: member.user.name.clone(),
                    bot: member.user.bot,
                    joined_at_ms: pipeline
                        .cache()
                        .member(member.guild_id, member.user.id)
                        .and_then(|old| old.joined_at())
                        .map(|at| at.as_micros() / 1000),
                })
            }
            Event::InteractionCreate(interaction)
                if interaction
                    .guild_id
                    .is_some_and(|guild| guild.get() == self.guild_id) =>
            {
                Some(OnboardingJob::Interaction(Box::new(interaction.0.clone())))
            }
            _ => None,
        }
    }

    pub async fn handle(&self, job: OnboardingJob, now_ms: i64) -> Result<(), RuntimeError> {
        let config = self.config().await?;
        let occurred_at = two_bot_core::funnel::format_iso_millis(now_ms);
        match job {
            OnboardingJob::Welcome {
                guild_id,
                member_id,
                bot,
                pending,
                trigger,
                roles,
            } => {
                if guild_id != config.guild_id
                    || !welcome_trigger(trigger)
                    || decide_prompt(bot, pending, false) != PromptDecision::Prompt
                {
                    return Ok(());
                }
                self.welcome(&config, member_id, &roles, now_ms / 1000, &occurred_at)
                    .await
            }
            OnboardingJob::Goodbye {
                guild_id,
                username,
                bot,
                joined_at_ms,
            } => {
                if guild_id != config.guild_id
                    || bot
                    || config.gates.mode != OnboardingMode::Session
                    || config.gates.dry_run
                {
                    return Ok(());
                }
                let channel = self
                    .first_postable(&config, &config.goodbye_channel_ids)
                    .await;
                if let Some(GoodbyeEffect::Post {
                    channel_id,
                    content,
                    mentions,
                }) = adjudicate_goodbye(
                    config.gates.mode,
                    &username,
                    days_in_guild(joined_at_ms, Some(now_ms)),
                    channel.as_deref(),
                    config.gates.dry_run,
                ) {
                    self.executor
                        .post_channel_message(
                            &channel_id,
                            &content,
                            &[],
                            &allowed_mentions(mentions),
                        )
                        .await
                        .map_err(|_| RuntimeError::Discord)?;
                }
                Ok(())
            }
            OnboardingJob::Interaction(interaction) => {
                self.interaction(&config, &interaction, &occurred_at).await
            }
        }
    }

    async fn first_postable(&self, config: &OnboardingConfig, channels: &[u64]) -> Option<String> {
        if channels.is_empty() {
            return None;
        }
        let access = MemberAccess::load(&self.executor, config.guild_id, self.bot_id).await?;
        for channel in channels {
            let channel = channel.to_string();
            if access.permits(&self.executor, &channel, true).await {
                return Some(channel);
            }
        }
        None
    }

    async fn welcome(
        &self,
        config: &OnboardingConfig,
        member_id: u64,
        roles: &[String],
        now_secs: i64,
        at: &str,
    ) -> Result<(), RuntimeError> {
        if config.gates.dry_run && config.gates.mode != OnboardingMode::Session {
            return Ok(());
        }
        let guild = config.guild_id.to_string();
        let member = member_id.to_string();
        if has_onboarding_prompt(&self.pool, &guild, &member)
            .await
            .map_err(|_| RuntimeError::Store)?
        {
            return Ok(());
        }
        let channels = if config.gates.mode == OnboardingMode::Anchor {
            config.anchor_channel_id.into_iter().collect::<Vec<_>>()
        } else {
            config.landing_channel_ids.clone()
        };
        let Some(channel) = self.first_postable(config, &channels).await else {
            return Ok(());
        };
        let WelcomeEffect::Post {
            channel_id,
            mut content,
            mention_user_id,
            picker,
        } = adjudicate_welcome(
            config.gates.mode,
            member_id,
            Some(&channel),
            &channel,
            now_secs,
            SUNDAY_SQUAD,
            config.gates.dry_run,
        )
        else {
            return Ok(());
        };
        if config.gates.mode == OnboardingMode::Anchor {
            content = content.replace(SUNDAY_SQUAD.channel_id, &channel);
        }
        let Some(guard) = begin_prompt(&self.pool, &guild, &member)
            .await
            .map_err(|_| RuntimeError::Store)?
        else {
            return Ok(());
        };
        let role_refs: Vec<_> = roles.iter().map(String::as_str).collect();
        let components = picker_components(picker, &role_refs, &config.session_picks);
        self.executor
            .post_channel_message(
                &channel_id,
                &content,
                &components,
                &allowed_mentions(MentionPolicy::Member(mention_user_id)),
            )
            .await
            .map_err(|_| RuntimeError::Discord)?;
        guard
            .record_sent(&channel_id, at)
            .await
            .map_err(|_| RuntimeError::Store)?;
        if config.gates.mode == OnboardingMode::Anchor {
            let plan = GameSelection {
                role_ids: vec![],
                destinations: vec![],
                channel_ids: vec![channel_id],
                unknown_keys: vec![],
                degraded_count: 0,
            };
            record_channel_routed(&self.pool, &guild, &member, &plan, at)
                .await
                .map_err(|_| RuntimeError::Store)?;
        }
        Ok(())
    }

    async fn interaction(
        &self,
        config: &OnboardingConfig,
        interaction: &Interaction,
        at: &str,
    ) -> Result<(), RuntimeError> {
        let router = InteractionRouter::new(RouterGates {
            configured_guild: Some(config.guild_id),
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: config.gates.mode != OnboardingMode::Session
                && !config.landing_channel_ids.is_empty(),
            session_picker: config.gates.mode == OnboardingMode::Session,
        });
        let RoutedInteraction::Component {
            values,
            outcome: ComponentOutcome::Handled { handler },
            ..
        } = route_interaction(&router, interaction, None)
        else {
            return Ok(());
        };
        if !matches!(
            handler,
            ComponentHandler::GamePicker | ComponentHandler::SessionPicker
        ) {
            return Ok(());
        }
        let Some(user) = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
        else {
            return Ok(());
        };
        if user.bot {
            return Ok(());
        }
        self.executor
            .answer_interaction(interaction.id.get(), &interaction.token, &defer_ephemeral())
            .await
            .map_err(|_| RuntimeError::Discord)?;
        let keys: Vec<_> = values.iter().map(String::as_str).collect();
        let guild = config.guild_id.to_string();
        let member = user.id.to_string();
        if handler == ComponentHandler::SessionPicker {
            let access = MemberAccess::load(&self.executor, config.guild_id, user.id.get())
                .await
                .ok_or(RuntimeError::Member)?;
            let mut visible = HashSet::new();
            for pick in &config.session_picks {
                if access
                    .permits(&self.executor, &pick.channel_id, false)
                    .await
                {
                    visible.insert(pick.channel_id.clone());
                }
            }
            let outcome = adjudicate_session_select(
                &keys,
                &|channel| visible.contains(channel),
                &config.session_picks,
            );
            self.reply(interaction, &outcome.reply).await?;
            if outcome.record_routed {
                record_session_routed(&self.pool, &guild, &member, &outcome.routed, at)
                    .await
                    .map_err(|_| RuntimeError::Store)?;
            }
            return Ok(());
        }
        // Serialize whole-answer role writes across processes, not just individual
        // REST calls: the current role set is fetched only after taking the lock.
        let mut lock = self.pool.begin().await.map_err(|_| RuntimeError::Store)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{guild}:{member}:game_picker"))
            .execute(&mut *lock)
            .await
            .map_err(|_| RuntimeError::Store)?;
        let roles = if config.gates.dry_run {
            vec![]
        } else {
            match MemberAccess::load(&self.executor, config.guild_id, user.id.get()).await {
                Some(access) => access.role_ids,
                None => {
                    self.reply(interaction, PICKER_ROLE_FAILURE_REPLY).await?;
                    return Err(RuntimeError::Member);
                }
            }
        };
        let role_refs: Vec<_> = roles.iter().map(String::as_str).collect();
        let Some(outcome) = adjudicate_game_select(
            config.gates.mode,
            &keys,
            &role_refs,
            &|_| false,
            config.guild_id,
            config.gates.dry_run,
        ) else {
            return Ok(());
        };
        for (role, add) in outcome
            .add_role_ids
            .iter()
            .map(|role| (role, true))
            .chain(outcome.remove_role_ids.iter().map(|role| (role, false)))
        {
            if self
                .executor
                .set_member_role(&guild, &member, role, add, "member selected games")
                .await
                .is_err()
            {
                self.reply(interaction, PICKER_ROLE_FAILURE_REPLY).await?;
                return Err(RuntimeError::Discord);
            }
        }
        if !outcome.record_selected {
            return self.reply(interaction, &outcome.reply).await;
        }
        let access = MemberAccess::load(&self.executor, config.guild_id, user.id.get())
            .await
            .ok_or(RuntimeError::Member)?;
        let mut visible = HashSet::new();
        let channels: HashSet<_> = keys
            .iter()
            .filter_map(|key| pick_by_key(key))
            .flat_map(|pick| {
                pick.primary_channel_id
                    .into_iter()
                    .chain([pick.fallback_channel_id])
            })
            .collect();
        for channel in channels {
            if access.permits(&self.executor, channel, false).await {
                visible.insert(channel.to_owned());
            }
        }
        let mut plan = plan_game_selection(&keys, &|channel| visible.contains(channel));
        let selected: Vec<_> = plan
            .destinations
            .iter()
            .map(|destination| destination.key.clone())
            .collect();
        // The frozen domain assumes the hub is public. Do not link it if a live
        // overwrite has made it unavailable; keep successful role selection honest.
        plan.destinations
            .retain(|destination| visible.contains(&destination.channel_id));
        plan.channel_ids.retain(|channel| visible.contains(channel));
        plan.degraded_count = plan
            .destinations
            .iter()
            .filter(|destination| destination.degraded)
            .count();
        let reply = if plan.channel_ids.is_empty() {
            "Your games are saved, but I couldn't find a room you can open. Please try again later."
                .to_owned()
        } else {
            game_picker_reply(&plan, config.guild_id)
        };
        self.reply(interaction, &reply).await?;
        record_game_selected(&self.pool, &guild, &member, &selected, at)
            .await
            .map_err(|_| RuntimeError::Store)?;
        if !plan.channel_ids.is_empty() {
            record_channel_routed(&self.pool, &guild, &member, &plan, at)
                .await
                .map_err(|_| RuntimeError::Store)?;
        }
        lock.commit().await.map_err(|_| RuntimeError::Store)?;
        Ok(())
    }

    async fn reply(&self, interaction: &Interaction, content: &str) -> Result<(), RuntimeError> {
        self.executor
            .edit_interaction_response(
                interaction.application_id.get(),
                &interaction.token,
                content,
                &[],
            )
            .await
            .map_err(|_| RuntimeError::Discord)
    }
}
