//! Process-wide Discord invalid-request budget, global cooldown, per-route
//! bucket deadlines and shared global pacing for the backfill REST client.
//!
//! The mutex covers bookkeeping only, never network I/O or a sleep. Already
//! in-flight requests may finish after a breaker opens; every new attempt
//! checks admission again. Clones and independently constructed executors
//! share [`process_guard`], including callers using an API proxy.
//!
//! Rate-limit facts below follow the official rate-limits page
//! (<https://docs.discord.com/developers/topics/rate-limits>, verified
//! 2026-10-12): per-route buckets via `X-RateLimit-Bucket`/`Limit`/
//! `Remaining`/`Reset`/`Reset-After`, bots capped at 50 requests/second
//! globally, 429-only `X-RateLimit-Global`/`Scope` (`user`/`global`/`shared`)
//! with `Retry-After`/`retry_after` waits, and 401/403/429 responses counting
//! toward the 10k-per-10-minute invalid-request cutoff except `shared`-scope
//! 429s. Every per-route number honored here arrives in a response header;
//! the only constants are the documented 50/second floor (which no header
//! carries) and the legacy 250 ms safety pad the 429 path already uses.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::time::Instant;

pub const INVALID_REQUEST_WINDOW: Duration = Duration::from_secs(600);
pub const DEFAULT_INVALID_REQUEST_THRESHOLD: usize = 5_000;
/// Bot-wide request ceiling, requests/second (official rate-limits page,
/// verified 2026-10-12). No response header carries it, so the shared pacing
/// floor below names the documented value instead of inferring one.
pub const GLOBAL_REQUESTS_PER_SECOND: u64 = 50;
/// Minimum spacing between admissions on one guard, derived from
/// [`GLOBAL_REQUESTS_PER_SECOND`] (1000/50): independent executors sharing a
/// guard spread out instead of bursting past the documented global rate.
pub const GLOBAL_MIN_INTERVAL: Duration = Duration::from_millis(1000 / GLOBAL_REQUESTS_PER_SECOND);
/// Extra safety margin folded into each pacing slot (conservative over the
/// documented floor, never under it): timer wake granularity plus the
/// post-admission dispatch overhead (request build, lane queue, connection
/// reuse variance) can otherwise compress the wire spacing a fraction under
/// the floor — nightly saw 99.99 ms across five 20 ms slots vs the 100 ms
/// acceptance threshold. Two milliseconds per slot (≈45 req/s) keeps wire
/// spacing at or above the documented 50/s floor under loaded-runner jitter.
pub const GLOBAL_PACING_SAFETY_PAD: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, Copy)]
pub struct GuardConfig {
    pub invalid_request_threshold: usize,
    pub window: Duration,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            invalid_request_threshold: DEFAULT_INVALID_REQUEST_THRESHOLD,
            window: INVALID_REQUEST_WINDOW,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    #[error("discord invalid-request circuit breaker is open")]
    CircuitOpen,
    #[error("discord bot token is invalid (token_invalid)")]
    TokenInvalid,
    #[error("discord admission timed out before any wire attempt")]
    AdmissionTimeout,
    #[error("discord global admission closed after authorization")]
    GlobalPaused,
}

/// Low-cardinality counters/gauges for a metrics exporter; no tokens, routes,
/// guild IDs or response bodies are retained.
#[derive(Debug, Clone, Copy, Default)]
pub struct GuardSnapshot {
    pub invalid_requests_in_window: usize,
    pub invalid_requests_total: u64,
    pub rejected_requests_total: u64,
    pub breaker_opens_total: u64,
    pub breaker_closes_total: u64,
    pub global_pauses_total: u64,
    pub breaker_open: bool,
    pub token_invalid: bool,
    pub global_pause_remaining: Duration,
    /// Admission stays closed while global-header response bodies are unresolved,
    /// even if the provisional remaining time reaches zero.
    pub pending_global_responses: usize,
}

impl From<GuardSnapshot> for two_bot_core::metrics::RestGuardSnapshot {
    fn from(snapshot: GuardSnapshot) -> Self {
        Self {
            invalid_requests_in_window: snapshot.invalid_requests_in_window,
            invalid_requests_total: snapshot.invalid_requests_total,
            rejected_requests_total: snapshot.rejected_requests_total,
            breaker_opens_total: snapshot.breaker_opens_total,
            breaker_closes_total: snapshot.breaker_closes_total,
            global_pauses_total: snapshot.global_pauses_total,
            breaker_open: snapshot.breaker_open,
            token_invalid: snapshot.token_invalid,
            global_pause_remaining: snapshot.global_pause_remaining,
            pending_global_responses: snapshot.pending_global_responses,
        }
    }
}

/// Per-route bucket deadlines learned from `X-RateLimit-*` response headers,
/// keyed by bucket id scoped to the request's top-level resource (or the
/// request route when the response carries no bucket id). Debug reports the
/// count only: keys name routes and buckets, and metrics stay low-cardinality
/// (no tokens, routes, guild IDs or bodies).
#[derive(Default)]
struct BucketTable(HashMap<String, Instant>);

impl std::fmt::Debug for BucketTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketTable")
            .field("buckets", &self.0.len())
            .finish()
    }
}

/// Request-route to bucket-id mapping learned alongside [`BucketTable`]: the
/// admit path knows the request route, the response carries the bucket id.
#[derive(Default)]
struct RouteTable(HashMap<String, String>);

impl std::fmt::Debug for RouteTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteTable")
            .field("routes", &self.0.len())
            .finish()
    }
}

#[derive(Debug, Default)]
struct State {
    invalid: VecDeque<Instant>,
    global_until: Option<Instant>,
    pending_global: Vec<(u64, Instant)>,
    next_global_id: u64,
    buckets: BucketTable,
    routes: RouteTable,
    /// Last shared-pacing release, tagged with the runtime whose clock
    /// minted it. Instants from another runtime's clock identify nothing on
    /// this one (the process guard outlives individual test runtimes), so a
    /// foreign tag simply starts a fresh pacing chain here.
    last_pace_release: Option<(tokio::runtime::Id, Instant)>,
    counters: GuardSnapshot,
}

#[derive(Debug)]
pub struct RateLimitGuard {
    config: GuardConfig,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}

