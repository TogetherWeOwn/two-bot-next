//! Process shutdown policy: the total drain deadline and the second-signal exit.
//!
//! Drain order itself lives in `main::supervise_gateway_bounded` and the gateway
//! dispatcher; this module only owns the operator-facing knobs around it.

use std::time::Duration;

/// Operator override for the total drain deadline, in whole seconds.
pub(crate) const TIMEOUT_ENV: &str = "SHUTDOWN_TIMEOUT_SECONDS";
/// Cloudflare Containers allow a 15 minute grace period before SIGKILL.
const MAX_SECONDS: u64 = 900;

/// After the gateway task fails, `/readyz` keeps serving (with `gateway_failure`)
/// this long before the process drains and exits: longer than one 60 s
/// keepalive tick plus the 6 s probe timeout (see `wrangler/wrangler.toml`
/// `KEEPALIVE_SECONDS` and the keepalive probes in `wrangler/src/index.ts`),
/// so at least one keepalive scrape always sees the incremented
/// `two_bot_gateway_checkpoint_failures_total` counter and the `/readyz`
/// failure class before the restart resets the counter — still far below the
/// Container's SIGTERM-to-SIGKILL grace period. A shutdown signal cuts it short.
pub(crate) const FAILURE_LINGER: Duration = Duration::from_secs(75);

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

    #[test]
    fn failure_linger_covers_a_keepalive_tick() {
        // Pinned against `wrangler/wrangler.toml` KEEPALIVE_SECONDS (60) and
        // the 6 s keepalive probe timeout in `wrangler/src/index.ts`: a
        // checkpoint failure must stay scrapable for at least one full
        // keepalive tick, or the `gateway_checkpoint_failures` rule never sees
        // the increase before the restart resets the counter. Keep the bound
        // far below the container grace period.
        const KEEPALIVE_SECONDS: u64 = 60;
        const PROBE_TIMEOUT_SECONDS: u64 = 6;
        assert!(
            FAILURE_LINGER.as_secs() > KEEPALIVE_SECONDS + PROBE_TIMEOUT_SECONDS,
            "linger {:?} must outlast one keepalive tick plus the probe timeout",
            FAILURE_LINGER
        );
        assert!(FAILURE_LINGER.as_secs() < MAX_SECONDS);
    }
}
