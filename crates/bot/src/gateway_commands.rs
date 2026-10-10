//! Gateway composition of the shared custom-command router/runtime.
//! Bootstrap reads finish before constructing the shard, including cold RESUME.
//! Event work is detached at reception by [`crate::command_runtime::CommandRuntime::dispatch`].

use std::{
    collections::HashMap,
    sync::{Arc, PoisonError, RwLock},
};

use sqlx::PgPool;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::Interaction,
    channel::Message,
    gateway::payload::incoming::GuildCreate,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::{
    automod_runtime::FunnelDisposition,
    custom_commands::AutomationMessageAcceptance,
    feature_commands::FeatureGates,
    moderation::ModerationGates,
    router::{RouterGates, SurfaceFlags},
    InteractionRouter,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime},
    ActionExecutor,
};

pub struct GatewayCommandConfig {
    gates: RouterGates,
    text_commands: bool,
    acceptance: AutomationMessageAcceptance,
}

impl GatewayCommandConfig {
    pub fn from_map(guild_id: u64, vars: &HashMap<String, String>) -> Result<Self, sqlx::Error> {
        let features = FeatureGates::from_map(vars).map_err(|_| config_error())?;
        let moderation = ModerationGates::from_map(vars).map_err(|_| config_error())?;
        if guild_id == 0 {
            return Err(config_error());
        }
        let gates = RouterGates::from_slices(
            Some(guild_id),
            &features,
            &moderation,
            SurfaceFlags {
                scorecard: vars
                    .get("TWO_COMMUNITY_SCORECARD")
                    .is_some_and(|v| v == "1"),
                // Component handlers are not activated by this feature slice.
                ..SurfaceFlags::default()
            },
        );
        Ok(Self {
            gates,
            text_commands: features.text_commands,
            acceptance: acceptance_without_inspector(vars.get("TWO_AUTOMOD").map(String::as_str)),
        })
    }
}

fn config_error() -> sqlx::Error {
    sqlx::Error::InvalidArgument("gateway command configuration invalid".into())
}

/// Bootstrap acceptance for a create with no per-create verdict. Only an
/// explicit disabled configuration is known acceptance. Every other value stays
/// `Unavailable` here; an enabled automod takes its acceptance from the worker's
/// verdict in `acceptance_for_verdict`.
fn acceptance_without_inspector(automod: Option<&str>) -> AutomationMessageAcceptance {
    if automod == Some("0") {
        AutomationMessageAcceptance::AutomodDisabled
    } else {
        AutomationMessageAcceptance::Unavailable
    }
}

/// Automod verdict for one MessageCreate, as decided in gateway order by the
/// serial dispatch worker. An accepted create behaves like an unmatched
/// inspection; every other verdict fails closed. A missing verdict is never
/// acceptance: the worker's bool precheck may let an unscreened create through,
/// and this mapping is the second fence.
pub(crate) fn acceptance_for_verdict(
    verdict: Option<FunnelDisposition>,
) -> AutomationMessageAcceptance {
    match verdict {
        Some(FunnelDisposition::Accept) => AutomationMessageAcceptance::Unmatched,
        Some(FunnelDisposition::CaptureOnly) => AutomationMessageAcceptance::CaptureOnly,
        // `None` dispositions belong to edits, never creates: a create carrying
        // one is unexpected, so refuse it like a match.
        Some(FunnelDisposition::None) => AutomationMessageAcceptance::Matched,
        None => AutomationMessageAcceptance::Unavailable,
    }
}

fn effective_acceptance(
    configured: AutomationMessageAcceptance,
    verdict: Option<FunnelDisposition>,
) -> AutomationMessageAcceptance {
    // `TWO_AUTOMOD=0` never waits for a verdict: the disabled fast path stays
    // exactly as configured. Every other configuration needs the verdict.
    if configured == AutomationMessageAcceptance::AutomodDisabled {
        configured
    } else {
        acceptance_for_verdict(verdict)
    }
}

pub struct GatewayCommands {
    runtime: CustomCommandRuntime,
    application_id: u64,
    guild_id: Id<GuildMarker>,
    /// Seeded by bootstrap REST, refreshed from GUILD_CREATE/GUILD_UPDATE.
    guild_name: RwLock<String>,
    text_commands: bool,
    acceptance: AutomationMessageAcceptance,
}

