//! Runtime adapters for the three existing website-contract domains, plus the
//! `guild_settings` hot-reload poll registered alongside them (TOG-10898).

use std::{collections::HashMap, sync::Arc, time::Duration};

use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tokio::sync::{watch, Mutex, OnceCell};
use two_bot_core::{
    build_community_snapshot, build_counter_reading,
    database_tls::{self, TlsPolicy},
    diagnose_rank_roles, match_rank_roles, normalize_events, now_iso, plan_rank_heal,
    read_raid_windows, replace_events,
    voice_create_admission::CreateAdmissionConfig,
    write_counter, write_rank_snapshot, Config, JobGate, RankHeal, RankRungProblem,
    RawScheduledEvent, RosterMember, ScheduledEventsSkip, WebsiteStoreError,
    LIVE_COUNTER_INTERVAL_MS, RANK_SNAPSHOT_INTERVAL_MS, SCHEDULED_EVENTS_INTERVAL_MS,
};
use two_bot_discord::executor::{ActionExecutor, DiscordError};
use two_bot_discord::internal_events::canonical_snowflake;

use crate::{
    activation::BootActivation,
    audit_runtime, community_jobs, feed_jobs,
    jobs::{self, ErrorClass, Job},
    member_runtime::{self, MemberRuntime},
    scheduled_jobs,
    self_role_handlers::{SelfRoleService, RECOVERY_JOB_NAME},
    server,
};

/// Every supervised job name, in `/readyz` order. The settings poll needs
/// only the database, so it is registered separately from the REST-backed
/// jobs below.
pub const NAMES: [&str; 5] = [
    "counter",
    "rank",
    "scheduled_events",
    "event_voice",
    crate::settings_jobs::NAME,
];

#[path = "event_voice_create_runtime.rs"]
mod event_voice_create_runtime;
#[path = "event_voice_preflight.rs"]
mod event_voice_preflight;
#[path = "event_voice_reconcile.rs"]
mod event_voice_reconcile;
#[path = "event_voice_source.rs"]
mod event_voice_source;

#[cfg(test)]
#[path = "event_voice_production_tick_tests.rs"]
mod event_voice_production_tick_tests;
#[cfg(test)]
#[path = "event_voice_reconcile_runtime_tests.rs"]
mod event_voice_reconcile_runtime_tests;
#[path = "event_voice_ticker.rs"]
mod event_voice_ticker;

#[cfg(test)]
#[path = "website_jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "event_voice_category_runtime_tests.rs"]
mod event_voice_category_runtime_tests;

#[cfg(test)]
#[path = "website_snapshot_tests.rs"]
mod snapshot_tests;

#[cfg(test)]
#[path = "website_rank_repair_acceptance_tests.rs"]
mod rank_repair_acceptance_tests;

#[cfg(test)]
#[path = "website_event_identity_acceptance_tests.rs"]
mod event_identity_acceptance_tests;

#[cfg(test)]
#[path = "website_jobgate_acceptance_tests.rs"]
mod jobgate_acceptance_tests;

#[cfg(test)]
#[path = "website_events_dispatch_tests.rs"]
mod events_dispatch_tests;

#[cfg(test)]
#[path = "website_event_voice_plumbing_tests.rs"]
mod event_voice_plumbing_tests;

#[cfg(test)]
#[path = "event_voice_preflight_tests.rs"]
mod event_voice_preflight_tests;

#[cfg(test)]
#[path = "event_voice_context_tests.rs"]
mod event_voice_context_tests;

#[cfg(test)]
#[path = "event_voice_create_runtime_tests.rs"]
mod event_voice_create_runtime_tests;

#[cfg(test)]
#[path = "event_voice_source_tests.rs"]
mod event_voice_source_tests;

#[cfg(test)]
#[path = "event_voice_source_runtime_tests.rs"]
mod event_voice_source_runtime_tests;

#[derive(Clone, Copy)]
pub enum Kind {
    Counter,
    Rank,
    Events,
    EventVoice,
}

impl Kind {
    /// Snapshot tick seam; the production adapter supplies durable claims and
    /// worker-guarded deletes through the same decision engine.
    ///
    /// Test-only: the pinned acceptance tests drive this entry. Production
    /// drives the same engine via `run_claimed` with durable claims (the
    /// card-1 claim-before-Discord procedure), which this snapshot signature
    /// cannot express: it takes no pool, and the production continuation
    /// already holds the preflight gate this seam would re-acquire.
    #[cfg(test)]
    pub async fn run_event_voice_tick(
        rest: &ActionExecutor,
        guild: &str,
        events: &[Value],
        category_id: Option<&str>,
        now_ms: i64,
        shutdown: &watch::Receiver<bool>,
        gate: &JobGate,
    ) -> Result<Value, ErrorClass> {
        event_voice_ticker::run(rest, guild, events, category_id, now_ms, shutdown, gate).await
    }

    /// Whole-tick Events entry point with [`JobGate`] single-flight: a tick
    /// that cannot acquire the gate skips at once (`Ok`, no fetch, no
    /// publish, no queue, no error); otherwise the guard is held across the
    /// delegated tick, so overlapping ticks cannot both run. Counter and rank
    /// ticks never take this path.
    pub async fn run_events_tick(
        pool: &PgPool,
        rest: &ActionExecutor,
        guild: &str,
        observation: &Mutex<()>,
        shutdown: &watch::Receiver<bool>,
        rank_heal: bool,
        gate: &JobGate,
    ) -> Result<(), ErrorClass> {
        let Some(_guard) = gate.try_acquire() else {
            return Ok(());
        };
        snapshot_once(
            Kind::Events,
            pool,
            rest,
            guild,
            observation,
            shutdown,
            rank_heal,
        )
        .await
    }
}

/// Cadence overrides are compiled only into tests, never the deployed binary.
fn cadence(kind: Kind) -> Duration {
    let millis = match kind {
        Kind::Counter => LIVE_COUNTER_INTERVAL_MS,
        Kind::Rank => RANK_SNAPSHOT_INTERVAL_MS,
        Kind::Events => SCHEDULED_EVENTS_INTERVAL_MS,
        Kind::EventVoice => event_voice_ticker::INTERVAL_MS,
    };
    #[cfg(test)]
    let millis = {
        let name = match kind {
            Kind::Counter => "TWO_TEST_COUNTER_INTERVAL_MS",
            Kind::Rank => "TWO_TEST_RANK_INTERVAL_MS",
            Kind::Events => "TWO_TEST_EVENTS_INTERVAL_MS",
            Kind::EventVoice => "TWO_TEST_EVENT_VOICE_INTERVAL_MS",
        };
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(millis)
    };
    Duration::from_millis(millis)
}