impl RateLimitGuard {
    pub fn new(config: GuardConfig) -> Result<Self, String> {
        if config.invalid_request_threshold == 0 || config.window.is_zero() {
            return Err("Discord guard threshold and window must be positive".to_owned());
        }
        Ok(Self {
            config,
            state: Mutex::new(State::default()),
            changed: tokio::sync::Notify::new(),
        })
    }

    fn refresh(&self, state: &mut State, now: Instant) {
        while state
            .invalid
            .front()
            .is_some_and(|at| now.duration_since(*at) >= self.config.window)
        {
            state.invalid.pop_front();
        }
        let open = state.invalid.len() >= self.config.invalid_request_threshold;
        if open != state.counters.breaker_open {
            state.counters.breaker_open = open;
            if open {
                state.counters.breaker_opens_total += 1;
                tracing::warn!(
                    event = "discord_breaker_open",
                    invalid_requests = state.invalid.len(),
                    threshold = self.config.invalid_request_threshold,
                    "Discord invalid-request budget exhausted"
                );
            } else {
                state.counters.breaker_closes_total += 1;
                tracing::info!(
                    event = "discord_breaker_close",
                    invalid_requests = state.invalid.len(),
                    "Discord invalid-request budget cooled"
                );
            }
        }
        if state.global_until.is_some_and(|until| until <= now) {
            state.global_until = None;
        }
        // Expired bucket deadlines leave with the window: routes that still
        // map to them simply miss on the next admission, so neither table
        // grows past the live bucket set.
        state.buckets.0.retain(|_, until| *until > now);
        let orphaned: Vec<String> = state
            .routes
            .0
            .iter()
            .filter(|(_, bucket)| !state.buckets.0.contains_key(*bucket))
            .map(|(route, _)| route.clone())
            .collect();
        for route in orphaned {
            state.routes.0.remove(&route);
        }
    }

    /// Only interaction acknowledgements are essential at the executor seam.
    /// They may bypass the invalid budget, never global pauses or token_invalid.
    pub async fn admit(&self, essential: bool) -> Result<(), GuardError> {
        self.admit_inner(None, essential, false).await
    }

    /// Backfill admission for one request route: everything [`Self::admit`]
    /// enforces, plus the per-route bucket deadline learned from
    /// `X-RateLimit-*` response headers (see [`Self::observe_bucket`]) and the
    /// shared [`GLOBAL_MIN_INTERVAL`] pacing floor. Routes without learned
    /// state admit exactly like [`Self::admit`]. This is the interface every
    /// backfill endpoint slice reuses. `route` is the shared bucket key the
    /// executor derives (HTTP method plus path, without the query string);
    /// admission and observation must use the same key for one request.
    pub async fn admit_route(&self, route: &str, essential: bool) -> Result<(), GuardError> {
        self.admit_inner(Some(route), essential, true).await
    }

    /// Gate wait for one route with no pacing turn: the breaker, pending
    /// and global pauses and the learned bucket deadline, exactly like
    /// [`Self::admit_route`]. Lane callers wait here before taking their
    /// lane lock (and again after a restriction arrives mid-lane), so a
    /// route-local bucket wait never parks unrelated routes behind a lane.
    pub(crate) async fn admit_route_recheck(
        &self,
        route: &str,
        essential: bool,
    ) -> Result<(), GuardError> {
        self.admit_inner(Some(route), essential, false).await
    }

