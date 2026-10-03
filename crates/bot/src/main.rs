//! two-bot-next container entrypoint.
//!
//! Serves liveness (`/health`) and readiness (`/readyz`) and runs the gateway
//! shard supervisor (S3). Without `DISCORD_TOKEN`, `DATABASE_URL`, or a nonzero
//! `GUILD_ID` the shard stays parked and `/readyz` reports `gateway: down`
//! (HTTP 503) — the Container boots healthy on incomplete staging config.

#[cfg(test)]
mod admission_test_support;
mod audit_runtime;
mod automod_gateway;
mod backup_cli;
mod command_runtime;
#[cfg(test)]
mod command_runtime_tests;
mod commands_cli;
mod community_jobs;
mod containment_runtime;
#[cfg(test)]
mod containment_runtime_tests;
mod database_roles_cli;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod discord_test_common;
mod dispatch;
mod erasure_cli;
mod gateway;
mod gateway_failure;
mod gateway_metrics;
#[cfg(test)]
mod gateway_tests;
mod internal_action_http;
mod jobs;
mod join_risk_runtime;
#[cfg(test)]
mod join_risk_runtime_tests;
#[cfg(test)]
mod lifecycle_tests;
mod metrics_http;
mod moderation_cli;
mod onboarding;
#[cfg(test)]
mod onboarding_tests;
mod preflight;
mod raid_runtime;
#[cfg(test)]
mod raid_runtime_tests;
mod restore_drill;
mod schedule_runtime;
mod scheduled_jobs;
mod server;
// TOG-10292: boot composes the gated service below; fixture-only seams keep
// the module-level allowance.
#[allow(dead_code)]
mod self_role_handlers;
#[allow(dead_code)]
mod self_role_runtime;
mod shutdown;
#[cfg(test)]
mod smoke_error_contract_tests;
mod ticket_runtime;
#[cfg(test)]
#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;
mod website_jobs;

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;
use two_bot_core::{ComponentStatus, Config, VoiceGates};

use futures_util::FutureExt as _;
use gateway::{
    build_persistent_pipeline, build_shard, build_voice_runtime, ensure_crypto_provider,
    intents_from_env, run_shard, GatewayState,
};
use gateway_failure::{step_failure, FailureClass, FailureSlot, GatewayFailure};
use server::SharedState;
use website_jobs::serve;