/// Shared REST/DB context: website jobs also read `observation`; the community
/// jobs in [`crate::community_jobs`] reuse the pool, executor and guild but
/// hold their own lanes.
pub(crate) struct Context {
    pub(crate) url: String,
    pub(crate) pool: OnceCell<PgPool>,
    pub(crate) rest: ActionExecutor,
    pub(crate) guild: String,
    observation: Mutex<()>,
    events_gate: JobGate,
    event_voice: event_voice_preflight::Tick,
    /// Supervised-guild 429 pacing for the fresh source read; a source GET's
    /// backoff must survive across ticks without pausing other routes.
    event_voice_source: event_voice_source::Reader,
    /// Supervised-guild 429 pacing for binding reconciliation GETs.
    event_voice_reconcile: event_voice_reconcile::Reconciler,
    /// The boot-composed voice sink, cloned before `main` moves it into the
    /// gateway task. The T-15 ticker reads live gateway evidence through it;
    /// `None` (voice off) fails closed, never as an empty grant.
    voice: Option<Arc<dyn two_bot::voice_rooms::VoiceEventSink>>,
}

impl Context {
    pub(crate) async fn pool(&self) -> Result<&PgPool, ErrorClass> {
        self.pool
            .get_or_try_init(|| async {
                let db = two_bot_cutover::connect(
                    &self.url,
                    two_bot_cutover::DB_POOL_MAX_DEFAULT,
                    true, // Operator provisions migrations/views; runtime is DML-only.
                )
                .await
                .map_err(|_| ErrorClass::Database)?;
                Ok(db.pool().clone())
            })
            .await
    }

    /// The gateway's live evidence handle for event-voice preflight, if the
    /// boot-composed voice sink owns one. `None` (voice off, no actor) fails
    /// closed; the tick treats it as unavailable, never as an empty grant.
    pub(crate) fn event_voice_live_guild(
        &self,
        guild: two_bot_core::Snowflake,
    ) -> Option<two_bot::voice_rooms::LiveGuild> {
        self.voice
            .as_ref()
            .and_then(|voice| voice.event_voice_live(guild))
    }