    async fn admit_inner(
        &self,
        route: Option<&str>,
        essential: bool,
        pace: bool,
    ) -> Result<(), GuardError> {
        // Take the pacing turn once for this admission, and only on a pass
        // where every other gate is clear: pacing before a long pause would
        // stale-date the spacing and release every waiter together when the
        // pause ends, and refusals must not consume spacing at all. After
        // the turn, loop back through the gates — a restriction may have
        // arrived during the (at most one-interval) pacing sleep — but
        // never pace twice for one admission.
        let mut paced = false;
        enum Step {
            WaitPending,
            Sleep { until: Option<Instant> },
        }
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let step = {
                let mut state = self.state.lock().expect("Discord guard state");
                let now = Instant::now();
                self.refresh(&mut state, now);
                if state.counters.token_invalid {
                    state.counters.rejected_requests_total += 1;
                    return Err(GuardError::TokenInvalid);
                }
                if state.counters.breaker_open && !essential {
                    state.counters.rejected_requests_total += 1;
                    return Err(GuardError::CircuitOpen);
                }
                if !state.pending_global.is_empty() {
                    Step::WaitPending
                } else {
                    let mut until = Self::global_deadline(&state, now);
                    if let Some(route) = route {
                        until = until
                            .into_iter()
                            .chain(self.bucket_deadline(&state, route, now))
                            .max();
                    }
                    Step::Sleep { until }
                }
            };
            match step {
                // A short header estimate cannot release admission while the
                // body may still supply a longer authoritative deadline.
                // Finish/Drop or a fatal/breaker transition wakes us; timed
                // callers can cancel.
                Step::WaitPending => {
                    changed.await;
                }
                Step::Sleep { until } => match until {
                    // Another in-flight response may extend the shared pause.
                    Some(until) => tokio::select! {
                        _ = tokio::time::sleep_until(until) => {},
                        _ = &mut changed => {},
                    },
                    None => {
                        if pace && !paced {
                            self.pace_once().await;
                            paced = true;
                            continue;
                        }
                        return Ok(());
                    }
                },
            }
        }
    }

    /// One shared pacing turn: reserve the next paced slot after the previous
    /// turn's release on this guard, wait it out, and leave the reservation
    /// for the next turn. Each slot is [`GLOBAL_MIN_INTERVAL`] plus
    /// [`GLOBAL_PACING_SAFETY_PAD`]: the pad keeps wire spacing at or above
    /// the documented floor under timer granularity and post-admission
    /// dispatch overhead. The reservation is taken under the bookkeeping
    /// lock and the sleep holds no lock, so a burst of admitters spaces
    /// itself one interval at a time without ever parking on a shared async
    /// gate: parallel paused-clock tests share the process guard, and
    /// parking one runtime on another's turn advances the parked runtime
    /// to its outer timeout while it makes no progress. A previous release
    /// minted on another runtime's clock (foreign Instants name nothing on
    /// this clock) starts a fresh chain here instead of waiting.
    /// Same-runtime reservations chain behind each other, bounded by the
    /// live concurrency times one interval.
    async fn pace_once(&self) {
        let runtime = tokio::runtime::Handle::try_current()
            .ok()
            .map(|handle| handle.id());
        let wait = {
            let mut state = self.state.lock().expect("Discord guard state");
            let now = Instant::now();
            match (state.last_pace_release, runtime) {
                (Some((id, last)), Some(mine)) if id == mine => {
                    let interval = GLOBAL_MIN_INTERVAL + GLOBAL_PACING_SAFETY_PAD;
                    let earliest = last.checked_add(interval).unwrap_or(now);
                    let wait = earliest.saturating_duration_since(now);
                    state.last_pace_release = Some((mine, earliest.max(now)));
                    wait
                }
                _ => {
                    if let Some(mine) = runtime {
                        state.last_pace_release = Some((mine, now));
                    }
                    Duration::ZERO
                }
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Scope a Discord bucket hash to the request's top-level resource.
    /// `X-RateLimit-Bucket` is non-inclusive of top-level resources in the path
    /// (official rate-limits page, verified 2026-10-12): one hash is reused
    /// across channels, guilds and webhooks whose limits are calculated
    /// independently. Keying deadlines on the bare hash lets one channel's
    /// remaining budget delete another channel's live deadline (the next read
    /// 429s and spends invalid budget) and lets one exhausted channel stall
    /// every other channel sharing the hash.
    fn scoped_bucket_key(bucket_hash: &str, route: &str) -> String {
        format!("{bucket_hash}:{}", Self::top_level_resource(route))
    }

    /// Top-level resource for one request route (`channels/{id}`,
    /// `guilds/{id}`, `webhooks/{id}[/{token}]`); the route is the executor's
    /// bucket key (HTTP method plus path, without the query string), so the
    /// method prefix and any stray leading slash or query are stripped first.
    /// Unrecognized shapes fall back to the full path: unrelated routes never
    /// share a deadline, at the cost of one entry per distinct path.
    fn top_level_resource(route: &str) -> String {
        let path = route.split_once(' ').map_or(route, |(_, path)| path);
        let path = path.strip_prefix('/').unwrap_or(path);
        let path = path.split('?').next().unwrap_or(path);
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        let first = segments.next().unwrap_or("");
        let second = segments.next().unwrap_or("");
        match first {
            // `webhooks/{id}/{token}` names the webhook; deeper segments are
            // message ids under it.
            "webhooks" => {
                let third = segments.next().unwrap_or("");
                if third.is_empty() {
                    format!("webhooks/{second}")
                } else {
                    format!("webhooks/{second}/{third}")
                }
            }
            "channels" | "guilds" => {
                if second.is_empty() {
                    first.to_owned()
                } else {
                    format!("{first}/{second}")
                }
            }
            _ => path.to_owned(),
        }
    }

    /// Deadline for one request route from learned bucket state, if any.
    fn bucket_deadline(&self, state: &State, route: &str, now: Instant) -> Option<Instant> {
        let key = state.routes.0.get(route).map_or(route, String::as_str);
        state
            .buckets
            .0
            .get(key)
            .copied()
            .filter(|until| *until > now)
    }

    /// Learn one response's per-route bucket state from its `X-RateLimit-*`
    /// headers. Only an exhausted bucket (`Remaining` 0 with a resolvable
    /// reset) installs a deadline; a response with remaining budget retires a
    /// live one only when its own reset does not predate the stored deadline.
    /// Every number honored arrives in the headers — the relative
    /// `Reset-After` wins when present, the absolute `Reset` epoch is measured
    /// against the wall clock otherwise, and the only constant is the legacy
    /// 250 ms safety pad the 429 path already uses. Wakes admission only when
    /// a live deadline is cleared; setting one never needs to (sleepers wait
    /// on the deadline itself).
    pub fn observe_bucket(&self, route: &str, headers: &http::HeaderMap) {
        let observed_at = Instant::now();
        let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let bucket = match value("x-ratelimit-bucket") {
            Some(hash) => Self::scoped_bucket_key(hash, route),
            None => route.to_owned(),
        };
        let remaining = value("x-ratelimit-remaining").and_then(|v| v.parse::<u64>().ok());
        let reset_wait = value("x-ratelimit-reset-after")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|secs| secs.is_finite() && *secs >= 0.0)
            .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
            .or_else(|| {
                let epoch = value("x-ratelimit-reset")?
                    .parse::<f64>()
                    .ok()
                    .filter(|secs| secs.is_finite() && *secs >= 0.0)?;
                let target =
                    std::time::UNIX_EPOCH.checked_add(Duration::try_from_secs_f64(epoch).ok()?)?;
                target.duration_since(std::time::SystemTime::now()).ok()
            });
        let mut state = self.state.lock().expect("Discord guard state");
        let cleared_live = match (remaining, reset_wait) {
            (Some(0), Some(wait)) => {
                let wait = wait
                    .checked_add(Duration::from_millis(two_bot_core::RETRY_AFTER_PADDING_MS))
                    .unwrap_or(wait);
                let until = observed_at
                    .checked_add(wait)
                    .unwrap_or(observed_at + self.config.window);
                // Never shorten a restriction from the same bucket; a changed
                // bucket id replaces the route mapping instead of extending
                // the old bucket's deadline.
                let entry = state.buckets.0.entry(bucket.clone()).or_insert(until);
                *entry = (*entry).max(until);
                state.routes.0.insert(route.to_owned(), bucket);
                false
            }
            (Some(_), _) => {
                // A response with remaining budget retires a live deadline
                // only when its own reset does not predate the stored one:
                // in-flight replies can arrive out of order within one
                // resource, and without reset timing there is nothing to
                // order against, so a live deadline stands. Cross-resource
                // interference is already impossible: the key is scoped per
                // top-level resource above.
                let stored = state
                    .buckets
                    .0
                    .get(bucket.as_str())
                    .copied()
                    .filter(|until| *until > observed_at);
                let response_until = reset_wait
                    .and_then(|wait| {
                        wait.checked_add(Duration::from_millis(
                            two_bot_core::RETRY_AFTER_PADDING_MS,
                        ))
                    })
                    .and_then(|wait| observed_at.checked_add(wait));
                let clears = match (stored, response_until) {
                    (Some(stored), Some(response_until)) => response_until >= stored,
                    (Some(_), None) | (None, _) => false,
                };
                if clears {
                    state.buckets.0.remove(bucket.as_str());
                }
                state.routes.0.insert(route.to_owned(), bucket);
                clears
            }
            (None, _) => false,
        };
        drop(state);
        if cleared_live {
            self.changed.notify_waiters();
        }
    }

    /// Withdraw one invalid-request tally (a `shared`-scope 429 whose body
    /// arrived after its headers were tallied): the window entry leaves and
    /// the breaker re-evaluates, possibly closing again.
    pub(crate) fn retract_invalid(&self, at: Instant) {
        let mut state = self.state.lock().expect("Discord guard state");
        if let Some(pos) = state.invalid.iter().position(|entry| *entry == at) {
            state.invalid.remove(pos);
        }
        state.counters.invalid_requests_total =
            state.counters.invalid_requests_total.saturating_sub(1);
        self.refresh(&mut state, Instant::now());
        drop(state);
        self.changed.notify_waiters();
    }

    /// A late DB authorization must never be followed by another admission
    /// sleep. Refuse locally if a restriction arrived during that authorization.
    pub(crate) fn check_now(&self, essential: bool) -> Result<(), GuardError> {
        let mut state = self.state.lock().expect("Discord guard state");
        let now = Instant::now();
        self.refresh(&mut state, now);
        let error = if state.counters.token_invalid {
            Some(GuardError::TokenInvalid)
        } else if state.counters.breaker_open && !essential {
            Some(GuardError::CircuitOpen)
        } else if !state.pending_global.is_empty() || Self::global_deadline(&state, now).is_some() {
            Some(GuardError::GlobalPaused)
        } else {
            None
        };
        if let Some(error) = error {
            state.counters.rejected_requests_total += 1;
            return Err(error);
        }
        Ok(())
    }

    /// Non-blocking route re-check for lane holders: [`Self::check_now`]
    /// plus the learned bucket deadline for `route`. A pending global or
    /// bucket pause reports [`GuardError::GlobalPaused`] without spending
    /// a rejection — the caller releases its lane and waits the pause out
    /// through [`Self::admit_route_recheck`], so the pause is a retry, not
    /// a refusal. Terminal refusals count exactly like admission's.
    pub(crate) fn check_route_now(&self, route: &str, essential: bool) -> Result<(), GuardError> {
        let mut state = self.state.lock().expect("Discord guard state");
        let now = Instant::now();
        self.refresh(&mut state, now);
        let terminal = if state.counters.token_invalid {
            Some(GuardError::TokenInvalid)
        } else if state.counters.breaker_open && !essential {
            Some(GuardError::CircuitOpen)
        } else {
            None
        };
        if let Some(error) = terminal {
            state.counters.rejected_requests_total += 1;
            return Err(error);
        }
        if !state.pending_global.is_empty()
            || Self::global_deadline(&state, now).is_some()
            || self.bucket_deadline(&state, route, now).is_some()
        {
            return Err(GuardError::GlobalPaused);
        }
        Ok(())
    }

    /// Account as soon as headers arrive, even if the response body fails or
    /// the caller cancels while reading it. Callback-token 401s aren't bot 401s.
    pub fn observe_status(&self, status: u16, bot_authenticated: bool) {
        if !matches!(status, 401 | 403 | 429) {
            return;
        }
        let mut state = self.state.lock().expect("Discord guard state");
        self.observe_status_locked(&mut state, status, bot_authenticated, Instant::now());
        drop(state);
        // Recheck fatal/breaker admission immediately, not at the end of an
        // unrelated global cooldown or unresolved response body.
        self.changed.notify_waiters();
    }

    fn observe_status_locked(
        &self,
        state: &mut State,
        status: u16,
        bot_authenticated: bool,
        now: Instant,
    ) {
        if !matches!(status, 401 | 403 | 429) {
            return;
        }
        self.refresh(state, now);
        state.counters.invalid_requests_total += 1;
        state.invalid.push_back(now);
        if status == 401 && bot_authenticated && !state.counters.token_invalid {
            state.counters.token_invalid = true;
            tracing::error!(
                event = "discord_token_invalid",
                "Discord refused the bot token; REST disabled until restart"
            );
        }
        self.refresh(state, now);
    }

    /// Extend (never shorten) a global pause. Missing/invalid timing fails
    /// closed for a full budget window, rather than repeatedly hitting 429.
    pub fn observe_global(&self, retry_after_secs: Option<f64>) {
        let observed_at = Instant::now();
        let id = self.begin_global(retry_after_secs, observed_at);
        self.finish_global(id, retry_after_secs, observed_at);
    }

    fn global_deadline(state: &State, now: Instant) -> Option<Instant> {
        state
            .global_until
            .into_iter()
            .chain(state.pending_global.iter().map(|(_, until)| *until))
            .filter(|until| *until > now)
            .max()
    }

    fn global_until(&self, retry_after_secs: Option<f64>, observed_at: Instant) -> Instant {
        let wait = retry_after_secs
            .filter(|secs| secs.is_finite() && *secs >= 0.0)
            .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
            .and_then(|wait| wait.checked_add(Duration::from_millis(250)))
            .unwrap_or(self.config.window);
        observed_at
            .checked_add(wait)
            .unwrap_or(observed_at + self.config.window)
    }

    fn begin_global(&self, timing: Option<f64>, observed_at: Instant) -> u64 {
        let mut state = self.state.lock().expect("Discord guard state");
        self.begin_global_locked(&mut state, timing, observed_at)
    }

    fn begin_global_locked(
        &self,
        state: &mut State,
        timing: Option<f64>,
        observed_at: Instant,
    ) -> u64 {
        let until = self.global_until(timing, observed_at);
        self.refresh(state, observed_at);
        if state.pending_global.is_empty() && Self::global_deadline(state, observed_at).is_none() {
            state.counters.global_pauses_total += 1;
            tracing::warn!(
                event = "discord_global_pause",
                wait_ms = until.duration_since(observed_at).as_millis() as u64,
                "Discord global REST pause"
            );
        }
        let id = state.next_global_id;
        state.next_global_id += 1;
        state.pending_global.push((id, until));
        id
    }

    fn finish_global(&self, id: u64, timing: Option<f64>, observed_at: Instant) {
        let until = self.global_until(timing, observed_at);
        let mut state = self.state.lock().expect("Discord guard state");
        state
            .pending_global
            .retain(|(pending_id, _)| *pending_id != id);
        // Only replace this response's provisional timing. Never shorten a
        // restriction learned from another response, even while bodies overlap.
        state.global_until = Some(state.global_until.map_or(until, |old| old.max(until)));
        drop(state);
        self.changed.notify_waiters();
    }

    #[must_use]
    pub fn snapshot(&self) -> GuardSnapshot {
        let mut state = self.state.lock().expect("Discord guard state");
        let now = Instant::now();
        self.refresh(&mut state, now);
        GuardSnapshot {
            invalid_requests_in_window: state.invalid.len(),
            pending_global_responses: state.pending_global.len(),
            global_pause_remaining: Self::global_deadline(&state, now)
                .map_or(Duration::ZERO, |until| until.duration_since(now)),
            ..state.counters
        }
    }
}

/// Shared by production executors and /readyz. Invalid env values retain the
/// conservative default; configuration is captured once on first use.
#[must_use]
pub fn process_guard() -> Arc<RateLimitGuard> {
    static GUARD: OnceLock<Arc<RateLimitGuard>> = OnceLock::new();
    Arc::clone(GUARD.get_or_init(|| {
        let threshold = std::env::var("DISCORD_INVALID_REQUEST_THRESHOLD")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_INVALID_REQUEST_THRESHOLD);
        Arc::new(
            RateLimitGuard::new(GuardConfig {
                invalid_request_threshold: threshold,
                ..GuardConfig::default()
            })
            .expect("valid default Discord guard"),
        )
    }))
}

