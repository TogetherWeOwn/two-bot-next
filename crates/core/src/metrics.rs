//! Fixed-cardinality, process-local metrics; no background task or retained payloads.
//! Text format: https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details

use std::{
    fmt::Write,
    sync::{Mutex, OnceLock},
    time::Duration,
};

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
pub const EVENTS: &[&str] = &[
    "READY",
    "RESUMED",
    "GUILD_CREATE",
    "GUILD_DELETE",
    "GUILD_UPDATE",
    "GUILD_MEMBER_ADD",
    "GUILD_MEMBER_REMOVE",
    "GUILD_MEMBER_UPDATE",
    "MESSAGE_CREATE",
    "MESSAGE_UPDATE",
    "MESSAGE_DELETE",
    "VOICE_STATE_UPDATE",
    "INVITE_CREATE",
    "INVITE_DELETE",
    "INTERACTION_CREATE",
    "HEARTBEAT_ACK",
    "GATEWAY_CLOSE",
    "other",
];
pub const REST_ROUTES: &[&str] = &[
    "GET /channels/:channel",
    "GET /channels/:channel/messages",
    "GET /guilds/:guild",
    "GET /guilds/:guild/members",
    "GET /guilds/:guild/scheduled-events",
    "DELETE /guilds/:guild/bans/:member",
    "DELETE /channels/:channel/permissions/:overwrite",
    "PUT /applications/:application/commands",
    "PUT /applications/:application/guilds/:guild/commands",
    "POST /interactions/:interaction/:token/callback",
    "POST /channels/:channel/messages",
    "DELETE /channels/:channel/messages/:message",
    "DELETE /guilds/:guild/members/:member",
    "PUT /guilds/:guild/bans/:member",
    "PATCH /guilds/:guild/members/:member",
    "PATCH /channels/:channel",
    "PUT /channels/:channel/permissions/:overwrite",
    "POST /channels/:channel/messages/bulk-delete",
    "PUT /guilds/:guild/members/:member/roles/:role",
    "DELETE /guilds/:guild/members/:member/roles/:role",
    "POST /guilds/:guild/scheduled-events",
    "PATCH /guilds/:guild/scheduled-events/:event",
    "DELETE /guilds/:guild/scheduled-events/:event",
    "other",
];
const RESULTS: &[&str] = &["2xx", "3xx", "4xx", "429", "5xx", "transport"];
pub const JOBS: &[&str] = &["invite_snapshot", "session_checkpoint", "other"];
const BUCKETS_MICROS: &[u64] = &[
    1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000,
];

fn bounded_index(value: &str, allowed: &[&str]) -> usize {
    allowed
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(allowed.len() - 1)
}

#[derive(Default)]
struct Histogram {
    buckets: [u64; BUCKETS_MICROS.len()],
    count: u64,
    sum_micros: u64,
}

impl Histogram {
    fn observe(&mut self, duration: Duration) {
        let micros = duration.as_micros().min(u64::MAX as u128) as u64;
        self.count = self.count.saturating_add(1);
        self.sum_micros = self.sum_micros.saturating_add(micros);
        for (bucket, bound) in self.buckets.iter_mut().zip(BUCKETS_MICROS) {
            if micros <= *bound {
                *bucket = bucket.saturating_add(1);
            }
        }
    }

    fn render(&self, out: &mut String, name: &str) {
        header(out, name, "histogram", "Handler elapsed time in seconds.");
        for (bucket, bound) in self.buckets.iter().zip(BUCKETS_MICROS) {
            writeln!(
                out,
                "{name}_bucket{{le=\"{}\"}} {bucket}",
                *bound as f64 / 1_000_000.0
            )
            .unwrap();
        }
        writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {}", self.count).unwrap();
        writeln!(out, "{name}_sum {}", self.sum_micros as f64 / 1_000_000.0).unwrap();
        writeln!(out, "{name}_count {}", self.count).unwrap();
    }
}

#[derive(Default)]
struct Values {
    events: [u64; EVENTS.len()],
    rest: [[u64; RESULTS.len()]; REST_ROUTES.len()],
    jobs: [u64; JOBS.len()],
    reconnects: u64,
    resumes: u64,
    latency_micros: Option<u64>,
    handler: Histogram,
}

/// All storage is fixed-size. Unknown labels collapse to `other`, including hostile input.
#[derive(Default)]
pub struct Metrics(Mutex<Values>);

pub fn global() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::default)
}

impl Metrics {
    pub fn gateway_event(&self, name: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.events[bounded_index(name, EVENTS)];
        *counter = counter.saturating_add(1);
        if name == "RESUMED" {
            values.resumes = values.resumes.saturating_add(1);
        }
    }

    pub fn gateway_reconnect(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.reconnects = values.reconnects.saturating_add(1);
        // A stale ACK must not look like latency on the new connection.
        values.latency_micros = None;
    }

