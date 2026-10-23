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
use two_bot_discord::onboarding_messages::{defer_ephemeral, picker_components};
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
#[derive(serde::Serialize, serde::Deserialize)]
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
        /// Who left, so the completed goodbye can release their prompt guard
        /// and a rejoin is welcomed again. `None` for payloads captured
        /// before the clearing path existed (serde leaves a missing `Option`
        /// as `None`): the goodbye still posts, only the clearing is skipped.
        member_id: Option<u64>,
    },
    #[serde(skip)]
    Interaction(Box<Interaction>),
}

impl OnboardingJob {
    /// Persist captured member state, but never a component callback credential.
    /// Interrupted components require a fresh member submission after restart;
    /// replaying an uncertain defer or partial role mutation is not safe.
    pub fn durable_payload(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Interaction(interaction) => serde_json::to_string(&serde_json::json!({
                "interrupted_interaction": interaction.id.to_string(),
            })),
            _ => serde_json::to_string(self),
        }
    }

    pub fn recover(payload: &str) -> Result<Option<Self>, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(payload)?;
        if value["interrupted_interaction"].as_str().is_some() {
            Ok(None)
        } else {
            serde_json::from_value(value).map(Some)
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Config,
    Settings,
    Identity,
    Member,
    Role,
    Discord,
    Store,
}

enum FreshRecipient {
    Eligible { roles: Vec<String> },
    Ineligible,
}

/// Parse one strict member document. Missing `bot` or `pending` means false
/// (Discord omits both for an ordinary human). A present non-bool, a user id
/// that is not this member, or a role that is not a snowflake string is
/// unreadable, not proof the member left.
fn fresh_recipient(
    member_id: u64,
    body: &serde_json::Value,
) -> Result<FreshRecipient, RuntimeError> {
    let user = body
        .get("user")
        .and_then(serde_json::Value::as_object)
        .ok_or(RuntimeError::Member)?;
    let id = user
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or(RuntimeError::Member)?;
    if id != member_id.to_string() {
        return Err(RuntimeError::Member);
    }
    let bot = match user.get("bot") {
        None => false,
        Some(value) => value.as_bool().ok_or(RuntimeError::Member)?,
    };
    let pending = match body.get("pending") {
        None => false,
        Some(value) => value.as_bool().ok_or(RuntimeError::Member)?,
    };
    let roles = body
        .get("roles")
        .and_then(serde_json::Value::as_array)
        .ok_or(RuntimeError::Member)?;
    let mut current = Vec::with_capacity(roles.len());
    for role in roles {
        let id = role.as_str().ok_or(RuntimeError::Member)?;
        if id
            .parse::<u64>()
            .ok()
            .filter(|parsed| *parsed != 0)
            .is_none()
        {
            return Err(RuntimeError::Member);
        }
        current.push(id.to_owned());
    }
    if bot || pending {
        Ok(FreshRecipient::Ineligible)
    } else {
        Ok(FreshRecipient::Eligible { roles: current })
    }
}

pub struct OnboardingRuntime {
    pool: PgPool,
    executor: ActionExecutor,
    deployment: HashMap<String, String>,
    guild_id: u64,
    bot_id: u64,
}

/// Merge a stored `TWO_ONBOARDING_DRY_RUN` rendering over the deployment map.
/// Stored rows predate write-time refusal: a legacy rendering the old writer
/// accepted and ran live (`"true"`, `"false"`, `"on"`, ...) must not fail the
/// hot-refresh path closed. Only an exact `"0"` or `"1"` overrides the
/// boot-validated deployment value; anything else keeps deployment.
/// Returns true when a present-but-invalid stored value was refused, so the
/// caller can log the key (never the value). No database access: pure merge.
fn apply_stored_dry_run(vars: &mut HashMap<String, String>, stored: Option<&str>) -> bool {
    match stored {
        None => false,
        Some(value @ ("0" | "1")) => {
            vars.insert("TWO_ONBOARDING_DRY_RUN".to_owned(), value.to_owned());
            false
        }
        Some(_) => true,
    }
}