#[tokio::main]
async fn main() {
    ensure_crypto_provider();
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if cli_args.first().is_some_and(|arg| arg == "preflight") {
        std::process::exit(preflight::dispatch(&cli_args[1..]).await);
    }
    if cli_args.first().is_some_and(|arg| arg == "moderation") {
        std::process::exit(moderation_cli::dispatch(&cli_args[1..]).await);
    }
    // Docker HEALTHCHECK probe: GET /health on the configured port and exit
    // 0/1. Kept dependency-free (std + tokio only) so the check path cannot
    // rot behind an HTTP-client upgrade.
    if std::env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(healthcheck().await);
    }

    // Operator CLI (TOG-9881): backup/restore + sealed guild-config snapshot.
    // No subcommand falls through to the gateway path below. sqlx is linked
    // (core `db` feature) so these paths can open Postgres directly.
    if cli_args.first().is_some_and(|arg| arg == "commands") {
        std::process::exit(commands_cli::dispatch(&cli_args[1..]).await);
    }
    if !cli_args.is_empty() && cli_args[0] != "--help" && cli_args[0] != "-h" {
        let code = backup_cli::dispatch(&cli_args).await;
        // 100 = not a backup subcommand: fall through to serve.
        if code != 100 {
            std::process::exit(code);
        }
    } else if !cli_args.is_empty() {
        print_backup_help_and_exit().await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "two_bot=info".into()),
        )
        .init();

    let receiver_config = two_bot_core::internal_action_config::InternalActionConfig::from_env()
        .unwrap_or_else(|_| {
            tracing::error!(
                error_class = "receiver_config_invalid",
                "internal-action startup refused"
            );
            std::process::exit(1);
        });
    let config = Config::from_env().unwrap_or_else(|err| {
        if receiver_config.is_some() {
            tracing::error!(
                error_class = "receiver_prerequisites_invalid",
                "internal-action startup refused"
            );
            std::process::exit(1);
        }
        tracing::warn!(error = %err, "config invalid; continuing with safe defaults");
        Config {
            discord_token: None,
            database_url: None,
            // Gateway config errors must not move the listener away from
            // the Worker probes and Docker healthcheck's configured address.
            listen_addr: std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned()),
            guild_id: None,
        }
    });

    if receiver_config.is_some() && internal_receiver_prerequisites(&config).is_err() {
        tracing::error!(
            error_class = "receiver_prerequisites_invalid",
            "internal-action startup refused"
        );
        std::process::exit(1);
    }
    let gateway = Arc::new(RwLock::new(GatewayState::new(&config)));
    let listener = server::bind(&config.listen_addr).await.unwrap_or_else(|_| {
        tracing::error!(
            startup_phase = "listener_bind",
            error_class = "listener_bind_failed",
            "container listener failed"
        );
        std::process::exit(1);
    });
    let gateway_url = match std::env::var("DISCORD_GATEWAY_URL") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::error!(
                startup_phase = "gateway_override",
                error_class = "gateway_override_invalid",
                "DISCORD_GATEWAY_URL must be valid UTF-8"
            );
            std::process::exit(1);
        }
    };
    if gateway_url
        .as_deref()
        .is_some_and(|url| !gateway::is_loopback_gateway(url))
    {
        tracing::error!(
            startup_phase = "gateway_override",
            error_class = "gateway_override_invalid",
            "DISCORD_GATEWAY_URL must be a loopback mock websocket address"
        );
        std::process::exit(1);
    }

    // Opt-in boot command registry sync (TOG-10860) runs before opening the
    // gateway database so a configured registry is never skipped by a later
    // DB failure; refusal/failure exits before shard startup. Opt-out is a
    // no-op and preserves existing server behavior.
    if let Ok((token, _, guild_id)) = gateway_prerequisites(&config) {
        if let Err(error) = commands_cli::publish_on_boot(token, guild_id).await {
            tracing::error!(error = %error, "boot command registry synchronization failed");
            std::process::exit(1);
        }
    }

    let store = match gateway_prerequisites(&config).ok().map(|(_, url, _)| url) {
        Some(url) => match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            // Runtime is DML-only; the operator applies both store and gateway
            // migrations and the web contract before startup.
            two_bot_store::Store::connect(url, true),
        )
        .await
        {
            Ok(Ok(store)) => {
                metrics_http::register_pool(store.pool().clone());
                Some(store)
            }
            _ => {
                tracing::error!(
                    startup_phase = "database_init",
                    error_class = "database_connect_failed",
                    "database initialization failed; exiting for supervisor restart"
                );
                std::process::exit(1);
            }
        },
        None => None,
    };
    let state = SharedState::new(
        Arc::clone(&gateway),
        store.as_ref().map(|s| s.pool().clone()),
    );

    // Disable guard: refuse to boot with moderation/automation disabled
    // while releases are still owed (pending tempban unbans, active
    // lockdowns, enabled scheduled messages). Enabled gates short-circuit without a
    // database read; the explicit override proceeds and is logged loudly.
    // Without a database there is no owed state to read.
    if let Some(pool) = store.as_ref().map(|s| s.pool().clone()) {
        let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
        let gates = two_bot_core::disable_preflight::DisableGates::from_map(&vars);
        if !gates.moderation || !gates.automations {
            let overridden = two_bot_core::disable_preflight::override_active(&vars, &cli_args);
            match two_bot_core::disable_preflight::boot_check(&pool, &gates, overridden).await {
                Ok(two_bot_core::disable_preflight::BootVerdict::Proceed) => {}
                Ok(two_bot_core::disable_preflight::BootVerdict::Refused(owed)) => {
                    tracing::error!(
                        owed = %owed.report(),
                        "moderation_disable_refused: boot refused with moderation/automation disabled while releases are owed; complete or cancel them, or set TWO_ALLOW_OWED_RELEASES=1 to override"
                    );
                    std::process::exit(1);
                }
                Ok(two_bot_core::disable_preflight::BootVerdict::Overridden(owed)) => {
                    tracing::warn!(
                        owed = %owed.report(),
                        "moderation_disable_override: booting with moderation/automation disabled while releases are owed; members may stay banned and channels locked"
                    );
                }
                Err(_) => {
                    tracing::error!(
                        error_class = "moderation_disable_unknown",
                        "moderation_disable_refused: owed-release state unreadable while moderation/automation is disabled; refusing boot"
                    );
                    std::process::exit(1);
                }
            }
        }
    }

    // Capture the authoritative pool before the gateway's async move owns it.
    // Bind privately before starting tasks; enabled failures never fall back.
    let receiver = match receiver_config {
        Some(receiver_config) => {
            let token =
                internal_receiver_prerequisites(&config).expect("validated receiver prerequisites");
            let pool = store
                .as_ref()
                .expect("required receiver store")
                .pool()
                .clone();
            Some(
                internal_action_http::bind(receiver_config, pool, token)
                    .await
                    .unwrap_or_else(|_| {
                        tracing::error!(
                            error_class = "receiver_bind_failed",
                            "internal-action startup refused"
                        );
                        std::process::exit(1);
                    }),
            )
        }
        None => None,
    };

    // ONE optional self-role service: gateway dispatch and the supervised
    // recovery job share this Arc. Empty/invalid catalogues, non-staging guilds
    // and failed identity reads leave the surface and the job unregistered.
    let self_roles = match (gateway_prerequisites(&config), store.as_ref()) {
        (Ok((token, _, guild_id)), Some(db)) => {
            self_role_handlers::SelfRoleService::from_env(db.pool().clone(), token, guild_id).await
        }
        _ => None,
    };
    // V1 voice rooms: per-guild lifecycle actors fed by the gateway sink.
    // Inert unless TWO_VOICE=1 with token + database present; any failure
    // degrades to voice-off with a warn, never a boot failure.
    let voice = build_voice_runtime(&config, VoiceGates::from_env().enabled).await;

    let (shutdown, stopping) = tokio::sync::watch::channel(false);
    // Filled by the gateway task; read by the shared maintenance tick.
    let automod_slot: automod_gateway::Slot = Arc::default();
    let gateway_task = if let Ok((token, url, guild_id)) = gateway_prerequisites(&config) {
        let token = token.to_owned();
        let url = url.to_owned();
        let failure_slot = state.failure.clone();
        let state = Arc::clone(&gateway);
        let slot = Arc::clone(&automod_slot);
        let self_roles = self_roles.clone();
        let linger_stop = stopping.clone();
        Some(tokio::spawn(async move {
            // A panic is caught only to name it on /readyz; the task still ends
            // in `Err`, which the supervisor treats exactly like a JoinError.
            let outcome: Result<Result<(), gateway_failure::StepFailure>, _> =
                std::panic::AssertUnwindSafe(async {
                    // Opt-in registry sync already ran before Store::connect; the
                    // gateway task proceeds directly to checkpoint/shard startup.
                    let db = store.ok_or_else(|| {
                        step_failure(
                            FailureClass::StoreUnavailable,
                            sqlx::Error::InvalidArgument(
                                "DATABASE_URL required for gateway checkpoint".into(),
                            ),
                        )
                    })?;
                    // Feature work (onboarding, shared commands) holds connections
                    // across Discord I/O: keep the ordered checkpoint writer on its
                    // own single-connection pool so it can never be starved.
                    let gateway_db =
                        two_bot_cutover::connect(&url, gateway::GATEWAY_POOL_MAX, true)
                            .await
                            .map_err(|error| {
                                step_failure(FailureClass::GatewayPoolConnectFailed, error)
                            })?;
                    let pool = db.pool().clone();
                    let store = two_bot_cutover::gateway_session::GatewaySessionStore::new(
                        gateway_db.pool().clone(),
                        guild_id.to_string(),
                        0,
                    );
                    let saved = gateway::load_boot_session(&store)
                        .await
                        .map_err(|error| step_failure(FailureClass::CheckpointLoadFailed, error))?;
                    let gates = two_bot_core::OnboardingGates::from_env().map_err(|_| {
                        step_failure(
                            FailureClass::OnboardingGatesInvalid,
                            sqlx::Error::InvalidArgument("invalid onboarding mode".into()),
                        )
                    })?;
                    // ONE router + REST executor + sqlx stores over the feature pool.
                    // Bad command env gates still park only the command surface.
                    // The ordered leveling path shares this runtime's
                    // executor/pacing for XP awards and role rewards.
                    let runtime = command_runtime::CommandRuntime::from_env(
                        pool.clone(),
                        &token,
                        guild_id,
                        self_roles,
                        gates,
                    );
                    // Onboarding renders through that same executor: one shared
                    // admission lane and pacing, never a private Discord client.
                    // Its identity probe honors the mock REST seam through the
                    // executor's `DISCORD_API_BASE` proxy. A parked command
                    // runtime (bad env gates) parks onboarding too.
                    let onboarding = match runtime.as_ref() {
                        Some(runtime) => Some(Arc::new(
                            onboarding::OnboardingRuntime::from_env(
                                pool.clone(),
                                runtime.executor(),
                                guild_id,
                            )
                            .await
                            .map_err(|_| {
                                step_failure(
                                    FailureClass::OnboardingInitFailed,
                                    sqlx::Error::InvalidArgument(
                                        "onboarding initialization failed".into(),
                                    ),
                                )
                            })?,
                        )),
                        None => {
                            tracing::warn!("command runtime parked; onboarding runtime disabled");
                            None
                        }
                    };
                    let leveling = runtime.as_ref().map(|runtime| runtime.leveling());
                    let pipeline = Arc::new(
                        build_persistent_pipeline(&store, guild_id, token.clone(), leveling)
                            .await
                            .map_err(|error| {
                                step_failure(FailureClass::MilestonesLoadFailed, error)
                            })?,
                    );
                    // Join-burst watch is always on, as legacy raid watch was (TWO-56),
                    // and independent of the anti-nuke flags. It shares the command
                    // runtime's REST executor and runs behind the funnel's join row.
                    let raid_executor = match runtime.as_ref() {
                        Some(runtime) => runtime.executor(),
                        None => two_bot_discord::ActionExecutor::with_proxy(
                            token.clone(),
                            std::env::var("DISCORD_API_BASE")
                                .ok()
                                .filter(|value| !value.is_empty()),
                        )
                        .map_err(|_| {
                            step_failure(
                                FailureClass::RaidExecutorFailed,
                                sqlx::Error::InvalidArgument("raid REST executor failed".into()),
                            )
                        })?,
                    };
                    let raid =
                        raid_runtime::start_from_env(pool.clone(), raid_executor.clone(), guild_id);
                    // Join-risk delivery (R2) shares the raid observer slot:
                    // the pipeline takes one observer, so risk chains behind
                    // the raid watch and both stay behind the funnel's join
                    // row. Without exact TWO_ANTI_NUKE=1 on the staging guild
                    // the chain is the raid watch alone.
                    pipeline.set_join_observer(join_risk_runtime::chain_from_env(
                        pool.clone(),
                        raid_executor.clone(),
                        guild_id,
                        raid,
                    ));
                    // Containment (R3) watches the audit-log entry slot, a
                    // separate observer from the join slot above. Without
                    // exact TWO_ANTI_NUKE=1 on the staging guild there is no
                    // observer at all; dry-run is the default and only an
                    // armed worker executes removals.
                    if let Some(containment) =
                        containment_runtime::start_from_env(pool.clone(), raid_executor, guild_id)
                    {
                        pipeline.set_audit_entry_observer(containment);
                    }
                    // Automod shares the command runtime's REST executor; it never
                    // builds a private client, router or timer.
                    let vars: std::collections::HashMap<String, String> =
                        std::env::vars().collect();
                    let automod =
                        match automod_gateway::resolve(&vars, guild_id).map_err(|reason| {
                            tracing::error!(reason, "automod configuration rejected");
                            step_failure(
                                FailureClass::AutomodConfigInvalid,
                                sqlx::Error::InvalidArgument(reason.into()),
                            )
                        })? {
                            Some(resolved) => {
                                let executor = match runtime.as_ref() {
                                    Some(runtime) => runtime.executor(),
                                    None => two_bot_discord::ActionExecutor::with_proxy(
                                        token.clone(),
                                        std::env::var("DISCORD_API_BASE")
                                            .ok()
                                            .filter(|value| !value.is_empty()),
                                    )
                                    .map_err(|_| {
                                        step_failure(
                                            FailureClass::AutomodExecutorFailed,
                                            sqlx::Error::InvalidArgument(
                                                "automod REST executor failed".into(),
                                            ),
                                        )
                                    })?,
                                };
                                let automod = automod_gateway::build(resolved, pool, executor);
                                let _ = slot.set(Arc::clone(&automod));
                                info!("automod activation wired into the gateway loop");
                                Some(automod)
                            }
                            None => None,
                        };
                    // V4 `kick` collision: when the voice sink owns a kick
                    // target (tracked room), the router yields so the vote
                    // is answered exactly once. Both runtimes exist only
                    // inside this task, so the claim wires here.
                    if let (Some(runtime), Some(voice)) = (runtime.as_ref(), voice.as_ref()) {
                        let voice = Arc::clone(voice);
                        runtime.set_voice_kick_claim(Arc::new(move |guild, member| {
                            let voice = Arc::clone(&voice);
                            Box::pin(async move { voice.kick_claim_room(guild, member).await })
                        }));
                    }
                    let shard = build_shard(
                        token,
                        intents_from_env(),
                        saved.as_ref(),
                        gateway_url.as_deref(),
                    );
                    info!(
                        resume = saved.is_some(),
                        "durable gateway initialized; shard connecting"
                    );
                    run_shard(
                        shard,
                        pipeline,
                        Arc::clone(&state),
                        store,
                        onboarding,
                        runtime,
                        automod,
                        voice,
                        async move {
                            server::shutdown_requested(stopping).await;
                        },
                    )
                    .await
                    .map_err(|error| step_failure(FailureClass::GatewayRuntimeFailed, error))
                })
                .catch_unwind()
                .await;
            let failed = match outcome {
                Ok(Ok(())) => None,
                Ok(Err(failed)) => Some(failed),
                Err(_) => Some(step_failure(
                    FailureClass::GatewayTaskPanicked,
                    sqlx::Error::InvalidArgument("gateway task panicked".into()),
                )),
            };
            match failed {
                None => Ok(()),
                Some(failed) => {
                    publish_gateway_failure(
                        &state,
                        &failure_slot,
                        failed.class,
                        linger_stop,
                        shutdown::FAILURE_LINGER,
                    )
                    .await;
                    Err(failed.error)
                }
            }
        }))
    } else {
        *gateway.write().await = GatewayState::Unconfigured;
        info!(
            missing = gateway_prerequisites(&config).unwrap_err(),
            status = ?ComponentStatus::Down,
            "gateway prerequisites missing; gateway parked, /readyz reports down"
        );
        None
    };

    let http = serve(
        &config,
        listener,
        state,
        shutdown.clone(),
        self_roles,
        automod_slot,
        receiver,
    );
    let result = match gateway_task {
        Some(task) => supervise_gateway(task, http, gateway, shutdown).await,
        None => http.await,
    };
    if result.is_err() {
        tracing::error!(
            startup_phase = "service_supervisor",
            error_class = "container_service_failed",
            "container service failed"
        );
        std::process::exit(1);
    }
}

