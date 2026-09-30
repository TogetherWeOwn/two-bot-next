//! Process-wide Discord invalid-request budget and global cooldown.
//!
//! The mutex covers bookkeeping only, never network I/O or a sleep. Already
//! in-flight requests may finish after a breaker opens; every new attempt
//! checks admission again. Clones and independently constructed executors
//! share [`process_guard`], including callers using an API proxy.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::time::Instant;

pub const INVALID_REQUEST_WINDOW: Duration = Duration::from_secs(600);
pub const DEFAULT_INVALID_REQUEST_THRESHOLD: usize = 5_000;

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
}

#[derive(Debug, Default)]
struct State {
    invalid: VecDeque<Instant>,
    global_until: Option<Instant>,
    counters: GuardSnapshot,
}

#[derive(Debug)]
pub struct RateLimitGuard {
    config: GuardConfig,
    state: Mutex<State>,
}

impl RateLimitGuard {
    pub fn new(config: GuardConfig) -> Result<Self, String> {
        if config.invalid_request_threshold == 0 || config.window.is_zero() {
            return Err("Discord guard threshold and window must be positive".to_owned());
        }
        Ok(Self {
            config,
            state: Mutex::new(State::default()),
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
    }

    /// Only interaction acknowledgements are essential at the executor seam.
    /// They may bypass the invalid budget, never global pauses or token_invalid.
    pub async fn admit(&self, essential: bool) -> Result<(), GuardError> {
        loop {
            let until = {
                let mut state = self.state.lock().expect("Discord guard state");
                self.refresh(&mut state, Instant::now());
                let error = if state.counters.token_invalid {
                    Some(GuardError::TokenInvalid)
                } else if state.counters.breaker_open && !essential {
                    Some(GuardError::CircuitOpen)
                } else {
                    None
                };
                if let Some(error) = error {
                    state.counters.rejected_requests_total += 1;
                    return Err(error);
                }
                state.global_until
            };
            match until {
                // A shared deadline, not one additive sleep per waiter. Recheck
                // on waking because another in-flight response may extend it.
                Some(until) => tokio::time::sleep_until(until).await,
                None => return Ok(()),
            }
        }
    }

    /// Account as soon as headers arrive, even if the response body fails or
    /// the caller cancels while reading it. Callback-token 401s aren't bot 401s.
    pub fn observe_status(&self, status: u16, bot_authenticated: bool) {
        if !matches!(status, 401 | 403 | 429) {
            return;
        }
        let mut state = self.state.lock().expect("Discord guard state");
        let now = Instant::now();
        self.refresh(&mut state, now);
        state.counters.invalid_requests_total += 1;
        state.invalid.push_back(now);
        if status == 401 && bot_authenticated && !state.counters.token_invalid {
            state.counters.token_invalid = true;
            tracing::error!(
                event = "discord_token_invalid",
                "Discord refused the bot token; REST disabled until restart"
            );
        }
        self.refresh(&mut state, now);
    }

    /// Extend (never shorten) a global pause. Missing/invalid timing fails
    /// closed for a full budget window, rather than repeatedly hitting 429.
    pub fn observe_global(&self, retry_after_secs: Option<f64>) {
        let wait = retry_after_secs
            .filter(|secs| secs.is_finite() && *secs >= 0.0)
            .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
            .and_then(|wait| wait.checked_add(Duration::from_millis(250)))
            .unwrap_or(self.config.window);
        let now = Instant::now();
        let until = now.checked_add(wait).unwrap_or(now + self.config.window);
        let mut state = self.state.lock().expect("Discord guard state");
        self.refresh(&mut state, now);
        if state.global_until.is_none() {
            state.counters.global_pauses_total += 1;
            tracing::warn!(
                event = "discord_global_pause",
                wait_ms = wait.as_millis() as u64,
                "Discord global REST pause"
            );
        }
        state.global_until = Some(state.global_until.map_or(until, |old| old.max(until)));
    }

    #[must_use]
    pub fn snapshot(&self) -> GuardSnapshot {
        let mut state = self.state.lock().expect("Discord guard state");
        let now = Instant::now();
        self.refresh(&mut state, now);
        GuardSnapshot {
            invalid_requests_in_window: state.invalid.len(),
            global_pause_remaining: state
                .global_until
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
    global_header: bool,
    retry_after_header: Option<f64>,
    finished: bool,
}

impl<'a> ResponseAccounting<'a> {
    pub(crate) fn new(
        guard: &'a RateLimitGuard,
        status: u16,
        headers: &http::HeaderMap,
        bot_authenticated: bool,
    ) -> Self {
        guard.observe_status(status, bot_authenticated);
        let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
        let global_header = status == 429
            && (header("x-ratelimit-global").is_some_and(|v| v.eq_ignore_ascii_case("true"))
                || header("x-ratelimit-scope").is_some_and(|v| v.eq_ignore_ascii_case("global")));
        let retry_after_header = header("retry-after").and_then(|v| v.parse::<f64>().ok());
        Self {
            guard,
            global_header,
            retry_after_header,
            finished: false,
        }
    }

    pub(crate) fn finish(&mut self, response: &crate::executor::RawResponse) {
        let body: Option<serde_json::Value> = serde_json::from_slice(&response.body).ok();
        let global_body = body.as_ref().is_some_and(|value| {
            value.get("global").and_then(|v| v.as_bool()) == Some(true)
                || value.get("scope").and_then(|v| v.as_str()) == Some("global")
        });
        if response.status == 429 && (self.global_header || global_body) {
            let timing = response
                .body_retry_after_secs()
                .filter(|secs| *secs >= 0.0)
                .or(self.retry_after_header);
            self.guard.observe_global(timing);
        }
        self.finished = true;
    }
}

impl Drop for ResponseAccounting<'_> {
    fn drop(&mut self) {
        if !self.finished && self.global_header {
            self.guard.observe_global(self.retry_after_header);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