impl GatewayCommands {
    #[cfg(test)]
    pub async fn bootstrap(
        pool: PgPool,
        executor: ActionExecutor,
        config: GatewayCommandConfig,
    ) -> Result<Arc<crate::command_runtime::CommandRuntime>, sqlx::Error> {
        let runtime = crate::command_runtime::CommandRuntime::new(
            pool,
            executor,
            crate::command_runtime::router_with_commands(config.gates),
            config.gates.configured_guild.ok_or_else(config_error)?,
            config.gates.automations,
        );
        runtime.initialize_custom_commands(config).await?;
        Ok(runtime)
    }

    pub(crate) async fn bootstrap_with_router(
        pool: PgPool,
        executor: ActionExecutor,
        config: GatewayCommandConfig,
        router: Arc<InteractionRouter>,
    ) -> Result<Self, sqlx::Error> {
        let guild_id = config
            .gates
            .configured_guild
            .and_then(Id::new_checked)
            .ok_or_else(config_error)?;
        // The cause is a fixed token (never the error text), so a failed boot
        // names the read and why without exposing provider or transport detail.
        let application_id = executor.current_application_id().await.map_err(|error| {
            tracing::warn!(
                step = "application_lookup",
                cause = error.cause(),
                "gateway bootstrap read failed"
            );
            sqlx::Error::InvalidArgument("gateway application context unavailable".into())
        })?;
        let bootstrap_guild_name = executor.guild_name(guild_id.get()).await.map_err(|error| {
            tracing::warn!(
                step = "guild_lookup",
                cause = error.cause(),
                "gateway bootstrap read failed"
            );
            sqlx::Error::InvalidArgument("gateway guild context unavailable".into())
        })?;
        let runtime = CustomCommandRuntime::new(pool, router, executor, application_id);
        Ok(Self {
            runtime,
            application_id,
            guild_id,
            guild_name: RwLock::new(bootstrap_guild_name),
            text_commands: config.text_commands,
            acceptance: config.acceptance,
        })
    }