/// This receiver slice is staging-only, not authority to enable production.
fn internal_receiver_prerequisites(config: &Config) -> Result<&str, &'static str> {
    let (token, _, guild) = gateway_prerequisites(config)?;
    if guild.to_string() != two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID {
        return Err("internal-action receiver requires the staging guild");
    }
    Ok(token)
}

/// Publish why the gateway task stopped, then keep serving `/readyz` for
/// `linger` before the task returns (and the supervisor drains and exits). The
/// Worker's 60 s keepalive and the rollout gate's 5 s poll can only report the
/// class while the listener is still up. A shutdown request ends the linger at
/// once, so SIGTERM never waits on it. Only the fixed class is logged or
/// stored: SQLx errors can carry a connection URL.
async fn publish_gateway_failure(
    state: &RwLock<GatewayState>,
    slot: &FailureSlot,
    class: FailureClass,
    stopping: tokio::sync::watch::Receiver<bool>,
    linger: std::time::Duration,
) {
    tracing::error!(
        startup_phase = gateway_failure::FailurePhase::DurableGateway.as_str(),
        error_class = class.as_str(),
        "durable gateway failed; checkpoint unchanged, readiness unavailable"
    );
    slot.record(GatewayFailure::durable_gateway(class));
    {
        let mut state = state.write().await;
        if *state != GatewayState::Draining {
            *state = GatewayState::Armed;
        }
    }
    tokio::select! {
        biased;
        _ = server::shutdown_requested(stopping) => {}
        _ = tokio::time::sleep(linger) => {}
    }
}