/// Keeps accounting correct if a response body errors or its future is dropped.
pub(crate) struct ResponseAccounting<'a> {
    guard: &'a RateLimitGuard,
    global_header: Option<u64>,
    retry_after_header: Option<f64>,
    reset_after_header: Option<f64>,
    invalid_tally: Option<Instant>,
    observed_at: Instant,
}

impl<'a> ResponseAccounting<'a> {
    pub(crate) fn new(
        guard: &'a RateLimitGuard,
        status: u16,
        headers: &http::HeaderMap,
        bot_authenticated: bool,
    ) -> Self {
        let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
        let global_header = status == 429
            && (header("x-ratelimit-global").is_some_and(|v| v.eq_ignore_ascii_case("true"))
                || header("x-ratelimit-scope").is_some_and(|v| v.eq_ignore_ascii_case("global")));
        let retry_after_header = header("retry-after").and_then(|v| v.parse::<f64>().ok());
        // Bucket reset hint: the only timing when a 429 carries no
        // `retry-after` (official rate-limits page, verified 2026-10-12).
        let reset_after_header =
            header("x-ratelimit-reset-after").and_then(|v| v.parse::<f64>().ok());
        // A `shared`-scope 429 is another resource's limit, never the bot's:
        // it still sleeps-then-retries, but spends no invalid budget
        // (official rate-limits page, verified 2026-10-12).
        let shared_scope = status == 429
            && header("x-ratelimit-scope").is_some_and(|v| v.eq_ignore_ascii_case("shared"));
        let observed_at = Instant::now();
        // Status and the pending restriction are one transition: a notified
        // waiter must never see the invalid response without its global pause.
        // With no usable header timing, Drop commits a full-window fallback.
        let mut state = guard.state.lock().expect("Discord guard state");
        let tallied = matches!(status, 401 | 403 | 429) && !shared_scope;
        if tallied {
            guard.observe_status_locked(&mut state, status, bot_authenticated, observed_at);
        }
        let global_header = global_header
            .then(|| guard.begin_global_locked(&mut state, retry_after_header, observed_at));
        drop(state);
        if matches!(status, 401 | 403 | 429) {
            guard.changed.notify_waiters();
        }
        Self {
            guard,
            global_header,
            retry_after_header,
            reset_after_header,
            invalid_tally: tallied.then_some(observed_at),
            observed_at,
        }
    }