    /// Run the production event-voice continuation only after live category
    /// preflight; the same guard spans its database scan, claims and REST.
    async fn with_event_voice_tick<F, Fut>(
        &self,
        settings: event_voice_preflight::Settings,
        shutdown: &watch::Receiver<bool>,
        action: F,
    ) -> Result<(), ErrorClass>
    where
        F: FnOnce(u64) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), ErrorClass>> + Send,
    {
        let guild = self.guild.parse().map_err(|_| ErrorClass::Configuration)?;
        self.event_voice
            .run(
                &self.rest,
                guild,
                self.event_voice_live_guild(guild),
                settings,
                shutdown,
                action,
            )
            .await
    }

    /// One supervised website tick: Events ticks run through the shared
    /// [`JobGate`] single-flight entry point, so overlapping Events ticks
    /// cannot both run; EventVoice ticks run the guarded production
    /// composition below; every other kind runs direct via [`run_once`] and
    /// never touches a gate. The scheduler closure calls this instead of
    /// branching itself, so the routing stays unit-testable next to the tick
    /// it guards.
    async fn run_tick(
        &self,
        kind: Kind,
        shutdown: &watch::Receiver<bool>,
        rank_heal: bool,
    ) -> Result<(), ErrorClass> {
        if matches!(kind, Kind::Events) {
            return Kind::run_events_tick(
                self.pool().await?,
                &self.rest,
                &self.guild,
                &self.observation,
                shutdown,
                rank_heal,
                &self.events_gate,
            )
            .await;
        }
        if matches!(kind, Kind::EventVoice) {
            return self.run_event_voice_production_tick(shutdown).await;
        }
        run_once(
            kind,
            self.pool().await?,
            &self.rest,
            &self.guild,
            &self.observation,
            shutdown,
            rank_heal,
        )
        .await
    }

    /// Production EventVoice tick: live category preflight, fresh binding
    /// reconciliation, a fresh source read with live occupancy overlay, then
    /// the durable decision pass. Environment-sourced settings stay in this
    /// thin wrapper so the composition below is directly testable.
    async fn run_event_voice_production_tick(
        &self,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(), ErrorClass> {
        let settings = event_voice_preflight::Settings {
            category: two_bot::event_voice_category::configured_event_voice_category(|key| {
                std::env::var(key).ok()
            }),
            staff_channel: crate::raid_runtime::staff_alert_channel(|key| std::env::var(key).ok()),
        };
        let admission = two_bot::voice_rooms::configured_admission(|key| std::env::var(key).ok());
        self.run_event_voice_tick_guarded(settings, admission, shutdown)
            .await
    }

    /// The guarded production composition: the preflight gate spans the
    /// reconcile, scan, claims and REST below, so overlapping ticks cannot
    /// both run. The decision core takes no gate of its own here; passing the
    /// held preflight gate into the snapshot seam would self-skip every tick.
    async fn run_event_voice_tick_guarded(
        &self,
        settings: event_voice_preflight::Settings,
        admission: CreateAdmissionConfig,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(), ErrorClass> {
        self.with_event_voice_tick(settings, shutdown, |category| async move {
            let pool = self.pool().await?;
            let guild_id: u64 = self.guild.parse().map_err(|_| ErrorClass::Configuration)?;
            let report = self
                .event_voice_reconcile
                .run(pool, &self.rest, guild_id, shutdown)
                .await?;
            for missing in &report.missing {
                tracing::info!(
                    event_id = missing.event_id.as_str(),
                    instance_start = %missing.instance,
                    channel_id = missing.channel_id.as_str(),
                    "event voice binding 404s; row orphaned for one retry"
                );
            }
            tracing::info!(
                missing = report.missing.len(),
                perm_denied = report.perm_denied,
                rate_limited = report.rate_limited,
                unavailable = report.unavailable,
                "event voice reconcile pass finished"
            );
            let Some(mut snapshot) = self
                .event_voice_source
                .read(pool, &self.rest, &self.guild, shutdown)
                .await?
            else {
                return Ok(());
            };
            event_voice_source::overlay_live_state(pool, &mut snapshot.events).await?;
            let actor = self
                .rest
                .current_bot_user_id()
                .await
                .map_err(|_| ErrorClass::Rest)?;
            let now = i64::try_from(
                time::OffsetDateTime::now_utc()
                    .unix_timestamp_nanos()
                    .div_euclid(1_000_000),
            )
            .map_err(|_| ErrorClass::Rest)?;
            let io = event_voice_create_runtime::DurableTickIo {
                pool,
                rest: &self.rest,
                guild: &self.guild,
                snapshot: &snapshot,
                actor,
                admission,
            };
            let category_id = category.to_string();
            let outcome = event_voice_ticker::run_claimed(
                &io,
                &self.guild,
                &snapshot.events,
                Some(category_id.as_str()),
                now,
                shutdown,
            )
            .await?;
            let counter = |key: &str| outcome["counters"][key].as_u64().unwrap_or_default();
            let (created, rate_limited, perm_denied, no_category, deleted) = (
                counter("created"),
                counter("rate_limited"),
                counter("perm_denied"),
                counter("no_category"),
                counter("deleted"),
            );
            if perm_denied > 0 {
                tracing::warn!(
                    perm_denied,
                    "event voice tick saw permission denials; preflight pages staff once per gap"
                );
            }
            tracing::info!(
                created,
                rate_limited,
                perm_denied,
                no_category,
                deleted,
                "event voice tick finished"
            );
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
impl Context {
    /// Test-only constructor for sibling test modules, which cannot name the
    /// private `observation`/`events_gate` fields. Pre-seeds the pool cell with
    /// the caller's pool so offline tests never dial. Never compiled into the
    /// deployed binary.
    pub(crate) fn for_tests(
        url: String,
        pool: PgPool,
        rest: ActionExecutor,
        guild: String,
    ) -> Self {
        let cell = OnceCell::new();
        cell.set(pool).expect("seed the pool cell");
        Self {
            url,
            pool: cell,
            rest,
            guild,
            observation: Mutex::new(()),
            events_gate: JobGate::default(),
            event_voice: event_voice_preflight::Tick::default(),
            event_voice_source: event_voice_source::Reader::default(),
            event_voice_reconcile: event_voice_reconcile::Reconciler::default(),
            voice: None,
        }
    }
}

/// Fallback cancellation if the HTTP owner is dropped before graceful cleanup.
struct Shutdown(watch::Sender<bool>);
impl Drop for Shutdown {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// `self_roles` is the ONE boot-composed service Arc also injected into gateway
/// dispatch; recovery joins the existing supervisor, never another feature service.
fn governed_executor(
    token: &str,
    proxy: Option<String>,
    pool: PgPool,
) -> Result<ActionExecutor, String> {
    let admission = two_bot_core::send_admission::PgSendAdmission::new(pool, token)
        .map_err(|error| error.to_string())?;
    ActionExecutor::with_admission(token.to_owned(), proxy, Arc::new(admission))
}

/// Send-admission pool shape: max 2 + 15 s statement timeout (store parity)
/// with a 10 s acquire timeout. One shared helper for the website jobs, the
/// preflight `admission_transport` and the commands CLI `executor`
/// (threat-model F6).
pub(crate) const ADMISSION_POOL_MAX: u32 = 2;
pub(crate) const ADMISSION_STATEMENT_TIMEOUT_MS: u64 = 15_000;
pub(crate) const ADMISSION_ACQUIRE_TIMEOUT_SECS: u64 = 10;

/// Read the one TLS policy setting; an unset value is `Required`.
pub(crate) fn admission_tls_policy_from_env() -> Result<TlsPolicy, &'static str> {
    let value = std::env::var_os(database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    TlsPolicy::from_setting(value.as_ref().map(|v| v.to_str().unwrap_or("")))
}

/// Shared fenced options builder: validate before SQLx can WARN-log query
/// values, enforce the TLS policy, then apply the effective mode. The
/// statement timeout rides the connection options. URL/parse failures read as
/// the generic authority code; TLS refusals keep their fixed strings. Nothing
/// echoes the URL, host or password.
pub(crate) fn admission_connect_options(
    url: &str,
    tls: TlsPolicy,
) -> Result<PgConnectOptions, &'static str> {
    two_bot_core::database_url::validate(url).map_err(|_| "invalid admission authority")?;
    // Threat-model F6: refuse plaintext/unverified modes and the wrong host
    // class before SQLx parses the URL (see `docs/database-tls.md`).
    database_tls::enforce(url, tls)?;
    let options = two_bot_core::database_url::connect_options(url)
        .map_err(|_| "invalid admission authority")?;
    let options = database_tls::apply(options, tls);
    Ok(options.options([(
        "statement_timeout",
        format!("{ADMISSION_STATEMENT_TIMEOUT_MS}ms"),
    )]))
}

/// [`admission_pool`] with an explicit TLS policy (tests pass `LocalOnly`).
/// Lazy, so refusal cases are hermetic (no socket) and the happy path asserts
/// `pool.size() == 0`. The acquire timeout bounds the later wires.
pub(crate) fn admission_pool_with_tls(url: &str, tls: TlsPolicy) -> Result<PgPool, &'static str> {
    let options = admission_connect_options(url, tls)?;
    Ok(PgPoolOptions::new()
        .max_connections(ADMISSION_POOL_MAX)
        .acquire_timeout(Duration::from_secs(ADMISSION_ACQUIRE_TIMEOUT_SECS))
        .connect_lazy_with(options))
}

fn admission_pool(url: &str) -> Result<PgPool, String> {
    let tls =
        admission_tls_policy_from_env().map_err(|_| "invalid admission authority".to_owned())?;
    admission_pool_with_tls(url, tls).map_err(|_| "invalid admission authority".to_owned())
}

/// Boot-composed single call: the nine parameters are the full supervised
/// surface (config, listener, gateway, shutdown plus one slot per consumer).
/// `voice` is the gateway sink cloned before the gateway task owns it; the
/// T-15 ticker reads live gateway evidence through it.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    config: &Config,
    listener: tokio::net::TcpListener,
    gateway: server::SharedState,
    shutdown: watch::Sender<bool>,
    self_roles: Option<Arc<SelfRoleService>>,
    automod: crate::automod_gateway::Slot,
    receiver: Option<crate::internal_action_http::BoundReceiver>,
    eventsub_receiver: Option<crate::eventsub_http::BoundEventsubReceiver>,
    member: Option<Arc<MemberRuntime>>,
    voice: Option<Arc<dyn two_bot::voice_rooms::VoiceEventSink>>,
) -> std::io::Result<()> {
    let mut registered = Vec::new();
    let mut parked = Vec::new();
    // The same token-derived identity the router and gateway fence use, so a
    // job that posts under the bot identity cannot outrun a refused capability.
    let activation = BootActivation::from_config(config);
    if let Ok((token, url, guild)) = crate::gateway_prerequisites(config) {
        // The settings poll is DB-only: register it before REST construction
        // so a bad DISCORD_API_BASE cannot park hot reload.
        registered.push(crate::settings_jobs::job(url));
        // Lazy connection preserves parked/startup behavior; every wire attempt
        // still fails closed on this same runtime database authority.
        let rest = admission_pool(url).and_then(|pool| {
            governed_executor(token, std::env::var("DISCORD_API_BASE").ok(), pool)
        });
        match rest {
            Ok(rest) => {
                let context = Arc::new(Context {
                    url: url.to_owned(),
                    pool: OnceCell::new(),
                    rest,
                    guild: guild.to_string(),
                    observation: Mutex::new(()),
                    events_gate: JobGate::default(),
                    event_voice: event_voice_preflight::Tick::default(),
                    event_voice_source: event_voice_source::Reader::default(),
                    event_voice_reconcile: event_voice_reconcile::Reconciler::default(),
                    voice: voice.clone(),
                });
                // The rank tick is the only website job with a Discord write
                // path: the ladder self-heal grants roles under the bot
                // identity. It obeys the same live-identity fence as the other
                // posting jobs, resolved once at boot (the token never changes
                // at runtime). A refused identity keeps the old non-nested
                // refusal and never grants.
                let rank_heal = activation.rank_heal_permitted();
                for (name, kind) in NAMES.into_iter().zip([
                    Kind::Counter,
                    Kind::Rank,
                    Kind::Events,
                    Kind::EventVoice,
                ]) {
                    let context = context.clone();
                    let shutdown = shutdown.subscribe();
                    let cadence = cadence(kind);
                    let rank_heal = rank_heal && matches!(kind, Kind::Rank);
                    registered.push(Job {
                        name,
                        cadence,
                        cadence_source: None,
                        startup_jitter: jobs::startup_jitter(cadence, rand::random()),
                        timeout: Duration::from_secs(if matches!(kind, Kind::Counter) {
                            45
                        } else {
                            120
                        }),
                        action: Arc::new(move || {
                            let context = context.clone();
                            let shutdown = shutdown.clone();
                            Box::pin(async move {
                                tokio::select! {
                                    biased;
                                    _ = server::shutdown_requested(shutdown.clone()) => Ok(()),
                                    result = async {
                                        context.run_tick(kind, &shutdown, rank_heal).await
                                    } => result,
                                }
                            })
                        }),
                    });
                }
                let scheduled = scheduled_jobs::register(context.clone(), &activation);
                let scheduled_parked = scheduled.is_none();
                registered.extend(scheduled);
                if let Some(job) = crate::giveaway_runtime::register(context.clone(), &activation) {
                    registered.push(job);
                } else {
                    parked.push(crate::giveaway_runtime::REFRESH_JOB);
                }
                if let Some(job) =
                    crate::giveaway_runtime::register_draw(context.clone(), &activation)
                {
                    registered.push(job);
                } else {
                    parked.push(crate::giveaway_runtime::DRAW_JOB);
                }
                // The unban sweep shares the boot-composed member consumer:
                // one guild store across commands and sweep, never a second
                // same-guild consumer with its own local queues.
                if let Some(member) = member {
                    registered.push(member_runtime::sweep_job(member, context.rest.clone()));
                }
                let registration = community_jobs::register(context.clone());
                registered.extend(registration.jobs);
                parked = registration.parked;
                if scheduled_parked {
                    parked.push(scheduled_jobs::NAMES[0]);
                }
                if let Some(job) = feed_jobs::register(context.clone(), &activation) {
                    registered.push(job);
                } else {
                    parked.push(feed_jobs::NAME);
                }
                if let Some(job) = crate::lfg_jobs::register(context.clone(), &activation) {
                    registered.push(job);
                } else {
                    parked.push(crate::lfg_jobs::NAME);
                }
                // Repeat-history expiry rides the shared supervisor.
                if crate::automod_gateway::enabled() {
                    registered.push(crate::automod_gateway::expiry_job(automod));
                }
                match audit_runtime::register(context, shutdown.subscribe()) {
                    Some(job) => registered.push(job),
                    None => parked.extend(audit_runtime::NAMES),
                }
            }
            Err(_) => tracing::warn!("website jobs parked: invalid REST configuration"),
        }
    } else {
        tracing::info!("website jobs parked: gateway prerequisites missing");
    }

    // Retention scheduler: DB-only supervised job, registered even when the
    // Discord gateway is unconfigured. Missing/invalid/`off` mode or a
    // missing/unusable database parks the job without attempting destructive
    // work; the `/readyz` status entry exists either way.
    match retention_registration(config) {
        RetentionRegistration::Active(job) => registered.push(job),
        RetentionRegistration::Parked => parked.push(crate::retention_jobs::NAME),
    }

    if let Some(service) = self_roles {
        registered.push(service.recovery_job());
    }
    let status = prepare_job_statuses(&registered, &parked, &shutdown).await?;
    let public = server::serve(listener, gateway, status.clone(), shutdown.clone());
    let http = async {
        match (receiver, eventsub_receiver) {
            (Some(receiver), Some(eventsub)) => {
                // Three listeners: combine the two private receivers first,
                // then supervise the pair against the public listener. First
                // termination still stops admission everywhere.
                let private = serve_listeners(
                    receiver.serve(shutdown.subscribe()),
                    eventsub.serve(shutdown.subscribe()),
                    shutdown.clone(),
                );
                serve_listeners(public, private, shutdown.clone()).await
            }
            (Some(receiver), None) => {
                serve_listeners(
                    public,
                    receiver.serve(shutdown.subscribe()),
                    shutdown.clone(),
                )
                .await
            }
            (None, Some(eventsub)) => {
                serve_listeners(
                    public,
                    eventsub.serve(shutdown.subscribe()),
                    shutdown.clone(),
                )
                .await
            }
            (None, None) => public.await,
        }
    };
    serve_jobs(registered, status, shutdown.clone(), http).await
}

/// Retention registration outcome: an active supervised job, or a parked
/// status entry with no job (safe mode, disabled, or no database).
enum RetentionRegistration {
    Active(Job),
    Parked,
}

/// Resolve the retention scheduler registration from the process environment
/// and the database binding alone: the job never needs a Discord token or
/// guild. Anything but the exact `execute` opt-in is dry run or parked, and
/// a missing/unusable database parks without attempting destructive work.
fn retention_registration(config: &Config) -> RetentionRegistration {
    let setting = std::env::var(crate::retention_jobs::MODE_SETTING).ok();
    let mode = match crate::retention_jobs::mode_from_setting(setting.as_deref()) {
        Ok(mode) => mode,
        Err(_) => {
            tracing::warn!(
                "retention parked: invalid TWO_RETENTION_MODE; expected dry-run, execute or off"
            );
            return RetentionRegistration::Parked;
        }
    };
    let Some(mode) = mode else {
        return RetentionRegistration::Parked;
    };
    let url = config
        .database_url
        .as_ref()
        .map(|secret| secret.expose().as_str().to_owned())
        .filter(|url| !url.is_empty());
    let Some(url) = url else {
        return RetentionRegistration::Parked;
    };
    match crate::retention_jobs::pool_from_url(&url) {
        Ok(pool) => RetentionRegistration::Active(crate::retention_jobs::job(pool, mode)),
        Err(_) => {
            tracing::warn!("retention parked: invalid database configuration");
            RetentionRegistration::Parked
        }
    }
}

/// Prepare boot status, cancelling boot-owned siblings on registration failure.
async fn prepare_job_statuses(
    registered: &[Job],
    parked: &[&str],
    shutdown: &watch::Sender<bool>,
) -> std::io::Result<jobs::SharedStatus> {
    registered_statuses(registered, parked)
        .await
        .map_err(|error| {
            // The boot owner joins reconcile after serve returns, even when
            // registration fails before serve_jobs installs its shutdown guard.
            shutdown.send_replace(true);
            std::io::Error::other(error)
        })
}

async fn registered_statuses(
    registered: &[Job],
    parked: &[&str],
) -> Result<jobs::SharedStatus, jobs::UnregisteredJob> {
    // NAMES already carries the DB-only settings poll alongside the
    // REST-backed domains, so the status map lists all sixteen jobs.
    let mut names: Vec<&'static str> = NAMES
        .into_iter()
        .chain(community_jobs::NAMES)
        .chain(audit_runtime::NAMES)
        .chain(scheduled_jobs::NAMES)
        .chain(member_runtime::NAMES)
        .chain([RECOVERY_JOB_NAME])
        .chain([feed_jobs::NAME])
        .chain([crate::giveaway_runtime::REFRESH_JOB])
        .chain([crate::giveaway_runtime::DRAW_JOB])
        .chain([crate::lfg_jobs::NAME])
        .chain([crate::retention_jobs::NAME])
        .collect();
    if crate::automod_gateway::enabled() {
        names.push(crate::automod_gateway::JOB_NAME);
    }
    // Start everything parked; the loop below unparks exactly the registered
    // jobs. Recovery alone must not make unavailable website/community jobs
    // look active, and an absent recovery job must not report live.
    let status = jobs::statuses(&names, true);
    {
        let mut entries = status.write().await;
        // Fail fast before unparking anything: an unknown name is a
        // registration bug, not a status to invent.
        jobs::check_registered(registered, &entries)?;
        for job in registered {
            // Proven by the check above: every registered name resolves.
            let Some(entry) = entries.get_mut(job.name) else {
                unreachable!("registered_statuses validated every job name: {}", job.name);
            };
            entry.parked = false;
        }
        for name in parked {
            if let Some(entry) = entries.get_mut(*name) {
                entry.parked = true;
            }
        }
    }
    Ok(status)
}

/// First listener termination stops admission everywhere and drains its sibling.
/// Both futures stay owned by serve_jobs; its existing HTTP drain bound applies.
pub(crate) async fn serve_listeners(
    public: impl std::future::Future<Output = std::io::Result<()>>,
    private: impl std::future::Future<Output = std::io::Result<()>>,
    shutdown: watch::Sender<bool>,
) -> std::io::Result<()> {
    tokio::pin!(public, private);
    let (public_finished, first) = tokio::select! {
        result = &mut public => (true, result),
        result = &mut private => (false, result),
    };
    let unexpected = !*shutdown.borrow();
    shutdown.send_replace(true);
    let sibling = if public_finished {
        private.await
    } else {
        public.await
    };
    first?;
    sibling?;
    if unexpected {
        return Err(std::io::Error::other("HTTP listener stopped unexpectedly"));
    }
    Ok(())
}

pub(crate) const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Own the production spawn/cancel/join path independently of job registration.
pub(crate) async fn serve_jobs(
    registered: Vec<Job>,
    status: jobs::SharedStatus,
    shutdown: watch::Sender<bool>,
    http: impl std::future::Future<Output = std::io::Result<()>>,
) -> std::io::Result<()> {
    // Registration failures must also cancel sibling tasks owned by boot.
    let shutdown = Shutdown(shutdown);
    // Fail fast before spawning the supervisor task or draining HTTP: its
    // completion is joined with `let _`, so a registration error returned from
    // inside the task would read as a silent idle instead of a boot failure.
    // The guard is scoped: the read lock must be released before the
    // supervisor spawns, or the jobs' first status write would block on it.
    {
        let statuses = status.read().await;
        jobs::check_registered(&registered, &statuses).map_err(std::io::Error::other)?;
    }
    let supervisor = tokio::spawn(jobs::supervise(registered, status, shutdown.0.subscribe()));
    let result = drain_http(http, shutdown.0.subscribe()).await;
    shutdown.0.send_replace(true);
    let _ = supervisor.await;
    result
}

/// Bound only HTTP draining; the owner must still join job cleanup afterward.
async fn drain_http(
    http: impl std::future::Future<Output = std::io::Result<()>>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    tokio::pin!(http);
    tokio::select! {
        biased;
        result = &mut http => result,
        _ = server::shutdown_requested(shutdown) => {
            tokio::time::timeout(HTTP_DRAIN_TIMEOUT, &mut http)
                .await
                .unwrap_or_else(|_| Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "HTTP shutdown drain timed out",
                )))
        }
    }
}

pub(crate) async fn get(rest: &ActionExecutor, path: &str) -> Result<Value, ErrorClass> {
    rest.get_json(path)
        .await
        .map_err(|_| ErrorClass::Rest)?
        .ok_or(ErrorClass::Rest)
}

/// [`get`] plus the raw wire-body length, for callers enforcing a wire-body
/// cap. The length is measured over the collected response bytes, never over
/// a re-serialization of the parsed value.
async fn get_with_body_len(
    rest: &ActionExecutor,
    path: &str,
) -> Result<(Value, usize), ErrorClass> {
    rest.get_json_with_body_len(path)
        .await
        .map_err(|_| ErrorClass::Rest)?
        .ok_or(ErrorClass::Rest)
}

fn snowflake(value: &Value) -> Result<u64, ErrorClass> {
    value
        .as_str()
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .ok_or(ErrorClass::Rest)
}

fn roster_member(member: &Value) -> Result<RosterMember, ErrorClass> {
    let user = &member["user"];
    let id = snowflake(&user["id"])?;
    let is_bot = match user.get("bot") {
        None => false,
        Some(v) => v.as_bool().ok_or(ErrorClass::Rest)?,
    };
    let roles = member["roles"]
        .as_array()
        .ok_or(ErrorClass::Rest)?
        .iter()
        .map(|role| snowflake(role).map(|id| id.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RosterMember {
        user_id: id.to_string(),
        is_bot,
        roles,
    })
}

fn roster_page(page: &Value, after: &mut u64) -> Result<Vec<RosterMember>, ErrorClass> {
    let page = page.as_array().ok_or(ErrorClass::Rest)?;
    if page.len() > 1000 {
        return Err(ErrorClass::Rest);
    }
    let mut members = Vec::with_capacity(page.len());
    for member in page {
        let parsed = roster_member(member)?;
        let id = snowflake(&member["user"]["id"])?;
        if id <= *after {
            return Err(ErrorClass::Rest);
        }
        *after = id;
        members.push(parsed);
    }
    Ok(members)
}

pub(crate) enum RosterScan {
    Complete(Vec<RosterMember>),
    Truncated,
}

/// Only a confirmed 404 proves departure. Other failed reads, misbound user
/// identities and incomplete role facts are never an empty membership.
async fn rank_repair_member(
    rest: &ActionExecutor,
    guild: &str,
    member_id: &str,
) -> Result<Option<RosterMember>, ErrorClass> {
    let Some(value) = rest
        .get_json_strict(&format!("/guilds/{guild}/members/{member_id}"))
        .await
        .map_err(|_| ErrorClass::Rest)?
    else {
        return Ok(None);
    };
    let member = roster_member(&value)?;
    if member.user_id != member_id {
        return Err(ErrorClass::Rest);
    }
    Ok(Some(member))
}

/// Discord's paginated roster is the denominator, never approximate counts.
/// A full final page cannot prove completion; never expose a partial roster.
pub(crate) async fn roster(
    rest: &ActionExecutor,
    guild: &str,
    max_pages: usize,
) -> Result<RosterScan, ErrorClass> {
    let mut after = 0;
    let mut members = Vec::new();
    for _ in 0..max_pages {
        // No hidden retries: the page ceiling also bounds wire requests.
        let page = rest
            .get_json_once(&format!("/guilds/{guild}/members?limit=1000&after={after}"))
            .await
            .map_err(|_| ErrorClass::Rest)?
            .ok_or(ErrorClass::Rest)?;
        let page = roster_page(&page, &mut after)?;
        let complete = page.len() < 1000;
        members.extend(page);
        if complete {
            return Ok(RosterScan::Complete(members));
        }
    }
    Ok(RosterScan::Truncated)
}

/// Legacy BOT_FLOOR_MAX_PAGES: ten full pages plus one termination probe.
pub(crate) const BOT_FLOOR_MAX_PAGES: usize = 11;

/// Upper bound for the per-rung match count in the rank-ladder refusal log.
/// The diagnostic reports the raw count; only the log line is capped so one
/// misconfigured guild cannot bloat a warn line.
pub(crate) const RANK_LADDER_MATCH_LOG_CAP: usize = 99;

/// A daily floor scan is bounded independently of the website roster. A full
/// final page cannot prove completion, so discard the partial count. Exactly
/// 10,000 members completes via an empty eleventh page.
pub(crate) async fn bot_floor_scan(
    rest: &ActionExecutor,
    guild: &str,
) -> Result<two_bot_core::BotFloorScan, ErrorClass> {
    let mut after = 0;
    let mut bots = 0;
    for _ in 0..BOT_FLOOR_MAX_PAGES {
        // No hidden retries: the page ceiling also bounds wire requests.
        let page = rest
            .get_json_once(&format!("/guilds/{guild}/members?limit=1000&after={after}"))
            .await
            .map_err(|_| ErrorClass::Rest)?
            .ok_or(ErrorClass::Rest)?;
        let page = roster_page(&page, &mut after)?;
        bots += page.iter().filter(|member| member.is_bot).count() as i64;
        if page.len() < 1000 {
            return Ok(two_bot_core::BotFloorScan::Complete(bots));
        }
    }
    Ok(two_bot_core::BotFloorScan::Truncated)
}

fn raw_event(value: &Value, guild: &str) -> Result<RawScheduledEvent, ErrorClass> {
    if !value.is_object() || canonical_snowflake(&value["guild_id"]) != Some(guild) {
        return Err(ErrorClass::Rest);
    }
    let id = canonical_snowflake(&value["id"]).ok_or(ErrorClass::Rest)?;
    let channel_id = match value.get("channel_id") {
        None | Some(Value::Null) => None,
        Some(channel) => Some(
            canonical_snowflake(channel)
                .ok_or(ErrorClass::Rest)?
                .to_owned(),
        ),
    };
    let optional = |name: &str| match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(ErrorClass::Rest),
    };
    Ok(RawScheduledEvent {
        id: Some(id.to_owned()),
        name: optional("name")?,
        scheduled_start_time: optional("scheduled_start_time")?,
        channel_id,
        description: optional("description")?,
        status: value["status"].as_i64(),
    })
}

/// Cap on Discord role grants per rank tick. The cutover incident repaired 5
/// members; above this the tick heals nothing and fails closed as
/// [`ErrorClass::Configuration`], so a mass misconfiguration can never become
/// a mass grant. Each member needs at most four grants (five rungs).
pub(crate) const MAX_RANK_SELF_HEAL_GRANTS_PER_TICK: usize = 25;

/// Audit-log reason on every rank self-heal grant (ASCII, far under the
/// 512-char `X-Audit-Log-Reason` bound the executor enforces).
pub(crate) const RANK_SELF_HEAL_AUDIT_REASON: &str =
    "rank ladder self-heal: grant missing lower rank roles";

/// Heal-phase Discord failures keep precise classes: a confirmed refusal
/// (hierarchy, permissions, unknown role) proves the ladder cannot be healed
/// and reads as [`ErrorClass::Configuration`]; wire uncertainty stays
/// [`ErrorClass::Rest`] for the next tick. Nothing publishes either way.
fn heal_error(error: DiscordError) -> ErrorClass {
    match error {
        DiscordError::Rejected(_) => ErrorClass::Configuration,
        _ => ErrorClass::Rest,
    }
}

/// Revalidate each candidate in `plan` immediately before its grant, then
/// report whether repair completed (`true`) or stopped mid-heal (`false`).
/// Completion still requires authoritative post-repair verification before
/// publication; Discord grants already applied cannot be rolled back.
/// Fails closed before any mutation when the plan is empty or over bound,
/// when the guild roles carry no provable hierarchy, or when any target sits
/// at or above the bot's highest role. The original candidate queue never
/// grows: at most 25 eligibility reads and 25 grants, even under role churn.
async fn heal_rank_ladder(
    rest: &ActionExecutor,
    guild: &str,
    guild_roles: &Value,
    plan: &[RankHeal],
    shutdown: &watch::Receiver<bool>,
) -> Result<bool, ErrorClass> {
    if plan.is_empty() {
        tracing::warn!(
            job = "rank",
            "rank ladder not nested with no healable rungs; publication skipped"
        );
        return Err(ErrorClass::Configuration);
    }
    if plan.len() > MAX_RANK_SELF_HEAL_GRANTS_PER_TICK {
        tracing::warn!(
            job = "rank",
            grants = plan.len(),
            max = MAX_RANK_SELF_HEAL_GRANTS_PER_TICK,
            "rank self-heal over bound; publication skipped"
        );
        return Err(ErrorClass::Configuration);
    }
    if publication_stopped(shutdown) {
        return Ok(false);
    }
    // Hierarchy inputs come from the already-fetched guild object, so a
    // nested ladder never pays for them. A role without a numeric id or
    // position cannot be proven below the bot: fail closed.
    let Some(roles_array) = guild_roles.as_array() else {
        tracing::warn!(
            job = "rank",
            "rank self-heal cannot read guild roles; publication skipped"
        );
        return Err(ErrorClass::Configuration);
    };
    let ladder_roles = roles_array
        .iter()
        .map(|role| {
            let id = snowflake(&role["id"])?;
            let name = role["name"].as_str().ok_or(ErrorClass::Rest)?;
            Ok((id.to_string(), name.to_owned()))
        })
        .collect::<Result<Vec<_>, ErrorClass>>()?;
    let ladder = match_rank_roles(&ladder_roles).ok_or(ErrorClass::Configuration)?;
    let mut positions = HashMap::new();
    for role in roles_array {
        let (Some(id), Some(position)) = (role["id"].as_str(), role["position"].as_i64()) else {
            tracing::warn!(
                job = "rank",
                "rank self-heal cannot prove role hierarchy; publication skipped"
            );
            return Err(ErrorClass::Configuration);
        };
        positions.insert(id.to_owned(), position);
    }
    let bot_id = rest.current_bot_user_id().await.map_err(heal_error)?;
    let guild_id: u64 = guild.parse().map_err(|_| ErrorClass::Configuration)?;
    let bot_roles = rest
        .member_role_ids(guild_id, bot_id)
        .await
        .map_err(heal_error)?;
    let bot_highest = bot_roles
        .iter()
        .filter_map(|id| positions.get(id).copied())
        .max();
    let Some(bot_highest) = bot_highest else {
        tracing::warn!(
            job = "rank",
            "rank self-heal cannot read the bot hierarchy; publication skipped"
        );
        return Err(ErrorClass::Configuration);
    };
    for grant in plan {
        let position = positions.get(&grant.role_id).copied();
        if position.is_none_or(|position| position >= bot_highest) {
            tracing::warn!(
                job = "rank",
                member = grant.member_id.as_str(),
                role = grant.key.label(),
                "rank self-heal target at or above the bot; publication skipped"
            );
            return Err(ErrorClass::Configuration);
        }
    }
    for grant in plan {
        if publication_stopped(shutdown) {
            return Ok(false);
        }
        let observed = rank_repair_member(rest, guild, &grant.member_id)
            .await
            .inspect_err(|_| {
                tracing::warn!(
                    job = "rank",
                    "rank self-heal eligibility unverified; publication skipped"
                );
            })?;
        let Some(member) = observed else {
            continue;
        };
        // Targets were already filtered against the tick's fixed raid windows.
        // Recompute this member's current missing rungs, never trusting the old
        // higher rank or a previously successful PUT as eligibility evidence.
        if !plan_rank_heal(std::slice::from_ref(&member), &ladder, &[]).contains(grant) {
            continue;
        }
        if publication_stopped(shutdown) {
            return Ok(false);
        }
        rest.set_member_role(
            guild,
            &grant.member_id,
            &grant.role_id,
            true,
            RANK_SELF_HEAL_AUDIT_REASON,
        )
        .await
        .map_err(heal_error)?;
        tracing::info!(
            job = "rank",
            member = grant.member_id.as_str(),
            role = grant.key.label(),
            "rank ladder self-heal grant applied; observation pending"
        );
    }
    Ok(true)
}

/// Cancel queued/in-flight observations directly, without waiting for the job
/// supervisor to abort us. Biased selection discards a simultaneously-ready REST
/// result; publication fences also cover shutdown arriving during that poll.
/// This cannot undo a database commit already submitted before shutdown.
///
/// `rank_heal` is the boot-resolved live-identity fence for the rank ladder
/// self-heal: only the rank tick consults it, and only a permitted identity
/// may grant roles. Every other kind ignores it.
pub async fn run_once(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
    shutdown: &watch::Receiver<bool>,
    rank_heal: bool,
) -> Result<(), ErrorClass> {
    tokio::select! {
        biased;
        _ = server::shutdown_requested(shutdown.clone()) => Ok(()),
        result = snapshot_once(kind, pool, rest, guild, observation, shutdown, rank_heal) => result,
    }
}

fn publication_stopped(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}

/// Raw scheduled-events response-body cap, 1 MiB (bounded-claim decision §4).
/// Bodies over this refuse as [`ErrorClass::Rest`] before any row reaches
/// storage; the last good snapshot is kept by construction.
pub(crate) const MAX_SCHEDULED_EVENTS_BODY_BYTES: usize = 1_048_576;

/// Maximum age at the events publication boundary. This is independent of
/// the ten-minute poll cadence: it bounds the pending observation, not the
/// lifetime of the last good mirror between ticks.
pub(crate) const MAX_SCHEDULED_EVENTS_SNAPSHOT_AGE: time::Duration = time::Duration::minutes(5);

/// Strict five-minute age predicate: only observations strictly older than
/// [`MAX_SCHEDULED_EVENTS_SNAPSHOT_AGE`] are stale. Exactly five minutes old
/// is still fresh. Pure so the boundary pins deterministically; the seam
/// samples `now` at the gate and passes it in.
fn snapshot_age_exceeded(observed: time::OffsetDateTime, now: time::OffsetDateTime) -> bool {
    now - observed > MAX_SCHEDULED_EVENTS_SNAPSHOT_AGE
}

/// One Events snapshot attempt with the observation stamp injected.
/// Production stamps `observed_at` from the real clock in [`snapshot_once`]
/// immediately before the GET; age-bound tests inject a fixed stamp here
/// instead of waiting for a slow response.
///
/// Whole-response gates, in order: the raw wire-body cap (including unknown
/// fields and JSON-insignificant bytes), shape/normalization, then snapshot
/// age before storage. Older-than-five-minute observations skip the whole
/// replacement, including empty snapshots, with `StaleSnapshot`. The store's
/// independent epoch claim still rejects older-than-committed observations.
pub(crate) async fn snapshot_events_once(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    shutdown: &watch::Receiver<bool>,
    observed_at: &str,
) -> Result<(), ErrorClass> {
    let (response, body_len) = get_with_body_len(
        rest,
        &format!("/guilds/{guild}/scheduled-events?with_user_count=true"),
    )
    .await?;
    // Measure the wire bytes, not the parsed value: an unknown-field pad must
    // count toward the cap exactly like a long description does, and
    // JSON-insignificant bytes (whitespace, duplicate keys, \uXXXX escapes)
    // count as Discord sent them. A re-serialized measure would compact those
    // away and admit an over-cap body.
    if body_len > MAX_SCHEDULED_EVENTS_BODY_BYTES {
        return Err(ErrorClass::Rest);
    }
    let raw = response
        .as_array()
        .ok_or(ErrorClass::Rest)?
        .iter()
        .map(|value| raw_event(value, guild))
        .collect::<Result<Vec<_>, _>>()?;
    let events = normalize_events(&raw).ok_or(ErrorClass::Rest)?;
    if publication_stopped(shutdown) {
        return Ok(());
    }
    let observed =
        time::OffsetDateTime::parse(observed_at, &time::format_description::well_known::Rfc3339)
            .map_err(|_| ErrorClass::Configuration)?;
    if snapshot_age_exceeded(observed, time::OffsetDateTime::now_utc()) {
        tracing::warn!(
            job = "events",
            skip = ?ScheduledEventsSkip::StaleSnapshot,
            "snapshot exceeded the age bound; publication skipped"
        );
        return Ok(());
    }
    // Epoch-claim gate (main #1472): a strictly older fetch rolls the whole
    // swap back and reports StaleSnapshot; the tick logs the skip and keeps
    // the newer snapshot.
    match replace_events(pool, guild, observed_at, &events).await {
        Ok(None) => Ok(()),
        Ok(Some(skip)) => {
            // Warn, not info: the runner never overlaps ticks
            // (`jobs.rs` skips while one is active), so a lost claim
            // means wall-clock skew froze the whole events mirror until
            // real time passes the stored epoch.
            tracing::warn!(
                job = "events",
                skip = ?skip,
                "stale snapshot lost the epoch claim; publication skipped"
            );
            Ok(())
        }
        Err(_) => Err(ErrorClass::Database),
    }
}

async fn snapshot_once(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
    shutdown: &watch::Receiver<bool>,
    rank_heal: bool,
) -> Result<(), ErrorClass> {
    if matches!(kind, Kind::Events) {
        // Stamp the snapshot before the GET, not after: the mirror's
        // last-observed-wins `updated_at` must describe when the observation
        // began, so a mutation that lands while the GET is in flight keeps
        // its newer row instead of being overwritten by this older fetch.
        let fetched_at = now_iso();
        return snapshot_events_once(pool, rest, guild, shutdown, &fetched_at).await;
    }
    // Both kinds publish the denominator. Hold one shared lane from the first
    // observation through commit so a slow rank tick cannot overwrite a newer
    // counter roster. Events use independent tables and do not take this lock.
    let _observation = observation.lock().await;
    if publication_stopped(shutdown) {
        return Ok(());
    }
    let Some(windows) = read_raid_windows(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?
    else {
        tracing::info!(
            job = if matches!(kind, Kind::Counter) {
                "counter"
            } else {
                "rank"
            },
            "raid history ungrounded; publication skipped"
        );
        return Ok(());
    };
    if publication_stopped(shutdown) {
        return Ok(());
    }
    let members = match roster(rest, guild, BOT_FLOOR_MAX_PAGES).await? {
        RosterScan::Complete(members) => members,
        RosterScan::Truncated => {
            tracing::warn!(
                job = if matches!(kind, Kind::Counter) {
                    "counter"
                } else {
                    "rank"
                },
                "roster truncated at the page budget; publication skipped"
            );
            return Ok(());
        }
    };
    if publication_stopped(shutdown) {
        return Ok(());
    }
    if matches!(kind, Kind::Counter) {
        let reading = build_counter_reading(&members, &windows).ok_or(ErrorClass::Rest)?;
        let count =
            i32::try_from(reading.human_member_count).map_err(|_| ErrorClass::Configuration)?;
        if publication_stopped(shutdown) {
            return Ok(());
        }
        return write_counter(pool, guild, &now_iso(), count)
            .await
            .map_err(|_| ErrorClass::Database);
    }
    let guild_object = get(rest, &format!("/guilds/{guild}")).await?;
    let roles = guild_object["roles"]
        .as_array()
        .ok_or(ErrorClass::Rest)?
        .iter()
        .map(|role| {
            let id = snowflake(&role["id"])?;
            let name = role["name"].as_str().ok_or(ErrorClass::Rest)?;
            Ok((id.to_string(), name.to_owned()))
        })
        .collect::<Result<Vec<_>, ErrorClass>>()?;
    // A missing or ambiguous ladder is genuinely invalid: fail closed, but
    // name each failing rung by its fixed label plus the outcome and the
    // capped match count. Never logs a Discord role name or id: the
    // diagnostic carries only rung labels.
    let ladder = match match_rank_roles(&roles) {
        Some(ladder) => ladder,
        None => {
            let failures = diagnose_rank_roles(&roles);
            let detail = failures
                .iter()
                .map(|failure| {
                    let (outcome, count) = match failure.problem {
                        RankRungProblem::Missing => ("missing", 0),
                        RankRungProblem::Ambiguous { matches } => {
                            ("ambiguous", matches.min(RANK_LADDER_MATCH_LOG_CAP))
                        }
                    };
                    format!("{} {outcome} matches={count}", failure.key.label())
                })
                .collect::<Vec<_>>()
                .join("; ");
            tracing::warn!(
                job = "rank",
                failures = %if detail.is_empty() { "unknown".to_owned() } else { detail },
                "rank ladder unusable; publication skipped"
            );
            return Err(ErrorClass::Configuration);
        }
    };
    let mut members = members;
    let mut snapshot =
        build_community_snapshot(&members, &ladder, &windows).ok_or(ErrorClass::Rest)?;
    if !snapshot.nested {
        // Non-cumulative members (hand-granted higher rank without the lower
        // rungs) self-heal: grant the missing rungs with an audit reason,
        // bounded and hierarchy-fenced, then rebuild and re-verify. The heal
        // grants roles under the bot identity, so a refused identity keeps
        // the old refusal and never touches the wire.
        if !rank_heal {
            tracing::warn!(
                job = "rank",
                "rank ladder not nested; self-heal refused for this identity; publication skipped"
            );
            return Err(ErrorClass::Configuration);
        }
        let plan = plan_rank_heal(&members, &ladder, &windows);
        if !heal_rank_ladder(rest, guild, &guild_object["roles"], &plan, shutdown).await? {
            return Ok(());
        }
        // Refresh every affected member, including skipped candidates. The
        // initial plan bounds this set at 25. Only observed roles may enter the
        // snapshot; a successful PUT is not proof the role stuck or stayed.
        let mut affected: Vec<&str> = plan.iter().map(|grant| grant.member_id.as_str()).collect();
        affected.sort_unstable();
        affected.dedup();
        for member_id in affected {
            if publication_stopped(shutdown) {
                return Ok(());
            }
            let observed = rank_repair_member(rest, guild, member_id)
                .await
                .inspect_err(|_| {
                    tracing::warn!(
                        job = "rank",
                        "rank self-heal post-repair state unverified; publication skipped"
                    );
                })?;
            if let Some(observed) = observed {
                let member = members
                    .iter_mut()
                    .find(|member| member.user_id == member_id)
                    .ok_or(ErrorClass::Rest)?;
                *member = observed;
            } else {
                members.retain(|member| member.user_id != member_id);
            }
        }
        snapshot = build_community_snapshot(&members, &ladder, &windows).ok_or(ErrorClass::Rest)?;
        if !snapshot.nested {
            tracing::warn!(
                job = "rank",
                "rank ladder still not nested after self-heal; publication skipped"
            );
            return Err(ErrorClass::Configuration);
        }
        tracing::info!(
            job = "rank",
            "rank ladder self-healed: verified observed nested roles"
        );
    }
    if publication_stopped(shutdown) {
        return Ok(());
    }
    write_rank_snapshot(pool, guild, &now_iso(), &snapshot)
        .await
        .map_err(|error| match error {
            WebsiteStoreError::Invariant { .. } => ErrorClass::Configuration,
            _ => ErrorClass::Database,
        })
}
