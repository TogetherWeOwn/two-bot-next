//! Linux local-fixture measurement: no Discord, database, or deployment calls.
//! Run on an admitted controller or ephemeral CI: cargo run -p two-bot-core --example metrics_rss

// Local measurement CLI intentionally reports its fixture result to stdout.
#![allow(clippy::print_stdout)]

use std::{hint::black_box, time::Duration};
use two_bot_core::metrics::{
    Metrics, DB_ERROR_OPS, EVENTS, JOBS, REST_ROUTES, SEND_ADMISSION_OUTCOMES,
};

fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("Linux procfs");
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .expect("VmRSS");
    line.split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 1024
}

fn main() {
    let baseline = rss_bytes();
    let metrics = Metrics::default();
    for event in EVENTS {
        metrics.gateway_event(event);
    }
    for route in REST_ROUTES {
        for status in [Some(200), Some(302), Some(403), Some(429), Some(503), None] {
            metrics.rest_response(route, status);
        }
    }
    for job in JOBS {
        metrics.job_success(job, 123);
        metrics.job_failure(job);
    }
    for op in DB_ERROR_OPS {
        metrics.db_error(op);
    }
    for outcome in SEND_ADMISSION_OUTCOMES {
        metrics.send_admission(outcome);
    }
    metrics.gateway_latency(Duration::from_millis(20));
    for id in 0..100_000 {
        let unknown = format!("unbounded-{id}");
        metrics.gateway_event(&unknown);
        metrics.rest_response(&unknown, Some(429));
        metrics.job_success(&unknown, 123);
        metrics.job_failure(&unknown);
        metrics.db_error(&unknown);
        metrics.send_admission(&unknown);
        metrics.handler_duration(Duration::from_millis(id % 1000));
    }
    let mut peak = rss_bytes();
    let mut bytes = 0;
    for id in 0..10_000 {
        let text = metrics.render(Some((5, 2, 5)));
        bytes = text.len();
        black_box(&text);
        if id % 100 == 0 {
            peak = peak.max(rss_bytes());
        }
    }
    let after = rss_bytes();
    peak = peak.max(after);
    let delta = peak.saturating_sub(baseline);
    println!("metrics_rss baseline_bytes={baseline} after_bytes={after} sampled_peak_bytes={peak} delta_bytes={delta} registry_bytes={} exposition_bytes={bytes} unknown_labels=100000 scrapes=10000 lite_budget_bytes=268435456", std::mem::size_of::<Metrics>());
    // This is the instrumentation fixture, not a whole-bot/gateway cache budget proof.
    assert!(
        delta < 4 * 1024 * 1024,
        "metrics fixture exceeded 4 MiB incremental budget"
    );
    assert!(peak < 256 * 1024 * 1024, "fixture exceeded lite RSS budget");
}