    /// Return whether this response's retry is governed by shared admission,
    /// rather than a new local delay starting after body completion.
    pub(crate) fn finish(&mut self, response: &crate::executor::RawResponse) -> bool {
        let body: Option<serde_json::Value> = serde_json::from_slice(&response.body).ok();
        // Header-absent shared scope, learned from the body: withdraw the
        // tally the headers committed (the header path already exempted the
        // common case in `new`).
        if response.status == 429
            && body
                .as_ref()
                .is_some_and(|value| value.get("scope").and_then(|v| v.as_str()) == Some("shared"))
        {
            if let Some(at) = self.invalid_tally.take() {
                self.guard.retract_invalid(at);
            }
        }
        let global_body = body.as_ref().is_some_and(|value| {
            value.get("global").and_then(|v| v.as_bool()) == Some(true)
                || value.get("scope").and_then(|v| v.as_str()) == Some("global")
        });
        let global = response.status == 429 && (self.global_header.is_some() || global_body);
        if global {
            let timing = response
                .body_retry_after_secs()
                .filter(|secs| *secs >= 0.0)
                .or(self.retry_after_header)
                .or(self.reset_after_header);
            let id = self
                .global_header
                .take()
                .unwrap_or_else(|| self.guard.begin_global(timing, self.observed_at));
            self.guard.finish_global(id, timing, self.observed_at);
        }
        global
    }
}

