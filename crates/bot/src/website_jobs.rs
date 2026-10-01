//! Runtime adapters for the three existing website-contract domains, plus the
//! `guild_settings` hot-reload poll registered alongside them (TOG-10898).

use std::{sync::Arc, time::Duration};

use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::{watch, Mutex, OnceCell};
use two_bot_core::{
    apply_web_contract, build_community_snapshot, build_counter_reading, match_rank_roles,
    normalize_events, now_iso, read_raid_windows, replace_events, write_counter,
    write_rank_snapshot, Config, RawScheduledEvent, RosterMember, WebsiteStoreError,
    LIVE_COUNTER_INTERVAL_MS, RANK_SNAPSHOT_INTERVAL_MS, SCHEDULED_EVENTS_INTERVAL_MS,
};
use two_bot_discord::executor::ActionExecutor;

use crate::{
    jobs::{self, ErrorClass, Job},
    server,
};

/// Every supervised job name, in `/readyz` order. The settings poll needs
/// only the database, so it is registered separately from the REST-backed
/// domains in [`DOMAINS`].
pub const NAMES: [&str; 4] = [
    "counter",
    "rank",
    "scheduled_events",
    crate::settings_jobs::NAME,
];

/// The REST-backed website-contract domains.
const DOMAINS: [(&str, Kind); 3] = [
    ("counter", Kind::Counter),
    ("rank", Kind::Rank),
    ("scheduled_events", Kind::Events),
];

#[cfg(test)]
#[path = "website_jobs_tests.rs"]
mod tests;

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

struct Context {
    url: String,
    pool: OnceCell<PgPool>,
    rest: ActionExecutor,
    guild: String,
    observation: Mutex<()>,
}

impl Context {
    async fn pool(&self) -> Result<&PgPool, ErrorClass> {
        self.pool
            .get_or_try_init(|| async {
                let db = two_bot_cutover::connect(
                    &self.url,
                    two_bot_cutover::DB_POOL_MAX_DEFAULT,
                    false,
                )
                .await
                .map_err(|_| ErrorClass::Database)?;
                let pool = db.pool().clone();
                apply_web_contract(&pool)
                    .await
                    .map_err(|_| ErrorClass::Database)?;
                Ok(pool)
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

pub async fn serve(
    config: &Config,
    listener: tokio::net::TcpListener,
    gateway: server::SharedState,
    shutdown: watch::Sender<bool>,
) -> std::io::Result<()> {
    let mut registered = Vec::new();
    if let Ok((token, url, guild)) = crate::gateway_prerequisites(config) {
        // The settings poll is DB-only: register it before REST construction
        // so a bad DISCORD_API_BASE cannot park hot reload.
        registered.push(crate::settings_jobs::job(url));
        match ActionExecutor::with_proxy(token.to_owned(), std::env::var("DISCORD_API_BASE").ok()) {
            Ok(rest) => {
                let context = Arc::new(Context {
                    url: url.to_owned(),
                    pool: OnceCell::new(),
                    rest,
                    guild: guild.to_string(),
                    observation: Mutex::new(()),
                });
                for (name, kind) in DOMAINS {
                    let context = context.clone();
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
                            Box::pin(async move {
                                run_once(
                                    kind,
                                    context.pool().await?,
                                    &context.rest,
                                    &context.guild,
                                    &context.observation,
                                )
                                .await
                            })
                        }),
                    });
                }
            }
            Err(_) => tracing::warn!("website jobs parked: invalid REST configuration"),
        }
    } else {
        tracing::info!("website jobs parked: gateway prerequisites missing");
    }
    // `parked` means "not registered this boot": every job starts parked and
    // only registered names clear it, so a parked settings poll or a parked
    // website domain is distinguishable on /readyz.
    let status = jobs::statuses(&NAMES, true);
    {
        let mut statuses = status.write().await;
        for job in &registered {
            statuses.get_mut(job.name).expect("named job").parked = false;
        }
    }
    let http = server::serve(listener, gateway, status.clone(), shutdown.clone());
    serve_jobs(registered, status, shutdown, http).await
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

async fn get(rest: &ActionExecutor, path: &str) -> Result<Value, ErrorClass> {
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

/// Discord's paginated roster is the denominator, never approximate counts.
async fn roster(rest: &ActionExecutor, guild: &str) -> Result<Vec<RosterMember>, ErrorClass> {
    let mut after = 0;
    let mut members = Vec::new();
    loop {
        let page = get(
            rest,
            &format!("/guilds/{guild}/members?limit=1000&after={after}"),
        )
        .await?;
        let page = page.as_array().ok_or(ErrorClass::Rest)?;
        if page.len() > 1000 {
            return Err(ErrorClass::Rest);
        }
        for member in page {
            let user = &member["user"];
            let id = snowflake(&user["id"])?;
            if id <= after {
                return Err(ErrorClass::Rest);
            }
            after = id;
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
        if page.len() < 1000 {
            return Ok(members);
        }
    }
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

pub async fn run_once(
    kind: Kind,
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
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
        return replace_events(pool, guild, &now_iso(), &events)
            .await
            .map_err(|_| ErrorClass::Database);
    }
    // Both kinds publish the denominator. Hold one shared lane from the first
    // observation through commit so a slow rank tick cannot overwrite a newer
    // counter roster. Events use independent tables and do not take this lock.
    let _observation = observation.lock().await;
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
    let members = roster(rest, guild).await?;
    if matches!(kind, Kind::Counter) {
        let reading = build_counter_reading(&members, &windows).ok_or(ErrorClass::Rest)?;
        let count =
            i32::try_from(reading.human_member_count).map_err(|_| ErrorClass::Configuration)?;
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
    write_rank_snapshot(pool, guild, &now_iso(), &snapshot)
        .await
        .map_err(|error| match error {
            WebsiteStoreError::Invariant { .. } => ErrorClass::Configuration,
            _ => ErrorClass::Database,
        })
}
