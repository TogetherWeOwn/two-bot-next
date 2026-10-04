//! Runtime adapters for the three existing website-contract domains.

use std::{sync::Arc, time::Duration};

use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::{watch, Mutex, OnceCell};
use two_bot_core::{
    build_community_snapshot, build_counter_reading, match_rank_roles, normalize_events, now_iso,
    read_raid_windows, replace_events, write_counter, write_rank_snapshot, Config,
    RawScheduledEvent, RosterMember, WebsiteStoreError, LIVE_COUNTER_INTERVAL_MS,
    RANK_SNAPSHOT_INTERVAL_MS, SCHEDULED_EVENTS_INTERVAL_MS,
};
use two_bot_discord::executor::ActionExecutor;

use crate::{
    audit_runtime, community_jobs, feed_jobs,
    jobs::{self, ErrorClass, Job},
    member_runtime::{self, MemberRuntime},
    scheduled_jobs,
    self_role_handlers::{SelfRoleService, RECOVERY_JOB_NAME},
    server,
};

pub const NAMES: [&str; 3] = ["counter", "rank", "scheduled_events"];

#[cfg(test)]
#[path = "website_jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "website_snapshot_tests.rs"]
mod snapshot_tests;

#[derive(Clone, Copy)]
pub enum Kind {
    Counter,
    Rank,
    Events,
}

/// Cadence overrides are compiled only into tests, never the deployed binary.
fn cadence(kind: Kind) -> Duration {
    let millis = match kind {
        Kind::Counter => LIVE_COUNTER_INTERVAL_MS,
        Kind::Rank => RANK_SNAPSHOT_INTERVAL_MS,
        Kind::Events => SCHEDULED_EVENTS_INTERVAL_MS,
    };
    #[cfg(test)]
    let millis = {
        let name = match kind {
            Kind::Counter => "TWO_TEST_COUNTER_INTERVAL_MS",
            Kind::Rank => "TWO_TEST_RANK_INTERVAL_MS",
            Kind::Events => "TWO_TEST_EVENTS_INTERVAL_MS",
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

fn admission_pool(url: &str) -> Result<PgPool, String> {
    let options = two_bot_core::database_url::connect_options(url)
        .map_err(|_| "invalid admission authority".to_owned())?;
    Ok(sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy_with(options))
}

/// Boot-composed single call: the eight parameters are the full supervised
/// surface (config, listener, gateway, shutdown plus one slot per consumer).
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    config: &Config,
    listener: tokio::net::TcpListener,
    gateway: server::SharedState,
    shutdown: watch::Sender<bool>,
    self_roles: Option<Arc<SelfRoleService>>,
    automod: crate::automod_gateway::Slot,
    receiver: Option<crate::internal_action_http::BoundReceiver>,
    member: Option<Arc<MemberRuntime>>,
) -> std::io::Result<()> {
    let mut registered = Vec::new();
    let mut parked = Vec::new();
    if let Ok((token, url, guild)) = crate::gateway_prerequisites(config) {
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
                });
                for (name, kind) in NAMES
                    .into_iter()
                    .zip([Kind::Counter, Kind::Rank, Kind::Events])
                {
                    let context = context.clone();
                    let shutdown = shutdown.subscribe();
                    let cadence = cadence(kind);
                    registered.push(Job {
                        name,
                        cadence,
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
                                        run_once(
                                            kind,
                                            context.pool().await?,
                                            &context.rest,
                                            &context.guild,
                                            &context.observation,
                                            &shutdown,
                                        ).await
                                    } => result,
                                }
                            })
                        }),
                    });
                }
                registered.push(scheduled_jobs::register(context.clone()));
                // The unban sweep shares the boot-composed member consumer:
                // one guild store across commands and sweep, never a second
                // same-guild consumer with its own local queues.
                if let Some(member) = member {
                    registered.push(member_runtime::sweep_job(member, context.rest.clone()));
                }
                let registration = community_jobs::register(context.clone());
                registered.extend(registration.jobs);
                parked = registration.parked;
                if let Some(job) = feed_jobs::register(context.clone()) {
                    registered.push(job);
                } else {
                    parked.push(feed_jobs::NAME);
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
    if let Some(service) = self_roles {
        registered.push(service.recovery_job());
    }
    let status = registered_statuses(&registered, &parked).await;
    let public = server::serve(listener, gateway, status.clone(), shutdown.clone());
    let http = async {
        match receiver {
            Some(receiver) => {
                serve_listeners(
                    public,
                    receiver.serve(shutdown.subscribe()),
                    shutdown.clone(),
                )
                .await
            }
            None => public.await,
        }
    };
    serve_jobs(registered, status, shutdown.clone(), http).await
}