impl Drop for ResponseAccounting<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.global_header.take() {
            self.guard.finish_global(
                id,
                self.retry_after_header.or(self.reset_after_header),
                self.observed_at,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn pending_response_snapshot_renders_paused_even_after_deadline() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        let headers = http::HeaderMap::from_iter([
            (
                http::header::HeaderName::from_static("x-ratelimit-global"),
                http::HeaderValue::from_static("true"),
            ),
            (
                http::header::RETRY_AFTER,
                http::HeaderValue::from_static("0"),
            ),
        ]);
        let accounting = ResponseAccounting::new(&guard, 429, &headers, true);
        tokio::time::advance(Duration::from_millis(250)).await;
        let metrics = two_bot_core::metrics::Metrics::default();
        let pending = metrics.render_with_rest_guard(None, guard.snapshot().into());
        for sample in [
            "two_bot_rest_guard_pending_global_responses 1\n",
            "two_bot_rest_guard_global_pause_remaining_seconds 0\n",
            "two_bot_rest_guard_global_paused 1\n",
            "two_bot_rest_guard_invalid_requests_total 1\n",
            "two_bot_rest_guard_global_pauses_total 1\n",
        ] {
            assert!(pending.contains(sample), "missing {sample}");
        }
        drop(accounting);
        let settled = metrics.render_with_rest_guard(None, guard.snapshot().into());
        assert!(settled.contains("two_bot_rest_guard_pending_global_responses 0\n"));
        assert!(settled.contains("two_bot_rest_guard_global_paused 0\n"));
        assert!(settled.contains("two_bot_rest_guard_global_pauses_total 1\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn rolling_window_and_bounded_accounting() {
        let guard = RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: 2,
            window: Duration::from_secs(600),
        })
        .unwrap();
        guard.observe_status(403, true);
        tokio::time::advance(Duration::from_secs(1)).await;
        guard.observe_status(429, true);
        assert_eq!(guard.admit(false).await, Err(GuardError::CircuitOpen));
        guard.observe_status(403, true);
        assert_eq!(guard.snapshot().invalid_requests_total, 3);
        assert_eq!(guard.snapshot().invalid_requests_in_window, 3);
        tokio::time::advance(Duration::from_secs(599)).await;
        assert!(guard.snapshot().breaker_open);
        tokio::time::advance(Duration::from_secs(1)).await;
        guard.admit(false).await.unwrap();
        assert_eq!(guard.snapshot().breaker_closes_total, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn global_wait_is_not_clamped_to_legacy_sixty_seconds() {
        let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
        guard.observe_global(Some(120.0));
        let task = tokio::spawn({
            let guard = guard.clone();
            async move { guard.admit(true).await }
        });
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(61)).await;
        task.await.unwrap().unwrap();
        assert_eq!(guard.snapshot().global_pauses_total, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_body_replaces_only_its_own_provisional_pause() {
        let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
        let mut headers = http::HeaderMap::new();
        headers.insert("x-ratelimit-global", http::HeaderValue::from_static("true"));
        let mut accounting = ResponseAccounting::new(&guard, 429, &headers, true);
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            INVALID_REQUEST_WINDOW
        );
        let waiter = tokio::spawn({
            let guard = guard.clone();
            async move { guard.admit(false).await }
        });
        tokio::task::yield_now().await;
        guard.observe_global(Some(2.0));
        tokio::time::advance(Duration::from_millis(500)).await;
        accounting.finish(&crate::executor::RawResponse {
            status: 429,
            retry_after_header: None,
            body: br#"{"retry_after":0.1}"#.to_vec(),
            completion: None,
        });
        drop(accounting);
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            Duration::from_millis(1750)
        );
        assert_eq!(guard.snapshot().global_pauses_total, 1);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        tokio::time::advance(Duration::from_millis(1750)).await;
        waiter.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_body_keeps_header_deadline_without_restarting_it() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-ratelimit-scope",
            http::HeaderValue::from_static("global"),
        );
        headers.insert("retry-after", http::HeaderValue::from_static("2"));
        let accounting = ResponseAccounting::new(&guard, 429, &headers, true);
        tokio::time::advance(Duration::from_secs(1)).await;
        drop(accounting);
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            Duration::from_millis(1250)
        );
        assert_eq!(guard.snapshot().global_pauses_total, 1);
        let accounting = ResponseAccounting::new(
            &guard,
            429,
            &http::HeaderMap::from_iter([(
                http::header::HeaderName::from_static("x-ratelimit-global"),
                http::HeaderValue::from_static("true"),
            )]),
            true,
        );
        drop(accounting);
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            INVALID_REQUEST_WINDOW
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unresolved_body_holds_admission_past_provisional_expiry() {
        for (header, elapsed, body_timing) in [
            (Some("0.1"), Duration::from_millis(500), 1.0),
            (None, Duration::from_secs(601), 700.0),
        ] {
            let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
            let mut headers = http::HeaderMap::new();
            headers.insert("x-ratelimit-global", http::HeaderValue::from_static("true"));
            if let Some(header) = header {
                headers.insert("retry-after", http::HeaderValue::from_static(header));
            }
            let observed_at = Instant::now();
            let mut accounting = ResponseAccounting::new(&guard, 429, &headers, true);
            let waiter = tokio::spawn({
                let guard = guard.clone();
                async move { guard.admit(false).await }
            });
            tokio::task::yield_now().await;
            tokio::time::advance(elapsed).await;
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished(), "unresolved header pause expired");
            assert_eq!(guard.snapshot().pending_global_responses, 1);
            assert!(accounting.finish(&crate::executor::RawResponse {
                status: 429,
                retry_after_header: header.map(str::to_owned),
                body: serde_json::json!({"retry_after": body_timing})
                    .to_string()
                    .into_bytes(),
                completion: None,
            }));
            let until =
                observed_at + Duration::from_secs_f64(body_timing) + Duration::from_millis(250);
            assert_eq!(guard.snapshot().pending_global_responses, 0);
            assert_eq!(
                guard.snapshot().global_pause_remaining,
                until - Instant::now()
            );
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished());
            tokio::time::advance(until - Instant::now()).await;
            waiter.await.unwrap().unwrap();
            assert_eq!(guard.snapshot().global_pauses_total, 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn guard_rejections_wake_sleepers_without_advancing_cooldown() {
        for pending in [false, true] {
            for status in [401, 403] {
                let guard = Arc::new(
                    RateLimitGuard::new(GuardConfig {
                        invalid_request_threshold: 2,
                        ..GuardConfig::default()
                    })
                    .unwrap(),
                );
                let mut headers = http::HeaderMap::new();
                headers.insert("x-ratelimit-global", http::HeaderValue::from_static("true"));
                let mut accounting = ResponseAccounting::new(&guard, 429, &headers, true);
                if !pending {
                    accounting.finish(&crate::executor::RawResponse {
                        status: 429,
                        retry_after_header: None,
                        body: Vec::new(),
                        completion: None,
                    });
                }
                let ordinary = tokio::spawn({
                    let guard = guard.clone();
                    async move { guard.admit(false).await }
                });
                let essential = tokio::spawn({
                    let guard = guard.clone();
                    async move { guard.admit(true).await }
                });
                tokio::task::yield_now().await;
                assert!(!ordinary.is_finished());
                assert!(!essential.is_finished());
                let now = Instant::now();
                guard.observe_status(status, true);
                tokio::task::yield_now().await;
                assert!(
                    ordinary.is_finished(),
                    "rejection did not wake global waiter"
                );
                let expected = if status == 401 {
                    GuardError::TokenInvalid
                } else {
                    GuardError::CircuitOpen
                };
                assert_eq!(ordinary.await.unwrap(), Err(expected));
                if status == 401 {
                    assert!(
                        essential.is_finished(),
                        "fatal token did not wake essential waiter"
                    );
                    assert_eq!(essential.await.unwrap(), Err(GuardError::TokenInvalid));
                } else {
                    assert!(
                        !essential.is_finished(),
                        "budget cannot reject essential callback"
                    );
                    essential.abort();
                }
                assert_eq!(Instant::now(), now, "must not wait for the cooldown timer");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_header_notification_exposes_the_complete_transition() {
        for iteration in 0..128 {
            let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).unwrap());
            // The old header estimate expired, but its body still holds admission.
            let old_at = Instant::now() - Duration::from_secs(1);
            let old_id = guard.begin_global(Some(0.0), old_at);
            let (ready, registered) = tokio::sync::oneshot::channel();
            let observer = tokio::spawn({
                let guard = guard.clone();
                async move {
                    let changed = guard.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    ready.send(()).unwrap();
                    changed.await;
                    let snapshot = guard.snapshot();
                    assert_eq!(snapshot.invalid_requests_total, 1);
                    assert_eq!(
                        snapshot.pending_global_responses, 2,
                        "status notification exposed an incomplete header transition"
                    );
                }
            });
            registered.await.unwrap();
            let mut headers = http::HeaderMap::new();
            headers.insert("x-ratelimit-global", http::HeaderValue::from_static("true"));
            headers.insert("retry-after", http::HeaderValue::from_static("0"));
            let accounting = ResponseAccounting::new(&guard, 429, &headers, true);
            observer.await.unwrap();
            if iteration == 0 {
                let waiter = tokio::spawn({
                    let guard = guard.clone();
                    async move { guard.admit(false).await }
                });
                guard.finish_global(old_id, Some(0.0), old_at);
                // Expiring the new provisional estimate must not release its body.
                tokio::time::sleep(Duration::from_millis(260)).await;
                assert!(!waiter.is_finished());
                drop(accounting);
                tokio::time::timeout(Duration::from_secs(1), waiter)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(guard.snapshot().pending_global_responses, 0);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn missing_global_timing_fails_closed() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        guard.observe_global(None);
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            INVALID_REQUEST_WINDOW
        );
        guard.observe_global(Some(0.0));
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            INVALID_REQUEST_WINDOW
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_and_route_debug_renders_counts_only() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        guard.observe_bucket(
            "GET /guilds/1/roles",
            &http::HeaderMap::from_iter([
                (
                    http::header::HeaderName::from_static("x-ratelimit-bucket"),
                    http::HeaderValue::from_static("bucket-fixture"),
                ),
                (
                    http::header::HeaderName::from_static("x-ratelimit-remaining"),
                    http::HeaderValue::from_static("0"),
                ),
                (
                    http::header::HeaderName::from_static("x-ratelimit-reset-after"),
                    http::HeaderValue::from_static("200"),
                ),
            ]),
        );
        let rendered = format!("{guard:?}");
        assert!(
            rendered.contains("BucketTable"),
            "guard debug must name the bucket table: {rendered}"
        );
        assert!(
            rendered.contains("RouteTable"),
            "guard debug must name the route table: {rendered}"
        );
        assert!(
            !rendered.contains("bucket-fixture"),
            "debug must not leak bucket ids: {rendered}"
        );
        assert!(
            !rendered.contains("/guilds/1/roles"),
            "debug must not leak routes: {rendered}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn remaining_budget_clears_a_live_bucket_deadline_and_prunes_orphans() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        let bucketed = |remaining: &str| {
            http::HeaderMap::from_iter([
                (
                    http::header::HeaderName::from_static("x-ratelimit-bucket"),
                    http::HeaderValue::from_static("bucket-live"),
                ),
                (
                    http::header::HeaderName::from_static("x-ratelimit-remaining"),
                    http::HeaderValue::from_str(remaining).unwrap(),
                ),
                (
                    http::header::HeaderName::from_static("x-ratelimit-reset-after"),
                    http::HeaderValue::from_static("200"),
                ),
            ])
        };
        guard.observe_bucket("GET /guilds/1/roles", &bucketed("0"));
        assert!(
            guard.check_route_now("GET /guilds/1/roles", false).is_err(),
            "an exhausted bucket must gate its route"
        );
        guard.observe_bucket("GET /guilds/1/roles", &bucketed("5"));
        assert!(
            guard.check_route_now("GET /guilds/1/roles", false).is_ok(),
            "remaining budget must clear the learned deadline"
        );
        // A second route learns the same bucket, then outlives it: the next
        // admission prunes the expired bucket and its orphaned mapping, so a
        // later response relearns the route instead of riding stale state.
        guard.observe_bucket("GET /guilds/2/roles", &bucketed("0"));
        tokio::time::advance(Duration::from_secs(300)).await;
        guard
            .admit_route("GET /guilds/2/roles", false)
            .await
            .unwrap();
        guard.observe_bucket("GET /guilds/2/roles", &bucketed("0"));
        assert!(
            guard.check_route_now("GET /guilds/2/roles", false).is_err(),
            "a repruned route must gate again after relearning"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shared_bucket_hash_scopes_deadlines_per_top_level_resource() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        let headers = |bucket: &str, remaining: &str, reset_after: Option<&str>| {
            let mut headers = http::HeaderMap::from_iter([
                (
                    http::header::HeaderName::from_static("x-ratelimit-bucket"),
                    http::HeaderValue::from_str(bucket).unwrap(),
                ),
                (
                    http::header::HeaderName::from_static("x-ratelimit-remaining"),
                    http::HeaderValue::from_str(remaining).unwrap(),
                ),
            ]);
            if let Some(reset_after) = reset_after {
                headers.insert(
                    "x-ratelimit-reset-after",
                    http::HeaderValue::from_str(reset_after).unwrap(),
                );
            }
            headers
        };
        // One channel exhausts the shared hash: only that channel is gated.
        guard.observe_bucket(
            "GET channels/111/messages",
            &headers("bucket-shared", "0", Some("5")),
        );
        assert!(
            guard
                .check_route_now("GET channels/111/messages", false)
                .is_err(),
            "the exhausted channel must wait out its reset"
        );
        assert!(
            guard
                .check_route_now("GET channels/222/messages", false)
                .is_ok(),
            "one exhausted channel must not stall another sharing the hash"
        );
        // The other channel's remaining budget must not delete the live
        // deadline: the next channel-111 read would 429 and spend budget.
        guard.observe_bucket(
            "GET channels/222/messages",
            &headers("bucket-shared", "4", Some("5")),
        );
        assert!(
            guard
                .check_route_now("GET channels/111/messages", false)
                .is_err(),
            "a sibling channel's remaining budget cleared a live deadline"
        );
        // A late, out-of-order same-route reply with an older reset must not
        // clear it either.
        tokio::time::advance(Duration::from_secs(1)).await;
        guard.observe_bucket(
            "GET channels/111/messages",
            &headers("bucket-shared", "1", Some("0.5")),
        );
        assert!(
            guard
                .check_route_now("GET channels/111/messages", false)
                .is_err(),
            "an out-of-order reply with an older reset cleared a live deadline"
        );
        // A response with remaining budget but no reset timing carries
        // nothing to order against: the live deadline stands.
        guard.observe_bucket(
            "GET channels/111/messages",
            &headers("bucket-shared", "2", None),
        );
        assert!(
            guard
                .check_route_now("GET channels/111/messages", false)
                .is_err(),
            "an untimed reply cleared a live deadline"
        );
        // A same-route reply whose reset postdates the stored one retires it.
        guard.observe_bucket(
            "GET channels/111/messages",
            &headers("bucket-shared", "3", Some("10")),
        );
        assert!(
            guard
                .check_route_now("GET channels/111/messages", false)
                .is_ok(),
            "a newer reset with remaining budget must retire the deadline"
        );
        // Webhook tokens scope separately under one hash, and unrecognized
        // shapes fall back to the full path.
        guard.observe_bucket(
            "POST webhooks/1/token-a",
            &headers("bucket-webhook", "0", Some("5")),
        );
        assert!(
            guard
                .check_route_now("POST webhooks/1/token-b", false)
                .is_ok(),
            "one webhook's exhaustion must not stall another sharing the hash"
        );
        guard.observe_bucket("GET webhooks/7", &headers("bucket-webhook", "0", Some("5")));
        assert!(
            guard.check_route_now("GET webhooks/7", false).is_err(),
            "a token-less webhook route must still gate its own scope"
        );
        guard.observe_bucket("GET guilds", &headers("bucket-bare", "0", Some("5")));
        assert!(
            guard.check_route_now("GET guilds", false).is_err(),
            "a bare resource route must still gate its own scope"
        );
        guard.observe_bucket(
            "GET interactions/1/abc/callback",
            &headers("bucket-other", "0", Some("5")),
        );
        assert!(
            guard
                .check_route_now("GET interactions/2/def/callback", false)
                .is_ok(),
            "unrecognized shapes must not share a deadline across paths"
        );
        assert!(
            guard
                .check_route_now("GET interactions/1/abc/callback", false)
                .is_err(),
            "an unrecognized shape must still gate its own path"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn absolute_reset_epoch_installs_a_bucket_deadline() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        let epoch = std::time::SystemTime::now() + Duration::from_secs(100);
        let reset = epoch
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            .to_string();
        guard.observe_bucket(
            "GET /guilds/3/roles",
            &http::HeaderMap::from_iter([
                (
                    http::header::HeaderName::from_static("x-ratelimit-bucket"),
                    http::HeaderValue::from_static("bucket-epoch"),
                ),
                (
                    http::header::HeaderName::from_static("x-ratelimit-remaining"),
                    http::HeaderValue::from_static("0"),
                ),
                (
                    http::header::HeaderName::from_static("x-ratelimit-reset"),
                    http::HeaderValue::from_str(&reset).unwrap(),
                ),
            ]),
        );
        assert!(
            guard.check_route_now("GET /guilds/3/roles", false).is_err(),
            "an absolute reset must gate its route"
        );
        tokio::time::advance(Duration::from_secs(101)).await;
        assert!(
            guard.check_route_now("GET /guilds/3/roles", false).is_ok(),
            "an expired absolute reset must release its route"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn body_shared_scope_retracts_the_header_tally() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        let mut accounting = ResponseAccounting::new(&guard, 429, &http::HeaderMap::new(), true);
        assert_eq!(
            guard.snapshot().invalid_requests_total,
            1,
            "a header tally without scope spends budget"
        );
        let shared = accounting.finish(&crate::executor::RawResponse {
            status: 429,
            retry_after_header: None,
            body: br#"{"scope":"shared"}"#.to_vec(),
            completion: None,
        });
        assert!(!shared, "a shared scope is not a global pause");
        assert_eq!(
            guard.snapshot().invalid_requests_total,
            0,
            "a body-shared scope must retract the tally"
        );
        assert_eq!(
            guard.snapshot().invalid_requests_in_window,
            0,
            "a body-shared scope must leave the window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn body_global_scope_installs_a_pause_from_body_timing() {
        let guard = RateLimitGuard::new(GuardConfig::default()).unwrap();
        let mut accounting = ResponseAccounting::new(&guard, 429, &http::HeaderMap::new(), true);
        let global = accounting.finish(&crate::executor::RawResponse {
            status: 429,
            retry_after_header: None,
            body: br#"{"global":true,"retry_after":0.5}"#.to_vec(),
            completion: None,
        });
        assert!(global, "a body global flag must govern the retry");
        assert_eq!(
            guard.snapshot().global_pause_remaining,
            Duration::from_millis(750),
            "body timing plus the 250 ms pad"
        );
        assert_eq!(
            guard.snapshot().global_pauses_total,
            1,
            "one body-learned pause"
        );
    }
}