/// Merge a stored settings snapshot over the boot-validated deployment map.
///
/// This owns both halves of the dry-run handling: the generic stored-key loop
/// deliberately excludes `TWO_ONBOARDING_DRY_RUN` (plus boot-only identity and
/// mode keys), and `apply_stored_dry_run` then merges that key only when it is
/// exactly `"0"`/`"1"`. Keeping the filter and the helper in one function keeps
/// a `CONFIG_KEYS` refactor from silently re-copying a legacy `"true"` row into
/// `vars`, which would fail `OnboardingConfig::from_map` closed and poison the
/// job row. Returns the merged map plus whether a present-but-invalid stored
/// dry-run value was refused (the caller logs the key, never the value).
/// No database access: pure merge, directly unit-testable.
fn merge_stored_vars(
    deployment: &HashMap<String, String>,
    stored: &HashMap<String, String>,
) -> (HashMap<String, String>, bool) {
    let mut vars = deployment.clone();
    for key in CONFIG_KEYS.iter().filter(|key| {
        !matches!(
            **key,
            "DISCORD_GUILD_ID" | "TWO_ONBOARDING_MODE" | "TWO_ONBOARDING_DRY_RUN"
        )
    }) {
        if let Some(value) = stored.get(*key) {
            vars.insert((*key).to_owned(), value.clone());
        }
    }
    let refused = apply_stored_dry_run(
        &mut vars,
        stored.get("TWO_ONBOARDING_DRY_RUN").map(String::as_str),
    );
    (vars, refused)
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
            SettingsStore::new(&self.pool).load_guild_snapshot(&self.guild_id.to_string()),
        )
        .await
        .map_err(|_| RuntimeError::Settings)?
        .map_err(|_| RuntimeError::Settings)?;
        let cache = SettingsCache::load(&snapshot);
        let stored = cache.env_snapshot(Some(&self.guild_id.to_string()));
        // A legacy stored dry-run row must not fail the worker closed: see
        // `merge_stored_vars`. A refused row keeps deployment and logs the key
        // (never the value), so the job keeps running instead of poisoning
        // itself into `failed` and blocking every later boot in
        // `recover_onboarding_jobs`.
        let (vars, refused) = merge_stored_vars(&self.deployment, &stored);
        if refused {
            tracing::error!(
                "stored TWO_ONBOARDING_DRY_RUN must be exactly \"0\" or \"1\" when set; keeping deployment value"
            );
        }
        OnboardingConfig::from_map(&vars)
            .map_err(|_| RuntimeError::Config)?
            .ok_or(RuntimeError::Config)
    }

    pub fn capture<I: two_bot_discord::InviteSource>(
        &self,
        event: &Event,
        pipeline: &GatewayPipeline<I>,
    ) -> Option<OnboardingJob> {
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
                    member_id: Some(member.user.id.get()),
                })
            }
            Event::InteractionCreate(interaction) if self.accepts_interaction(&interaction.0) => {
                Some(OnboardingJob::Interaction(Box::new(interaction.0.clone())))
            }
            _ => None,
        }
    }

    /// Ingress eligibility uses only immutable identity and the shared router,
    /// never settings SQL. Stale/disabled mode submissions get an honest edit
    /// after defer; hot configuration cannot delay the initial acknowledgement.
    pub fn accepts_interaction(&self, interaction: &Interaction) -> bool {
        let router = InteractionRouter::new(RouterGates {
            configured_guild: Some(self.guild_id),
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: true,
            session_picker: true,
        });
        interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .is_some_and(|user| !user.bot)
            && matches!(
                route_interaction(&router, interaction, None),
                RoutedInteraction::Component {
                    outcome: ComponentOutcome::Handled {
                        handler: ComponentHandler::GamePicker | ComponentHandler::SessionPicker
                    },
                    ..
                }
            )
    }

    pub async fn acknowledge(
        &self,
        interaction: &Interaction,
        received: tokio::time::Instant,
    ) -> bool {
        // Include executor scheduling/HTTP in one receive-relative deadline,
        // leaving margin below Discord's three-second limit. Never retry defer.
        matches!(
            tokio::time::timeout_at(
                received + Duration::from_millis(2500),
                self.executor.answer_interaction(
                    interaction.id.get(),
                    &interaction.token,
                    &defer_ephemeral(),
                ),
            )
            .await,
            Ok(Ok(()))
        )
    }

    pub async fn unconfirmed_interaction(
        &self,
        interaction: &Interaction,
    ) -> Result<(), RuntimeError> {
        self.reply(
            interaction,
            "I couldn't confirm this interaction. Your selection may already have been processed, so I won't apply it again. Please open the menu and try again.",
        )
        .await
    }

    pub async fn handle_acknowledged(
        &self,
        interaction: &Interaction,
        now_ms: i64,
    ) -> Result<(), RuntimeError> {
        let occurred_at = two_bot_core::funnel::format_iso_millis(now_ms);
        // Configuration/settings failures are inside the post-defer boundary.
        // A successful error edit is terminal, with no role or success writes.
        let config = match self.config().await {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "onboarding picker configuration unavailable after defer"
                );
                return self
                    .reply(interaction, "I couldn't load the menu configuration. No selection was applied. Please open the menu and try again later.")
                    .await;
            }
        };
        self.interaction(&config, interaction, &occurred_at).await
    }

    pub async fn handle(&self, job: OnboardingJob, now_ms: i64) -> Result<(), RuntimeError> {
        if let OnboardingJob::Interaction(interaction) = job {
            if !self.accepts_interaction(&interaction) {
                return Ok(());
            }
            return if self
                .acknowledge(&interaction, tokio::time::Instant::now())
                .await
            {
                self.handle_acknowledged(&interaction, now_ms).await
            } else {
                self.unconfirmed_interaction(&interaction).await
            };
        }
        let config = self.config().await?;
        let occurred_at = two_bot_core::funnel::format_iso_millis(now_ms);
        match job {
            OnboardingJob::Welcome {
                guild_id,
                member_id,
                bot,
                pending,
                trigger,
                roles: _,
            } => {
                // Captured eligibility is a ceiling. welcome re-reads the member
                // and may only narrow it; an ineligible capture performs no read.
                if guild_id != config.guild_id
                    || !welcome_trigger(trigger)
                    || decide_prompt(bot, pending, false) != PromptDecision::Prompt
                {
                    return Ok(());
                }
                self.welcome(&config, member_id, now_ms / 1000, &occurred_at)
                    .await
            }
            OnboardingJob::Goodbye {
                guild_id,
                username,
                bot,
                joined_at_ms,
                member_id,
            } => {
                if guild_id != config.guild_id || bot {
                    return Ok(());
                }
                // Release the prompt guard first, whatever the goodbye-post
                // settings: a rejoin is welcomed again in every mode, since
                // welcomes post in all of them. The DELETE is idempotent, so
                // a failed clear retries before anything is public and a
                // failed post below retries with nothing to undo. The next
                // welcome re-runs the full suppression gate.
                if let Some(member_id) = member_id {
                    clear_onboarding_prompt(
                        &self.pool,
                        &guild_id.to_string(),
                        &member_id.to_string(),
                    )
                    .await
                    .map_err(|_| RuntimeError::Store)?;
                }
                if config.gates.mode != OnboardingMode::Session || config.gates.dry_run {
                    return Ok(());
                }
                let channel = self
                    .first_postable(&config, &config.goodbye_channel_ids)
                    .await?;
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
                        .post_channel_message(&channel_id, &content, &[], mentions)
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

    async fn first_postable(
        &self,
        config: &OnboardingConfig,
        channels: &[u64],
    ) -> Result<Option<String>, RuntimeError> {
        if channels.is_empty() {
            return Ok(None);
        }
        let Some(access) = MemberAccess::load(&self.executor, config.guild_id, self.bot_id)
            .await
            .map_err(|_| RuntimeError::Discord)?
        else {
            return Ok(None);
        };
        for channel in channels {
            let channel = channel.to_string();
            if access
                .permits(&self.executor, &channel, true)
                .await
                .map_err(|_| RuntimeError::Discord)?
            {
                return Ok(Some(channel));
            }
        }
        Ok(None)
    }

    /// Live guild member for a queued welcome. `Ineligible` is proven (404,
    /// pending, or bot) and must not leave a marker. Any other unreadable
    /// document is [`RuntimeError::Member`] so the worker retains the job.
    async fn current_recipient(
        &self,
        guild_id: u64,
        member_id: u64,
    ) -> Result<FreshRecipient, RuntimeError> {
        let path = format!("/guilds/{guild_id}/members/{member_id}");
        match self.executor.get_json_strict(&path).await {
            Ok(Some(body)) => fresh_recipient(member_id, &body),
            Ok(None) => Ok(FreshRecipient::Ineligible),
            Err(_) => Err(RuntimeError::Member),
        }
    }

    async fn welcome(
        &self,
        config: &OnboardingConfig,
        member_id: u64,
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
        let Some(channel) = self.first_postable(config, &channels).await? else {
            return Ok(());
        };
        // Fresh membership before the prompt lock. Proven absence, a pending
        // member, or a bot skips with no marker. Unreadable evidence fails
        // before begin_prompt so the durable job stays retryable.
        let FreshRecipient::Eligible { roles } =
            self.current_recipient(config.guild_id, member_id).await?
        else {
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
                MentionPolicy::Member(mention_user_id),
            )
            .await
            .map_err(|_| RuntimeError::Discord)?;
        if config.gates.mode == OnboardingMode::Anchor {
            guard.record_anchor_sent(&channel_id, at).await
        } else {
            guard.record_sent(&channel_id, at).await
        }
        .map_err(|_| RuntimeError::Store)?;
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
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: config.gates.mode != OnboardingMode::Session,
            session_picker: config.gates.mode == OnboardingMode::Session,
        });
        let RoutedInteraction::Component {
            values,
            outcome: ComponentOutcome::Handled { handler },
            ..
        } = route_interaction(&router, interaction, None)
        else {
            return self
                .reply(interaction, "This menu is no longer enabled. No selection was applied. Please open the current menu and try again.")
                .await;
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
        // The caller has confirmed ingress defer and committed the delivery
        // receipt. Never issue a second initial callback from a feature worker.
        let keys: Vec<_> = values.iter().map(String::as_str).collect();
        // Keep the error boundary outside the processing timeout: cancellation
        // drops uncommitted success rows before we try to finish the defer.
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            self.process_selection(config, interaction, handler, &keys, user.id.get(), at),
        )
        .await
        .unwrap_or(Err(RuntimeError::Discord));
        if let Err(error) = result {
            tracing::warn!(
                ?error,
                ?handler,
                "onboarding picker failed; attempting deferred error reply"
            );
            // The success rows commit before the reply, so a reply that never
            // reached the member (or a timeout past the commit) must not leave
            // them counted for a selection the member is told failed. Keys
            // are per submission, so this is a no-op when nothing committed.
            if let Err(retract) = retract_picker_rows(
                &self.pool,
                &config.guild_id.to_string(),
                &user.id.get().to_string(),
                at,
            )
            .await
            {
                tracing::warn!(
                    ?retract,
                    "could not retract picker rows after a failed reply"
                );
            }
            let content = if matches!(error, RuntimeError::Role) {
                PICKER_ROLE_FAILURE_REPLY
            } else if handler == ComponentHandler::GamePicker {
                "I couldn't finish your selection. Some game roles may already have changed. Please open the menu and try again."
            } else {
                "I couldn't finish routing your session selection. Please open the menu and try again."
            };
            // A delivered failure reply is terminal, not a reason to replay a
            // member's command. Only failure to finish the defer stays retryable.
            return self.reply(interaction, content).await;
        }
        Ok(())
    }

    async fn process_selection(
        &self,
        config: &OnboardingConfig,
        interaction: &Interaction,
        handler: ComponentHandler,
        keys: &[&str],
        member_id: u64,
        at: &str,
    ) -> Result<(), RuntimeError> {
        let guild = config.guild_id.to_string();
        let member = member_id.to_string();
        if handler == ComponentHandler::SessionPicker {
            let access = MemberAccess::load(&self.executor, config.guild_id, member_id)
                .await
                .map_err(|_| RuntimeError::Member)?
                .ok_or(RuntimeError::Member)?;
            let channels: HashSet<_> = keys
                .iter()
                .filter_map(|key| session_pick_by_key(key, &config.session_picks))
                .map(|pick| pick.channel_id.as_str())
                .collect();
            let mut visible = HashSet::new();
            for channel in channels {
                if access
                    .permits(&self.executor, channel, false)
                    .await
                    .map_err(|_| RuntimeError::Member)?
                {
                    visible.insert(channel.to_owned());
                }
            }
            let outcome = adjudicate_session_select(
                keys,
                &|channel| visible.contains(channel),
                &config.session_picks,
            );
            if outcome.record_routed {
                let mut transaction = self.pool.begin().await.map_err(|_| RuntimeError::Store)?;
                record_session_routed_in_transaction(
                    &mut transaction,
                    &guild,
                    &member,
                    &outcome.routed,
                    at,
                )
                .await
                .map_err(|_| RuntimeError::Store)?;
                // Commit before replying: the reply describes committed state,
                // so a commit failure yields the failure path, never a false
                // routed claim. A reply that then fails is retracted in
                // `interaction`.
                transaction
                    .commit()
                    .await
                    .map_err(|_| RuntimeError::Store)?;
                self.reply(interaction, &outcome.reply).await?;
            } else {
                self.reply(interaction, &outcome.reply).await?;
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
            MemberAccess::load(&self.executor, config.guild_id, member_id)
                .await
                .map_err(|_| RuntimeError::Member)?
                .ok_or(RuntimeError::Member)?
                .role_ids
        };
        let role_refs: Vec<_> = roles.iter().map(String::as_str).collect();
        let Some(outcome) = adjudicate_game_select(
            config.gates.mode,
            keys,
            &role_refs,
            &|_| false,
            config.guild_id,
            config.gates.dry_run,
        ) else {
            return Err(RuntimeError::Config);
        };
        let writes: Vec<(&str, bool)> = outcome
            .add_role_ids
            .iter()
            .map(|role| (role.as_str(), true))
            .chain(
                outcome
                    .remove_role_ids
                    .iter()
                    .map(|role| (role.as_str(), false)),
            )
            .collect();
        let mut applied: Vec<(&str, bool)> = Vec::with_capacity(writes.len());
        for (index, (role, add)) in writes.iter().enumerate() {
            if self
                .executor
                .set_member_role(&guild, &member, role, *add, "member selected games")
                .await
                .is_err()
            {
                return self
                    .game_partial_failure(
                        interaction,
                        lock,
                        GameSelectionTarget {
                            guild: &guild,
                            member: &member,
                            at,
                        },
                        &applied,
                        &writes[index..],
                    )
                    .await;
            }
            applied.push((*role, *add));
        }
        if !outcome.record_selected {
            return self.reply(interaction, &outcome.reply).await;
        }
        let access = MemberAccess::load(&self.executor, config.guild_id, member_id)
            .await
            .map_err(|_| RuntimeError::Member)?
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
            if access
                .permits(&self.executor, channel, false)
                .await
                .map_err(|_| RuntimeError::Member)?
            {
                visible.insert(channel.to_owned());
            }
        }
        let mut plan = plan_game_selection(keys, &|channel| visible.contains(channel));
        let selected: Vec<_> = plan
            .destinations
            .iter()
            .map(|destination| destination.key.clone())
            .collect();
        // The frozen domain assumes the hub is public. Do not link it if a live
        // overwrite has made it unavailable; keep successful role selection honest.
        plan.destinations.retain(|destination| {
            destination
                .channel_id
                .as_ref()
                .is_none_or(|channel| visible.contains(channel))
        });
        plan.channel_ids.retain(|channel| visible.contains(channel));
        plan.degraded_count = plan
            .destinations
            .iter()
            .filter(|destination| destination.degraded)
            .count();
        // One reply builder for every outcome: with no reachable room it says
        // the roles were saved and names each pick that has no channel, as the
        // legacy picker does (docs/onboarding-port.md, runtime contract 5).
        let reply = game_picker_reply(&plan, config.guild_id);
        record_game_selection_in_transaction(&mut lock, &guild, &member, &selected, &plan, at)
            .await
            .map_err(|_| RuntimeError::Store)?;
        // Commit before replying: the reply describes committed state, so a
        // commit failure yields the failure path, never a false saved claim.
        // A reply that then fails is retracted in `interaction`.
        lock.commit().await.map_err(|_| RuntimeError::Store)?;
        self.reply(interaction, &reply).await?;
        Ok(())
    }

    /// Finish a game-picker role loop that failed partway with some roles
    /// already flipped. With nothing applied this is the legacy total failure
    /// (generic role reply, nothing recorded). Otherwise the applied grants
    /// are committed as the durable record of what flipped, and the failure
    /// reply names applied vs pending picks, so the member's retry — a fresh
    /// whole-answer submission — converges to full success.
    async fn game_partial_failure(
        &self,
        interaction: &Interaction,
        lock: sqlx::Transaction<'_, sqlx::Postgres>,
        target: GameSelectionTarget<'_>,
        applied: &[(&str, bool)],
        pending: &[(&str, bool)],
    ) -> Result<(), RuntimeError> {
        if applied.is_empty() {
            return Err(RuntimeError::Role);
        }
        let mut lock = lock;
        let applied_keys: Vec<String> = applied
            .iter()
            .filter(|(_, add)| *add)
            .filter_map(|(role, _)| pick_for_role(role))
            .map(|pick| pick.key.to_owned())
            .collect();
        record_game_partial_in_transaction(
            &mut lock,
            target.guild,
            target.member,
            &applied_keys,
            target.at,
        )
        .await
        .map_err(|_| RuntimeError::Store)?;
        lock.commit().await.map_err(|_| RuntimeError::Store)?;
        let labels = |writes: &[(&str, bool)]| {
            writes
                .iter()
                .map(|(role, add)| partial_failure_label(role, *add))
                .collect::<Vec<_>>()
        };
        self.reply(
            interaction,
            &game_partial_failure_reply(&labels(applied), &labels(pending)),
        )
        .await
    }

    async fn reply(&self, interaction: &Interaction, content: &str) -> Result<(), RuntimeError> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.executor.edit_interaction_response_with_blocked_retry(
                interaction.application_id.get(),
                &interaction.token,
                content,
            ),
        )
        .await
        .map_err(|_| RuntimeError::Discord)?
        .map_err(|_| RuntimeError::Discord)
    }
}