    pub fn gateway_latency(&self, latency: Duration) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .latency_micros = Some(latency.as_micros().min(u64::MAX as u128) as u64);
    }

    pub fn handler_duration(&self, duration: Duration) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .handler
            .observe(duration);
    }

    /// One adapter-visible REST attempt, including executor retries. No URL/body is retained.
    pub fn rest_response(&self, route: &str, status: Option<u16>) {
        let result = match status {
            Some(200..=299) => 0,
            Some(300..=399) => 1,
            Some(429) => 3,
            Some(400..=499) => 2,
            Some(500..=599) => 4,
            _ => 5,
        };
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.rest[bounded_index(route, REST_ROUTES)][result];
        *counter = counter.saturating_add(1);
    }

    /// Call only after successful completion, not when a job starts or is merely scheduled.
    pub fn job_success(&self, job: &str, unix_seconds: u64) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .jobs[bounded_index(job, JOBS)] = unix_seconds;
    }

    /// Pool samples are supplied at scrape time; this function never opens a DB connection.
    pub fn render(&self, pool: Option<(u32, usize, u32)>) -> String {
        let values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = String::with_capacity(16_384);
        header(
            &mut out,
            "two_bot_gateway_events_total",
            "counter",
            "Gateway events by bounded event type.",
        );
        for (name, count) in EVENTS.iter().zip(values.events) {
            writeln!(
                out,
                "two_bot_gateway_events_total{{event=\"{name}\"}} {count}"
            )
            .unwrap();
        }
        scalar(
            &mut out,
            "two_bot_gateway_reconnects_total",
            "counter",
            values.reconnects,
        );
        scalar(
            &mut out,
            "two_bot_gateway_resumes_total",
            "counter",
            values.resumes,
        );
        header(
            &mut out,
            "two_bot_gateway_latency_seconds",
            "gauge",
            "Last heartbeat ACK latency; NaN until measured.",
        );
        match values.latency_micros {
            Some(micros) => writeln!(
                out,
                "two_bot_gateway_latency_seconds {}",
                micros as f64 / 1_000_000.0
            )
            .unwrap(),
            None => writeln!(out, "two_bot_gateway_latency_seconds NaN").unwrap(),
        }
        values
            .handler
            .render(&mut out, "two_bot_handler_duration_seconds");
        header(
            &mut out,
            "two_bot_rest_requests_total",
            "counter",
            "Adapter-visible REST attempts by route template and result, including retries.",
        );
        for (route, results) in REST_ROUTES.iter().zip(values.rest) {
            for (result, count) in RESULTS.iter().zip(results) {
                writeln!(
                    out,
                    "two_bot_rest_requests_total{{route=\"{route}\",result=\"{result}\"}} {count}"
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_job_last_success_timestamp_seconds",
            "gauge",
            "Unix time of last successful job completion; zero means never.",
        );
        for (job, timestamp) in JOBS.iter().zip(values.jobs) {
            writeln!(
                out,
                "two_bot_job_last_success_timestamp_seconds{{job=\"{job}\"}} {timestamp}"
            )
            .unwrap();
        }
        let (size, idle, max) = pool.unwrap_or_default();
        scalar(
            &mut out,
            "two_bot_db_pool_configured",
            "gauge",
            u64::from(pool.is_some()),
        );
        scalar(
            &mut out,
            "two_bot_db_pool_connections",
            "gauge",
            u64::from(size),
        );
        scalar(
            &mut out,
            "two_bot_db_pool_idle_connections",
            "gauge",
            idle as u64,
        );
        scalar(
            &mut out,
            "two_bot_db_pool_max_connections",
            "gauge",
            u64::from(max),
        );
        out
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}").unwrap();
}

fn scalar(out: &mut String, name: &str, kind: &str, value: u64) {
    header(out, name, kind, "Process-local health metric.");
    writeln!(out, "{name} {value}").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_has_cumulative_histogram_and_unique_series() {
        let metrics = Metrics::default();
        metrics.handler_duration(Duration::from_millis(5));
        metrics.handler_duration(Duration::from_secs(9));
        metrics.gateway_event("RESUMED");
        metrics.gateway_latency(Duration::from_millis(12));
        metrics.job_success("invite_snapshot", 123);
        let text = metrics.render(Some((3, 1, 5)));
        assert!(text.ends_with('\n'));
        assert!(text.contains("# TYPE two_bot_handler_duration_seconds histogram\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_bucket{le=\"0.001\"} 0\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_bucket{le=\"0.005\"} 1\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_bucket{le=\"+Inf\"} 2\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_sum 9.005\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_count 2\n"));
        assert!(text.contains("two_bot_gateway_latency_seconds 0.012\n"));
        assert!(text.contains("two_bot_gateway_resumes_total 1\n"));
        assert!(text.contains("two_bot_db_pool_idle_connections 1\n"));
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn untrusted_labels_cannot_grow_or_inject_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..10_000 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.gateway_event(&hostile);
            metrics.rest_response(&hostile, Some(429));
            metrics.job_success(&hostile, 123);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        assert!(!text.contains("secret"));
        assert!(text.contains("two_bot_gateway_events_total{event=\"other\"} 10000\n"));
        assert!(
            text.contains("two_bot_rest_requests_total{route=\"other\",result=\"429\"} 10000\n")
        );
        assert!(text.contains("two_bot_db_pool_configured 0\n"));
    }

    #[test]
    fn rest_errors_and_reconnects_are_distinguished() {
        let metrics = Metrics::default();
        for status in [Some(204), Some(302), Some(403), Some(429), Some(503), None] {
            metrics.rest_response(REST_ROUTES[0], status);
        }
        metrics.gateway_latency(Duration::from_millis(20));
        metrics.gateway_reconnect();
        let text = metrics.render(None);
        for result in RESULTS {
            assert!(text.contains(&format!(
                "route=\"{}\",result=\"{result}\"}} 1\n",
                REST_ROUTES[0]
            )));
        }
        assert!(text.contains("two_bot_gateway_reconnects_total 1\n"));
        assert!(text.contains("two_bot_gateway_latency_seconds NaN\n"));
    }
}