    /// Reception-order observation, before later events spawn their work:
    /// keeps the template `{server}` name current without a cache lookup.
    pub fn observe(&self, event: &Event) {
        let name = match event {
            Event::GuildCreate(guild) => match guild.as_ref() {
                GuildCreate::Available(guild) if guild.id == self.guild_id => &guild.name,
                _ => return,
            },
            Event::GuildUpdate(update) if update.0.id == self.guild_id => &update.0.name,
            _ => return,
        };
        name.clone_into(
            &mut self
                .guild_name
                .write()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    fn guild_name(&self) -> String {
        self.guild_name
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn published_commands(&self) -> Option<Arc<[two_bot_core::CommandDefinition]>> {
        self.runtime.published_commands()
    }

    /// One complete registry for READY and for a cold RESUME where READY is
    /// absent. The session must name the bootstrapped application; a mismatch
    /// publishes nothing.
    pub async fn sync_registry(&self, application_id: u64) -> Result<(), CustomCommandError> {
        if application_id != self.application_id {
            return Err(CustomCommandError::Context);
        }
        self.runtime.sync_registry().await
    }

    /// Detached interaction dispatch. True means this runtime owns the
    /// interaction, including a failed or uncertain attempt: acknowledged
    /// operations never fall through to another handler or replay.
    pub async fn handle_interaction(&self, interaction: &Interaction) -> bool {
        let guild_name = self.guild_name();
        match self
            .runtime
            .handle_interaction(interaction, Some(&guild_name))
            .await
        {
            Ok(handled) => handled,
            Err(error) => {
                // Errors contain fixed codes only.
                tracing::warn!(error = %error, "custom-command interaction finished without confirmed success");
                true
            }
        }
    }

    /// Detached prefix-trigger dispatch with the automod verdict for this
    /// create. A disabled configuration keeps its fast-path acceptance;
    /// every other configuration resolves the verdict (absent fails closed).
    /// The immutable pre-send attempt claim fences reordered or replayed events.
    pub async fn handle_message(&self, message: &Message, verdict: Option<FunnelDisposition>) {
        let acceptance = effective_acceptance(self.acceptance, verdict);
        let guild_name = self.guild_name();
        if let Err(error) = self
            .runtime
            .handle_message(message, acceptance, self.text_commands, Some(&guild_name))
            .await
        {
            tracing::warn!(error = %error, "custom-command trigger finished without confirmed success");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_inspector_accepts_only_explicitly_disabled_automod() {
        assert_eq!(
            acceptance_without_inspector(Some("0")),
            AutomationMessageAcceptance::AutomodDisabled
        );
        for value in [
            None,
            Some("1"),
            Some(""),
            Some("false"),
            Some(" 0"),
            Some("00"),
        ] {
            assert_eq!(
                acceptance_without_inspector(value),
                AutomationMessageAcceptance::Unavailable
            );
        }
    }

    #[test]
    fn automod_verdict_maps_to_trigger_acceptance() {
        // Allow: an accepted create behaves like an unmatched inspection.
        assert_eq!(
            acceptance_for_verdict(Some(FunnelDisposition::Accept)),
            AutomationMessageAcceptance::Unmatched
        );
        assert!(acceptance_for_verdict(Some(FunnelDisposition::Accept)).permits_automations());
        // Deny: containment and unexpected edit-like verdicts never run triggers.
        assert_eq!(
            acceptance_for_verdict(Some(FunnelDisposition::CaptureOnly)),
            AutomationMessageAcceptance::CaptureOnly
        );
        assert_eq!(
            acceptance_for_verdict(Some(FunnelDisposition::None)),
            AutomationMessageAcceptance::Matched
        );
        for verdict in [
            Some(FunnelDisposition::CaptureOnly),
            Some(FunnelDisposition::None),
        ] {
            assert!(!acceptance_for_verdict(verdict).permits_automations());
        }
        // Absent: a missing verdict fails closed.
        assert_eq!(
            acceptance_for_verdict(None),
            AutomationMessageAcceptance::Unavailable
        );
        assert!(!acceptance_for_verdict(None).permits_automations());
    }

    #[test]
    fn disabled_automod_keeps_fast_path_without_a_verdict() {
        assert_eq!(
            effective_acceptance(
                AutomationMessageAcceptance::AutomodDisabled,
                Some(FunnelDisposition::Accept)
            ),
            AutomationMessageAcceptance::AutomodDisabled
        );
        assert_eq!(
            effective_acceptance(AutomationMessageAcceptance::AutomodDisabled, None),
            AutomationMessageAcceptance::AutomodDisabled
        );
        assert!(
            effective_acceptance(AutomationMessageAcceptance::AutomodDisabled, None)
                .permits_automations()
        );
        // Any other configuration resolves the verdict, and absent stays closed.
        assert_eq!(
            effective_acceptance(
                AutomationMessageAcceptance::Unavailable,
                Some(FunnelDisposition::Accept)
            ),
            AutomationMessageAcceptance::Unmatched
        );
        assert_eq!(
            effective_acceptance(AutomationMessageAcceptance::Unavailable, None),
            AutomationMessageAcceptance::Unavailable
        );
        assert!(
            !effective_acceptance(AutomationMessageAcceptance::Unavailable, None)
                .permits_automations()
        );
    }

    #[test]
    fn command_gates_use_shared_parsers_and_fail_closed() {
        let mut vars = HashMap::new();
        assert!(GatewayCommandConfig::from_map(0, &vars).is_err());
        let config = GatewayCommandConfig::from_map(1, &vars).unwrap();
        assert!(!config.gates.automations);
        assert!(!config.text_commands);
        assert!(!config.acceptance.permits_automations());
        vars.insert("TWO_AUTOMATIONS".into(), "1".into());
        vars.insert("TWO_TEXT_COMMANDS".into(), "1".into());
        vars.insert("TWO_AUTOMOD".into(), "0".into());
        let config = GatewayCommandConfig::from_map(1, &vars).unwrap();
        assert!(config.gates.automations && config.text_commands);
        assert!(config.acceptance.permits_automations());
        vars.insert("TWO_MODERATION".into(), "1".into());
        assert!(GatewayCommandConfig::from_map(1, &vars).is_err());
    }

    /// Worker decision coverage for the verdict-to-trigger plumbing
    /// (offline): pins `gateway::worker_prefix_trigger` (the text-automation
    /// gate stays on the trigger verdict, the forwarded value is the
    /// trigger verdict) and the verdict-to-acceptance mapping, then proves
    /// both directions through the real trigger handler: the denied path
    /// early-returns `Ignored` (no POST, no DB), the accepted path passes the
    /// gate and reaches the store lookup (`Storage` on the row-less lazy
    /// pool), and the reply itself is proven via the in-memory row plus the
    /// real render and the real POST.
    ///
    /// Offline: mock activation (`verdict_of` with fake `Activation`),
    /// in-memory trigger store (`HashMap`, no live DB), loopback `MockRest`
    /// for the reply POST, and lazy pools that never connect. This harness
    /// has no trigger row, so a `dispatch_with_verdict` drive can post no
    /// reply for either verdict and cannot prove the reply split; that
    /// end-to-end proof lives in
    /// `command_runtime_tests::worker_verdict_drives_prefix_trigger_from_call_site`,
    /// which seeds a `!faq` row on an isolated test database.
    #[tokio::test]
    async fn worker_prefix_trigger_decision_is_verdict_sensitive() {
        use std::time::Duration;

        use twilight_gateway::Event;
        use twilight_model::gateway::payload::incoming::MessageCreate;
        use two_bot_core::automod_runtime::{
            CompletionKind, FunnelDisposition, MessageDeliveryKind, StoredOutcome,
        };
        use two_bot_core::custom_commands::{
            accepted_text_trigger, builtin_command_names, render_template, StoredCommand,
            TemplateContext,
        };
        use two_bot_discord::automod_activation::{Activation, ActivationOutcome, RetainReason};

        use crate::automod_gateway::{runs_text_automations, verdict_of, WorkerVerdict};
        use crate::gateway::worker_prefix_trigger;

        // Mock activation: a settled clean create hands `Accept` to triggers,
        // while an uninspected create keeps funnel `Accept` but hands triggers
        // `CaptureOnly` (the split the `trigger` plumbing must preserve).
        let clean = Activation {
            disposition: FunnelDisposition::CaptureOnly,
            outcome: ActivationOutcome::Duplicate(Some(StoredOutcome {
                matched: false,
                deleted: false,
                outcome: CompletionKind::Accepted,
            })),
        };
        assert_eq!(
            verdict_of(&clean, MessageDeliveryKind::Create),
            WorkerVerdict {
                funnel: FunnelDisposition::Accept,
                trigger: FunnelDisposition::Accept,
            }
        );
        let bypassed = Activation {
            disposition: FunnelDisposition::Accept,
            outcome: ActivationOutcome::Bypassed,
        };
        assert_eq!(
            verdict_of(&bypassed, MessageDeliveryKind::Create),
            WorkerVerdict {
                funnel: FunnelDisposition::Accept,
                trigger: FunnelDisposition::CaptureOnly,
            }
        );
        let retained = Activation {
            disposition: FunnelDisposition::Accept,
            outcome: ActivationOutcome::Retained(RetainReason::CompletionRefused),
        };
        assert_eq!(
            verdict_of(&retained, MessageDeliveryKind::Create),
            WorkerVerdict {
                funnel: FunnelDisposition::Accept,
                trigger: FunnelDisposition::CaptureOnly,
            }
        );

        fn message(id: u64, author: &str, content: &str) -> twilight_model::channel::Message {
            serde_json::from_value(serde_json::json!({
                "id": id.to_string(), "guild_id": "2222", "channel_id": "4444", "type": 0,
                "author": {"id": author, "username": "tester", "discriminator": "0000", "avatar": null},
                "content": content, "timestamp": "2026-09-30T12:00:00.000000+00:00", "edited_timestamp": null,
                "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                "attachments": [], "embeds": [], "pinned": false
            }))
            .expect("valid Twilight message fixture")
        }

        fn event(message: twilight_model::channel::Message) -> Event {
            Event::MessageCreate(Box::new(MessageCreate(message)))
        }

        // Automod enabled (not the `TWO_AUTOMOD=0` fast path): bootstrap
        // acceptance fails closed without a verdict; the worker verdict decides.
        let vars = HashMap::from([
            ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
            ("TWO_TEXT_COMMANDS".to_owned(), "1".to_owned()),
            ("TWO_AUTOMOD".to_owned(), "1".to_owned()),
        ]);
        let config = GatewayCommandConfig::from_map(2222, &vars).expect("command config");
        assert_eq!(config.acceptance, AutomationMessageAcceptance::Unavailable);
        assert!(!config.acceptance.permits_automations());

        // Worker plumbing on the real helper: the gate stays on `trigger`
        // (fail-closed, so an uninspected funnel-accept never dispatches),
        // the forwarded value is the trigger verdict (not `funnel`).
        let accept_disposition = Some(WorkerVerdict {
            funnel: FunnelDisposition::Accept,
            trigger: FunnelDisposition::Accept,
        });
        let split_disposition = Some(WorkerVerdict {
            funnel: FunnelDisposition::Accept,
            trigger: FunnelDisposition::CaptureOnly,
        });
        let contained_disposition = Some(WorkerVerdict {
            funnel: FunnelDisposition::CaptureOnly,
            trigger: FunnelDisposition::CaptureOnly,
        });
        let accept_event = event(message(51, "3333", "!faq please"));
        let split_event = event(message(52, "3334", "!faq please"));
        assert_eq!(
            worker_prefix_trigger(accept_disposition, &accept_event, true),
            Some(Some(FunnelDisposition::Accept)),
            "accepted create forwards its accept trigger"
        );
        assert!(
            worker_prefix_trigger(split_disposition, &split_event, true).is_none(),
            "uninspected funnel-accept never dispatches prefix triggers"
        );
        assert!(
            worker_prefix_trigger(contained_disposition, &split_event, true).is_none(),
            "contained trigger never dispatches prefix triggers"
        );
        assert!(
            worker_prefix_trigger(accept_disposition, &accept_event, false).is_none(),
            "disabled automod never dispatches from the worker"
        );
        // A missing verdict fails closed downstream even when the gate lets an
        // unscreened create through.
        assert!(runs_text_automations(None));
        assert_eq!(
            acceptance_for_verdict(None),
            AutomationMessageAcceptance::Unavailable
        );
        assert!(!acceptance_for_verdict(None).permits_automations());

        // Verdict-to-acceptance: `Accept` permits, `CaptureOnly` refuses.
        assert!(acceptance_for_verdict(Some(FunnelDisposition::Accept)).permits_automations());
        assert!(
            !acceptance_for_verdict(Some(FunnelDisposition::CaptureOnly)).permits_automations()
        );
        assert_eq!(
            effective_acceptance(
                AutomationMessageAcceptance::Unavailable,
                Some(FunnelDisposition::Accept)
            ),
            AutomationMessageAcceptance::Unmatched
        );
        assert_eq!(
            effective_acceptance(
                AutomationMessageAcceptance::Unavailable,
                Some(FunnelDisposition::CaptureOnly)
            ),
            AutomationMessageAcceptance::CaptureOnly
        );

        // In-memory trigger store (no live DB): the `!faq` row the reply renders.
        let mut triggers: HashMap<String, StoredCommand> = HashMap::new();
        triggers.insert(
            "!faq".to_owned(),
            StoredCommand {
                guild_id: "2222".to_owned(),
                name: "faq".to_owned(),
                description: "FAQ".to_owned(),
                template: "Hi {user} {username} in {server} {channel}".to_owned(),
                text_trigger: Some("!faq".to_owned()),
                enabled: true,
            },
        );
        let builtins = builtin_command_names();
        let trigger = accepted_text_trigger(true, true, false, "!faq please", &builtins);
        assert_eq!(trigger.as_deref(), Some("!faq"));
        let row = triggers
            .get(trigger.as_deref().unwrap())
            .expect("seeded trigger");
        let rendered = render_template(
            &row.template,
            &TemplateContext {
                user: "<@3333>".to_owned(),
                username: "tester".to_owned(),
                server: "Test guild".to_owned(),
                channel: "<#4444>".to_owned(),
            },
        )
        .expect("template renders");
        assert_eq!(rendered, "Hi <@3333> tester in Test guild <#4444>");

        // Accepted create sends the prefix reply over loopback REST.
        let rest = crate::discord_test_common::MockRest::start(
            vec![crate::discord_test_common::ScriptedResponse::json(
                200,
                serde_json::json!({"id": "9000"}),
            )],
            crate::discord_test_common::ScriptedResponse::status(500),
        )
        .await;
        let executor = two_bot_discord::ActionExecutor::with_proxy(
            "test-token".to_owned(),
            Some(rest.origin()),
        )
        .expect("mock executor");
        executor
            .post_message("4444", &rendered, Some(51))
            .await
            .expect("loopback reply posts");
        let posts: Vec<_> = rest
            .requests()
            .into_iter()
            .filter(|request| {
                request.method == "POST" && request.path.ends_with("/channels/4444/messages")
            })
            .collect();
        assert_eq!(posts.len(), 1, "accept posts exactly one prefix reply");
        let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
        assert_eq!(body["content"], rendered);
        rest.shutdown().await;

        // Denied create never reaches the store or the wire: the real trigger
        // handler early-returns `Refused` before any DB lookup or POST, even
        // with a lazy pool that could never serve one.
        let denied_rest = crate::discord_test_common::MockRest::start(
            Vec::new(),
            crate::discord_test_common::ScriptedResponse::status(500),
        )
        .await;
        // Offline harness: the closed loopback port may drop SYNs on hosted CI,
        // so the default 30 s acquire would outlive the settle window below.
        // Fail fast instead (same for the accepted/dispatch pools).
        let denied_pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool");
        let denied_router = std::sync::Arc::new(two_bot_core::InteractionRouter::new(
            two_bot_core::RouterGates {
                configured_guild: Some(2222),
                automations: true,
                moderation: false,
                voice: false,
                voice_assistant: false,
                scorecard: false,
                announcements: false,
                tickets: false,
                self_roles: false,
                onboarding_picker: false,
                session_picker: false,
            },
        ));
        // Authoritative empty custom-command fixture, like the runtime tests.
        let mut router_with_custom = two_bot_core::InteractionRouter::new(denied_router.gates());
        two_bot_discord::custom_commands::CustomCommandRuntime::register(&mut router_with_custom);
        let denied_runtime = two_bot_discord::custom_commands::CustomCommandRuntime::new(
            denied_pool,
            std::sync::Arc::new(router_with_custom),
            two_bot_discord::ActionExecutor::with_proxy(
                "test-token".to_owned(),
                Some(denied_rest.origin()),
            )
            .expect("mock executor"),
            1111,
        );
        let denied_outcome = denied_runtime
            .handle_message(
                &message(52, "3334", "!faq please"),
                acceptance_for_verdict(Some(FunnelDisposition::CaptureOnly)),
                true,
                Some("Test guild"),
            )
            .await
            .expect("denied trigger returns");
        assert_eq!(
            denied_outcome,
            two_bot_discord::custom_commands::TextCommandOutcome::Refused
        );
        assert!(
            denied_rest.requests().is_empty(),
            "capture-only sends no prefix reply and needs no DB"
        );
        denied_rest.shutdown().await;

        // Accepted verdict passes the same real gate: with a lazy pool it
        // reaches the store lookup and reports `Storage` instead of `Refused`,
        // proving it was not refused before the in-memory row above would reply.
        let accepted_rest = crate::discord_test_common::MockRest::start(
            Vec::new(),
            crate::discord_test_common::ScriptedResponse::status(500),
        )
        .await;
        let accepted_pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool");
        let accepted_router = std::sync::Arc::new(two_bot_core::InteractionRouter::new(
            two_bot_core::RouterGates {
                configured_guild: Some(2222),
                automations: true,
                moderation: false,
                voice: false,
                voice_assistant: false,
                scorecard: false,
                announcements: false,
                tickets: false,
                self_roles: false,
                onboarding_picker: false,
                session_picker: false,
            },
        ));
        let mut accepted_router_with_custom =
            two_bot_core::InteractionRouter::new(accepted_router.gates());
        two_bot_discord::custom_commands::CustomCommandRuntime::register(
            &mut accepted_router_with_custom,
        );
        let accepted_runtime = two_bot_discord::custom_commands::CustomCommandRuntime::new(
            accepted_pool,
            std::sync::Arc::new(accepted_router_with_custom),
            two_bot_discord::ActionExecutor::with_proxy(
                "test-token".to_owned(),
                Some(accepted_rest.origin()),
            )
            .expect("mock executor"),
            1111,
        );
        let accepted_result = accepted_runtime
            .handle_message(
                &message(53, "3335", "!faq please"),
                acceptance_for_verdict(Some(FunnelDisposition::Accept)),
                true,
                Some("Test guild"),
            )
            .await;
        assert!(
            matches!(
                accepted_result,
                Err(two_bot_discord::custom_commands::CustomCommandError::Storage)
            ),
            "accept reaches the store lookup (in-memory row above would reply), got {accepted_result:?}"
        );
        assert!(
            accepted_rest.requests().is_empty(),
            "no reply without the row; the in-memory POST above proves the send"
        );
        accepted_rest.shutdown().await;
    }
}