/// `--help` covers the gateway server and both operator CLI surfaces.
// Stdout lives in the CLI modules; the entrypoint only dispatches.
async fn print_backup_help_and_exit() -> ! {
    commands_cli::print_all_usage();
    backup_cli::print_server_usage();
    let code = backup_cli::dispatch(&["--help".to_owned()]).await;
    std::process::exit(code);
}

/// Validate before spawning: missing bindings park the gateway, whereas a
/// configured task's failures remain fatal. Errors contain binding names only.
fn gateway_prerequisites(config: &Config) -> Result<(&str, &str, u64), &'static str> {
    let token = config
        .discord_token
        .as_ref()
        .map(|secret| secret.expose().as_str())
        .filter(|token| !token.is_empty())
        .ok_or("DISCORD_TOKEN")?;
    let url = config
        .database_url
        .as_ref()
        .map(|secret| secret.expose().as_str())
        .filter(|url| !url.is_empty())
        .ok_or("DATABASE_URL")?;
    let guild_id = config.guild_id.filter(|id| *id != 0).ok_or("GUILD_ID")?;
    Ok((token, url, guild_id))
}

/// A configured gateway is essential: never leave a health-only zombie after
/// initialization/dispatch failure, stream termination, or a task panic. Exit
/// nonzero so the Container supervisor can restart from the committed checkpoint.
/// Source: <https://docs.rs/tokio/1/tokio/macro.select.html#cancellation-safety>
async fn supervise_gateway(
    task: tokio::task::JoinHandle<Result<(), sqlx::Error>>,
    http: impl std::future::Future<Output = std::io::Result<()>>,
    state: Arc<RwLock<GatewayState>>,
    shutdown: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    supervise_gateway_bounded(task, http, state, shutdown, shutdown::deadline()).await
}

