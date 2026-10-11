//! Process shutdown policy: the total drain deadline and the second-signal exit.
//!
//! Drain order itself lives in `main::supervise_gateway_bounded` and the gateway
//! dispatcher; this module only owns the operator-facing knobs around it.

use std::time::Duration;

/// Operator override for the total drain deadline, in whole seconds.
pub(crate) const TIMEOUT_ENV: &str = "SHUTDOWN_TIMEOUT_SECONDS";
/// Cloudflare Containers allow a 15 minute grace period before SIGKILL.
const MAX_SECONDS: u64 = 900;

/// After a startup failure, `/readyz` keeps serving (with `gateway_failure`)
/// this long before the process drains and exits: long enough for the rollout
/// gate's 5 s poll to land in it, far below the Container's
/// SIGTERM-to-SIGKILL grace period. A shutdown signal cuts it short.
pub(crate) const FAILURE_LINGER: Duration = Duration::from_secs(15);

/// After a running-gateway failure (`GatewayRuntimeFailed`, the class a
/// worker-stopping checkpoint failure maps to), `/readyz` keeps serving this
/// long instead: longer than one 60 s keepalive tick plus two 6 s probe
/// timeouts (see `wrangler/wrangler.toml` `KEEPALIVE_SECONDS` and the
/// `AbortSignal.timeout(6000)` keepalive probes in `wrangler/src/index.ts`),
/// so at least one keepalive scrape sees the incremented
/// `two_bot_gateway_checkpoint_failures_total` counter before the restart
/// resets it — still far below the grace period. Only the runtime class gets
/// the long linger; every startup class keeps `FAILURE_LINGER` so failed
/// starts restart without added delay.
pub(crate) const RUNTIME_FAILURE_LINGER: Duration = Duration::from_secs(75);

/// Linger for a gateway failure class: the long linger only for a failure of
/// the running gateway, the short linger for every startup failure.
pub(crate) fn failure_linger(class: crate::gateway_failure::FailureClass) -> Duration {
    match class {
        crate::gateway_failure::FailureClass::GatewayRuntimeFailed => RUNTIME_FAILURE_LINGER,
        _ => FAILURE_LINGER,
    }
}

/// Accepted dispatches get `DISPATCH_DRAIN_MAX` to commit their checkpoint; the
/// extra margin covers HTTP and job cleanup.
pub(crate) fn default_deadline() -> Duration {
    crate::dispatch::DISPATCH_DRAIN_MAX + Duration::from_secs(5)
}

/// Parse the override; an unset, empty or invalid value keeps the default.
pub(crate) fn deadline_from(raw: Option<&str>) -> Duration {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return default_deadline();
    };
    match raw.parse::<u64>() {
        Ok(seconds) if (1..=MAX_SECONDS).contains(&seconds) => Duration::from_secs(seconds),
        _ => {
            tracing::warn!(
                env = TIMEOUT_ENV,
                max_seconds = MAX_SECONDS,
                "invalid shutdown timeout; using default"
            );
            default_deadline()
        }
    }
}

pub(crate) fn deadline() -> Duration {
    deadline_from(std::env::var(TIMEOUT_ENV).ok().as_deref())
}

/// After drain starts, a further SIGTERM/SIGINT abandons it and exits at once.
pub(crate) fn exit_on_second_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return;
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
        tracing::warn!("second shutdown signal received; exiting immediately");
        std::process::exit(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_defaults_and_rejects_bad_values() {
        let default = default_deadline();
        for raw in [
            None,
            Some(""),
            Some("  "),
            Some("0"),
            Some("-1"),
            Some("abc"),
            Some("901"),
        ] {
            assert_eq!(deadline_from(raw), default, "{raw:?}");
        }
    }

    #[test]
    fn deadline_accepts_whole_seconds_in_range() {
        assert_eq!(deadline_from(Some("10")), Duration::from_secs(10));
        assert_eq!(deadline_from(Some(" 900 ")), Duration::from_secs(900));
        assert_eq!(deadline_from(Some("1")), Duration::from_secs(1));
    }

    /// Max `KEEPALIVE_SECONDS` across the environments in
    /// `wrangler/wrangler.toml`, read from the real file so raising the tick
    /// without raising the runtime linger fails this test instead of silently
    /// re-opening the missed-checkpoint window.
    fn max_keepalive_seconds() -> u64 {
        let toml = include_str!("../../../wrangler/wrangler.toml");
        let mut max = 0;
        for line in toml.lines() {
            let line = line.trim();
            if !line.starts_with("KEEPALIVE_SECONDS") {
                continue;
            }
            let digits: String = line.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(value) = digits.parse::<u64>() {
                max = max.max(value);
            }
        }
        max
    }

    #[test]
    fn runtime_linger_covers_a_keepalive_tick() {
        // A checkpoint failure must stay scrapable for at least one full
        // keepalive tick plus the probe timeout on each side (the
        // `AbortSignal.timeout(6000)` keepalive probes in
        // `wrangler/src/index.ts`), or the `gateway_checkpoint_failures` rule
        // never sees the increase before the restart resets the counter. Keep
        // the bound far below the container grace period.
        const PROBE_TIMEOUT_SECONDS: u64 = 6;
        let keepalive = max_keepalive_seconds();
        assert!(keepalive > 0, "wrangler.toml must define KEEPALIVE_SECONDS");
        assert!(
            RUNTIME_FAILURE_LINGER.as_secs() > keepalive + 2 * PROBE_TIMEOUT_SECONDS,
            "runtime linger {:?} must outlast the {keepalive} s keepalive tick plus two probe timeouts",
            RUNTIME_FAILURE_LINGER
        );
        assert!(RUNTIME_FAILURE_LINGER.as_secs() < MAX_SECONDS);
    }

    #[test]
    fn startup_linger_stays_short() {
        // Failed starts must not pay the runtime linger: each failed start
        // adds its linger without a gateway, against the rollout gate's
        // 300 s verify deadline.
        assert_eq!(FAILURE_LINGER, Duration::from_secs(15));
        assert!(FAILURE_LINGER < RUNTIME_FAILURE_LINGER);
    }

    #[test]
    fn linger_dispatches_on_failure_class() {
        use crate::gateway_failure::FailureClass;
        assert_eq!(
            failure_linger(FailureClass::GatewayRuntimeFailed),
            RUNTIME_FAILURE_LINGER
        );
        for class in [
            FailureClass::StoreUnavailable,
            FailureClass::GatewayPoolConnectFailed,
            FailureClass::CheckpointLoadFailed,
            FailureClass::GatewayTaskPanicked,
        ] {
            assert_eq!(failure_linger(class), FAILURE_LINGER, "{class:?}");
        }
    }
}