async fn registered_statuses(registered: &[Job], parked: &[&str]) -> jobs::SharedStatus {
    let mut names: Vec<&'static str> = NAMES
        .into_iter()
        .chain(community_jobs::NAMES)
        .chain(audit_runtime::NAMES)
        .chain(scheduled_jobs::NAMES)
        .chain(member_runtime::NAMES)
        .chain([RECOVERY_JOB_NAME])
        .chain([feed_jobs::NAME])
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
        for job in registered {
            entries
                .get_mut(job.name)
                .expect("known registered job")
                .parked = false;
        }
        for name in parked {
            if let Some(entry) = entries.get_mut(*name) {
                entry.parked = true;
            }
        }
    }
    status
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
    let shutdown = Shutdown(shutdown);
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

fn snowflake(value: &Value) -> Result<u64, ErrorClass> {
    value
        .as_str()
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .ok_or(ErrorClass::Rest)
}

fn roster_page(page: &Value, after: &mut u64) -> Result<Vec<RosterMember>, ErrorClass> {
    let page = page.as_array().ok_or(ErrorClass::Rest)?;
    if page.len() > 1000 {
        return Err(ErrorClass::Rest);
    }
    let mut members = Vec::with_capacity(page.len());
    for member in page {
        let user = &member["user"];
        let id = snowflake(&user["id"])?;
        if id <= *after {
            return Err(ErrorClass::Rest);
        }
        *after = id;
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
        members.push(RosterMember {
            user_id: id.to_string(),
            is_bot,
            roles,
        });
    }
    Ok(members)
}

/// Discord's paginated roster is the denominator, never approximate counts.
/// Website publication still requires the entire roster.
pub(crate) async fn roster(
    rest: &ActionExecutor,
    guild: &str,
) -> Result<Vec<RosterMember>, ErrorClass> {
    let mut after = 0;
    let mut members = Vec::new();
    loop {
        let page = get(
            rest,
            &format!("/guilds/{guild}/members?limit=1000&after={after}"),
        )
        .await?;
        let page = roster_page(&page, &mut after)?;
        let complete = page.len() < 1000;
        members.extend(page);
        if complete {
            return Ok(members);
        }
    }
}

/// Legacy BOT_FLOOR_MAX_PAGES: ten full pages plus one termination probe.
pub(crate) const BOT_FLOOR_MAX_PAGES: usize = 11;

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

fn raw_event(value: &Value) -> Result<RawScheduledEvent, ErrorClass> {
    if !value.is_object() {
        return Err(ErrorClass::Rest);
    }
    snowflake(&value["id"])?;
    let optional = |name: &str| match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(ErrorClass::Rest),
    };
    Ok(RawScheduledEvent {
        id: optional("id")?,
        name: optional("name")?,
        scheduled_start_time: optional("scheduled_start_time")?,
        channel_id: optional("channel_id")?,
        description: optional("description")?,
        status: value["status"].as_i64(),
    })
}

/// Cancel queued/in-flight observations directly, without waiting for the job
/// supervisor to abort us. Biased selection discards a simultaneously-ready REST
/// result; publication fences also cover shutdown arriving during that poll.
/// This cannot undo a database commit already submitted before shutdown.
pub async fn run_once(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
    shutdown: &watch::Receiver<bool>,
) -> Result<(), ErrorClass> {
    tokio::select! {
        biased;
        _ = server::shutdown_requested(shutdown.clone()) => Ok(()),
        result = snapshot_once(kind, pool, rest, guild, observation, shutdown) => result,
    }
}

fn publication_stopped(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}

async fn snapshot_once(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
    shutdown: &watch::Receiver<bool>,
) -> Result<(), ErrorClass> {
    if matches!(kind, Kind::Events) {
        let response = get(
            rest,
            &format!("/guilds/{guild}/scheduled-events?with_user_count=true"),
        )
        .await?;
        let raw = response
            .as_array()
            .ok_or(ErrorClass::Rest)?
            .iter()
            .map(raw_event)
            .collect::<Result<Vec<_>, _>>()?;
        let events = normalize_events(&raw).ok_or(ErrorClass::Rest)?;
        if publication_stopped(shutdown) {
            return Ok(());
        }
        return replace_events(pool, guild, &now_iso(), &events)
            .await
            .map_err(|_| ErrorClass::Database);
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
    let members = roster(rest, guild).await?;
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
    let ladder = match_rank_roles(&roles).ok_or(ErrorClass::Configuration)?;
    let snapshot = build_community_snapshot(&members, &ladder, &windows).ok_or(ErrorClass::Rest)?;
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