async fn supervise_gateway_bounded(
    mut task: tokio::task::JoinHandle<Result<(), sqlx::Error>>,
    http: impl std::future::Future<Output = std::io::Result<()>>,
    state: Arc<RwLock<GatewayState>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    shutdown_max: std::time::Duration,
) -> std::io::Result<()> {
    let stopping = shutdown.subscribe();
    futures_util::pin_mut!(http);
    let (http_result, gateway_stopped) = tokio::select! {
        biased;
        _ = server::shutdown_requested(stopping) => (None, false),
        // Check the essential task before the first HTTP poll: cancellation
        // must be sticky even if the HTTP/job owner has not subscribed yet.
        // Never expose task/SQL errors: they may contain connection secrets.
        _ = &mut task => (None, true),
        result = &mut http => (Some(result), false),
    };
    *state.write().await = GatewayState::Draining;
    shutdown.send_replace(true);
    // Retain the gateway JoinHandle through HTTP/job cleanup, never abort its
    // non-cancellable blocking writer. Poll both drains so neither failure can
    // skip the other's cleanup. A fatal deadline returns Err; main exits rather
    // than waiting for Tokio to shut down a detached blocking writer.
    tokio::time::timeout(shutdown_max, async {
        let drain_http = async {
            match http_result {
                Some(result) => result,
                None => http.await,
            }
        };
        if gateway_stopped {
            // The handle was already consumed by select; do not poll it twice.
            let _ = drain_http.await;
            return Err(std::io::Error::other(
                "gateway task stopped; container restart required",
            ));
        }
        let (gateway_result, http_result) = tokio::join!(&mut task, drain_http);
        match gateway_result {
            Ok(Ok(())) => http_result,
            _ => Err(std::io::Error::other(
                "gateway drain failed; restart required",
            )),
        }
    })
    .await
    .unwrap_or_else(|_| {
        tracing::error!(
            deadline_ms = shutdown_max.as_millis() as u64,
            "shutdown_deadline_exceeded: abandoning in-flight work"
        );
        Err(std::io::Error::other("service shutdown deadline exceeded"))
    })
}

/// Probe /health over plain HTTP using only tokio (no client dependency).
/// Returns the process exit code: 0 when the local server answers 200.
async fn healthcheck() -> i32 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    // Inside the container 0.0.0.0 is not diallable; loopback is equivalent.
    let addr = addr.replacen("0.0.0.0", "127.0.0.1", 1);
    let mut stream = match tokio::net::TcpStream::connect(&addr).await {
        Ok(stream) => stream,
        Err(_) => return 1,
    };
    let request = b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return 1;
    }
    let mut response = Vec::with_capacity(128);
    if stream.read_to_end(&mut response).await.is_err() {
        return 1;
    }
    if response.starts_with(b"HTTP/1.0 200") || response.starts_with(b"HTTP/1.1 200") {
        0
    } else {
        1
    }
}