/// Where-and-when for one game-picker submission, threaded to the
/// partial-failure finisher as a single argument.
struct GameSelectionTarget<'a> {
    guild: &'a str,
    member: &'a str,
    at: &'a str,
}

/// Member-facing label for one game-picker role write. Revokes carry their
/// direction: without it a completed revoke reads as `Applied: Shooters`.
fn partial_failure_label(role: &str, add: bool) -> String {
    let label = pick_for_role(role)
        .map(|pick| pick.label.to_owned())
        .unwrap_or_else(|| role.to_owned());
    if add {
        label
    } else {
        format!("{label} (removed)")
    }
}

/// Catalog pick behind one game-picker role write, for member-facing naming.
/// Searches games and platforms, like the submission lookup: either can ride a
/// whole-answer write list.
fn pick_for_role(role_id: &str) -> Option<&'static GamePick> {
    GAME_PICKS
        .iter()
        .chain(PLATFORM_PICKS.iter())
        .find(|pick| pick.role_id == role_id)
}

/// Member-facing partial-failure copy: which picks landed vs which still need
/// a retry. Labels, not ids: the member picked "Shooters", not a snowflake.
/// Deliberately not [`PICKER_ROLE_FAILURE_REPLY`]: that copy promises nothing
/// was applied, which is false once earlier roles already flipped.
fn game_partial_failure_reply(applied: &[String], pending: &[String]) -> String {
    format!(
        "I couldn't set all of those roles. Applied: {}. Still pending: {}. Open the menu and submit again to finish the rest.",
        applied.join(", "),
        pending.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The partial-failure reply names applied vs pending picks by label, and
    /// is never the generic role copy (which promises nothing was applied).
    #[test]
    fn partial_failure_reply_names_applied_vs_pending_labels() {
        let reply = game_partial_failure_reply(&["Shooters".to_owned()], &["Survival".to_owned()]);
        assert!(reply.contains("Shooters"), "{reply}");
        assert!(reply.contains("Survival"), "{reply}");
        assert!(reply.contains("Applied:"), "{reply}");
        assert!(reply.contains("Still pending:"), "{reply}");
        assert_ne!(reply, PICKER_ROLE_FAILURE_REPLY);
        let multi = game_partial_failure_reply(
            &["Shooters".to_owned(), "Horror".to_owned()],
            &["Survival".to_owned(), "PC".to_owned()],
        );
        assert!(multi.contains("Shooters, Horror"), "{multi}");
        assert!(multi.contains("Survival, PC"), "{multi}");
    }

    /// Role-to-pick lookup covers games and platforms; unknown ids stay
    /// unknown so the caller can fall back to the raw id.
    #[test]
    fn role_lookup_resolves_catalog_picks() {
        let shooters = pick_by_key("shooters").expect("catalog");
        assert_eq!(
            pick_for_role(shooters.role_id).map(|pick| pick.key),
            Some("shooters")
        );
        let pc = pick_by_key("pc").expect("catalog");
        assert_eq!(pick_for_role(pc.role_id).map(|pick| pick.key), Some("pc"));
        assert_eq!(pick_for_role("9999999999999999999"), None);
    }

    /// Revoked picks carry their direction in the partial-failure reply, so a
    /// completed revoke never reads as an applied grant. Grants keep the bare
    /// catalog label the acceptance tests pin.
    #[test]
    fn partial_failure_labels_mark_revoked_picks() {
        let shooters = pick_by_key("shooters").expect("catalog");
        assert_eq!(partial_failure_label(shooters.role_id, true), "Shooters");
        assert_eq!(
            partial_failure_label(shooters.role_id, false),
            "Shooters (removed)"
        );
        assert_eq!(
            partial_failure_label("9999999999999999999", true),
            "9999999999999999999"
        );
        assert_eq!(
            partial_failure_label("9999999999999999999", false),
            "9999999999999999999 (removed)"
        );
    }

    /// Goodbye payloads captured before the clearing path existed carry no
    /// member id; they still recover (with an unknown member) instead of
    /// poisoning restart recovery.
    #[test]
    fn goodbye_payloads_from_before_member_identity_still_recover() {
        let payload =
            r#"{"Goodbye":{"guild_id":22,"username":"dave","bot":false,"joined_at_ms":123}}"#;
        let recovered = OnboardingJob::recover(payload).expect("old payload parses");
        assert!(
            matches!(recovered, Some(OnboardingJob::Goodbye { .. })),
            "an old goodbye payload must recover as a goodbye job"
        );
        if let Some(OnboardingJob::Goodbye {
            member_id,
            username,
            ..
        }) = recovered
        {
            assert_eq!(member_id, None);
            assert_eq!(username, "dave");
        }
    }

    /// A legacy stored `TWO_ONBOARDING_DRY_RUN` row must never fail the
    /// hot-refresh path closed: only an exact `"0"`/`"1"` overrides the
    /// boot-validated deployment value, anything else keeps deployment and
    /// reports refusal (the caller logs the key, never the value).
    #[test]
    fn legacy_stored_dry_run_keeps_deployment_value() {
        // No stored row: deployment untouched, nothing to report.
        let mut vars = HashMap::from([("TWO_ONBOARDING_DRY_RUN".to_owned(), "0".to_owned())]);
        assert!(!apply_stored_dry_run(&mut vars, None));
        assert_eq!(
            vars.get("TWO_ONBOARDING_DRY_RUN").map(String::as_str),
            Some("0")
        );
        // Exact "0"/"1" still override, either direction.
        let mut vars = HashMap::from([("TWO_ONBOARDING_DRY_RUN".to_owned(), "0".to_owned())]);
        assert!(!apply_stored_dry_run(&mut vars, Some("1")));
        assert_eq!(
            vars.get("TWO_ONBOARDING_DRY_RUN").map(String::as_str),
            Some("1")
        );
        let mut vars = HashMap::new();
        assert!(!apply_stored_dry_run(&mut vars, Some("0")));
        assert_eq!(
            vars.get("TWO_ONBOARDING_DRY_RUN").map(String::as_str),
            Some("0")
        );
        // Legacy renderings the old writer accepted and ran live ("true",
        // "false", "on", ...) keep deployment and report refusal, so the
        // worker keeps running instead of poisoning its job row into `failed`.
        for raw in ["true", "false", "yes", "on", "TRUE", "", "01", " 1", "2"] {
            let mut vars = HashMap::from([("TWO_ONBOARDING_DRY_RUN".to_owned(), "0".to_owned())]);
            assert!(
                apply_stored_dry_run(&mut vars, Some(raw)),
                "{raw:?} must be refused"
            );
            assert_eq!(
                vars.get("TWO_ONBOARDING_DRY_RUN").map(String::as_str),
                Some("0"),
                "{raw:?} must keep deployment"
            );
            let mut vars = HashMap::new();
            assert!(apply_stored_dry_run(&mut vars, Some(raw)));
            assert!(
                !vars.contains_key("TWO_ONBOARDING_DRY_RUN"),
                "{raw:?} must not invent a value"
            );
        }
    }

    /// The filter exclusion and the helper must hold together: a stored
    /// `"true"` next to another stored key must keep the deployment dry-run
    /// value and still merge the other key, and the merged map must parse.
    /// Deleting `"TWO_ONBOARDING_DRY_RUN"` from the exclusion filter copies
    /// `"true"` into the map and fails this test (mutant check).
    #[test]
    fn stored_merge_excludes_legacy_dry_run_but_merges_other_keys() {
        let deployment = HashMap::from([
            ("DISCORD_GUILD_ID".to_owned(), "2222".to_owned()),
            ("TWO_ONBOARDING_DRY_RUN".to_owned(), "0".to_owned()),
        ]);
        let stored = HashMap::from([
            ("TWO_ONBOARDING_DRY_RUN".to_owned(), "true".to_owned()),
            ("DISCORD_LANDING_CHANNEL_IDS".to_owned(), "12".to_owned()),
        ]);
        let (vars, refused) = merge_stored_vars(&deployment, &stored);
        assert!(refused, "legacy stored dry-run must report refusal");
        assert_eq!(
            vars.get("TWO_ONBOARDING_DRY_RUN").map(String::as_str),
            Some("0"),
            "legacy stored dry-run must keep deployment"
        );
        assert_eq!(
            vars.get("DISCORD_LANDING_CHANNEL_IDS").map(String::as_str),
            Some("12"),
            "other stored keys must still merge"
        );
        let config = OnboardingConfig::from_map(&vars)
            .expect("merged map must parse")
            .expect("guild present");
        assert!(!config.gates.dry_run);
    }
}
