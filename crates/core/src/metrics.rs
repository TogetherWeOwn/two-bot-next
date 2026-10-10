//! Fixed-cardinality, process-local metrics; no background task or retained payloads.
//! Text format: <https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details>

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
    "MESSAGE_REACTION_ADD",
    "MESSAGE_REACTION_REMOVE",
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
    "POST /guilds/:guild/channels",
    "DELETE /channels/:channel",
    "other",
];
const RESULTS: &[&str] = &["2xx", "3xx", "4xx", "429", "5xx", "transport"];
pub const JOBS: &[&str] = &[
    "invite_snapshot",
    "session_checkpoint",
    "counter",
    "rank",
    "scheduled_events",
    "settings",
    "presence_probe",
    "community_scorecard",
    "inactivity",
    "audit_retry",
    "scheduled_messages",
    "other",
];
const JOB_OUTCOMES: &[&str] = &["success", "failure"];
/// Room lifecycle operations (TOG-13543): creator-channel create/move/delete
/// outcomes only. Retries (429/backoff) are not outcomes.
pub const VOICE_OPERATIONS: &[&str] = &["create", "move", "delete"];
const VOICE_OUTCOMES: &[&str] = &[
    "success",
    "category_full",
    "discord",
    "persistence",
    "cancelled",
];
/// Reconcile plan actions (TOG-13543): what one `reconcile` pass enqueued.
pub const VOICE_RECONCILE_ACTIONS: &[&str] = &[
    "delete_enqueued",
    "suspended",
    "resumed",
    "succession_enqueued",
];
/// Dead-lettered queue actions by bounded family (TOG-13543). Companion
/// grants/revokes share `companion`; all unknown shapes share `other`.
pub const VOICE_DEAD_ACTIONS: &[&str] = &[
    "create",
    "move",
    "delete",
    "companion",
    "ownership",
    "kick",
    "rename",
    "limit",
    "other",
];
/// Storage-failure operations: `admission` is send-admission SQL
/// (admit/extend/complete). Every other store reports `other` until its op
/// joins this allowlist.
pub const DB_ERROR_OPS: &[&str] = &["admission", "other"];
/// Terminal outcomes of one send-admission `admit()` decision:
/// `admitted` on Ok, `blocked` on lane/cooldown refusal, `storage_error`
/// when the admission SQL itself failed (also counted in
/// `two_bot_db_errors_total{op="admission"}`); anything else collapses to
/// `other`. Failed `complete()`/`extend()` storage writes count only in
/// db_errors: the admit decision was already recorded.
pub const SEND_ADMISSION_OUTCOMES: &[&str] = &["admitted", "blocked", "storage_error", "other"];
/// Dispatch-lane names for `two_bot_dispatch_drops_total{lane}`, in the bot's
/// `DISPATCH_LIMITS` order (messages, interactions, registry, privileged,
/// busy, reactions). Length must equal the lane count; unknown names collapse
/// to the trailing `reactions` slot only when the allowlist grows, never to a
/// dynamic label.
///
/// Saturation-drop policy (TOG-19878): the counter increments on **every**
/// saturation drop in `spawn_first`, including the busy-lane path. Logs emit
/// at most one `warn!` per 60 s per runtime (the first drop in the window);
/// a burst of N drops is therefore O(1) log lines with N counter
/// increments. Alert-threshold hook for M2.1: alert when any lane's drop rate
/// is sustained above zero across consecutive scrapes (exact rule lands with
/// M2.1 once TOG-18943 unblocks); a single drop during a burst is not paging.
pub const DISPATCH_LANES: &[&str] = &[
    "messages",
    "interactions",
    "registry",
    "privileged",
    "busy",
    "reactions",
];
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
struct JobMetrics {
    runs: [u64; JOB_OUTCOMES.len()],
    last_success: u64,
    consecutive_failures: u64,
}

#[derive(Default)]
struct Values {
    events: [u64; EVENTS.len()],
    rest: [[u64; RESULTS.len()]; REST_ROUTES.len()],
    jobs: [JobMetrics; JOBS.len()],
    reconnects: u64,
    resumes: u64,
    disconnects: u64,
    missed_events: u64,
    latency_micros: Option<u64>,
    handler: Histogram,
    voice_ops: [[u64; 5]; 3],
    voice_reconcile: [u64; 4],
    voice_dead: [u64; 9],
    voice_tracked: u64,
    voice_compensation: u64,
    voice_orphans: u64,
    db_errors: [u64; DB_ERROR_OPS.len()],
    send_admissions: [u64; SEND_ADMISSION_OUTCOMES.len()],
    dispatch_drops: [u64; DISPATCH_LANES.len()],
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

    /// One observed transport loss: reconnect failure, close frame, invalid
    /// session, or stream termination. The 48h watch pairs every disconnect
    /// with the matching RESUME or fresh READY; an unpaired disconnect means
    /// the gateway never came back.
    pub fn gateway_disconnect(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.disconnects = values.disconnects.saturating_add(1);
    }

    /// Dispatches Discord assigned but this process never received (sequence
    /// gaps inside one session). Zero is a no-op. Any nonzero increase over
    /// the watch window fails the zero-missed-events acceptance.
    pub fn gateway_missed_events(&self, count: u64) {
        if count == 0 {
            return;
        }
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.missed_events = values.missed_events.saturating_add(count);
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
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = &mut values.jobs[bounded_index(job, JOBS)];
        current.runs[0] = current.runs[0].saturating_add(1);
        current.last_success = unix_seconds;
        current.consecutive_failures = 0;
    }

    /// A completed failed attempt, including a timeout or isolated panic.
    pub fn job_failure(&self, job: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = &mut values.jobs[bounded_index(job, JOBS)];
        current.runs[1] = current.runs[1].saturating_add(1);
        current.consecutive_failures = current.consecutive_failures.saturating_add(1);
    }

    /// One finished room lifecycle outcome (TOG-13543). Call once per
    /// terminal create/move/delete: retries and rate-limit backoffs are not
    /// outcomes. Unknown `op`/`outcome` collapse to the trailing `other`
    /// slot only when the allowlists grow; today every caller passes a
    /// member, so `other` stays zero. No IDs, tokens or bodies are retained.
    pub fn voice_operation(&self, op: &str, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_ops[bounded_index(op, VOICE_OPERATIONS)]
            [bounded_index(outcome, VOICE_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// Add one reconcile plan size (TOG-13543). `count` is the number of
    /// rooms in this pass for `action`; zero is a no-op.
    pub fn voice_reconcile(&self, action: &str, count: u64) {
        if count == 0 {
            return;
        }
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_reconcile[bounded_index(action, VOICE_RECONCILE_ACTIONS)];
        *counter = counter.saturating_add(count);
    }

    /// One queue write exhausted `QUEUE_MAX_ATTEMPTS` (TOG-13543). Unknown
    /// families collapse to `other`.
    pub fn voice_dead_letter(&self, action: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_dead[bounded_index(action, VOICE_DEAD_ACTIONS)];
        *counter = counter.saturating_add(1);
    }

    /// Current worker state for ghost verification (TOG-13543): tracked
    /// rooms plus compensation-pending orphans awaiting delete. Last writer
    /// wins; on staging (single guild) this is exact, on multi-guild it is
    /// the freshest reporter until runtimes aggregate.
    pub fn voice_state(&self, tracked: u64, compensation: u64) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_tracked = tracked;
        values.voice_compensation = compensation;
    }

    /// One untracked creator-channel orphan needing manual deletion after
    /// failed `/create` compensation (TOG-13543). No channel ID is retained.
    pub fn voice_orphan(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_orphans = values.voice_orphans.saturating_add(1);
    }

    /// One storage-layer failure. Unknown `op` values collapse
    /// to `other`; no error text, query or identifier is retained.
    pub fn db_error(&self, op: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.db_errors[bounded_index(op, DB_ERROR_OPS)];
        *counter = counter.saturating_add(1);
    }

    /// One send-admission `admit()` terminal outcome. Call once
    /// per admit decision, not per retry or per completion. Unknown outcomes
    /// collapse to `other`.
    pub fn send_admission(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.send_admissions[bounded_index(outcome, SEND_ADMISSION_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One dispatch-lane saturation drop (TOG-19878). Call once per saturated
    /// lane per dropped event from `spawn_first`: a single-lane drop increments
    /// that lane; a multi-lane attempt whose every lane is full increments each
    /// attempted lane. Unknown lanes collapse to `other` only when the
    /// allowlist grows; today every caller passes a member, so the trailing
    /// slot stays zero. No IDs, tokens or bodies are retained.
    pub fn dispatch_drop(&self, lane: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.dispatch_drops[bounded_index(lane, DISPATCH_LANES)];
        *counter = counter.saturating_add(1);
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
        scalar(
            &mut out,
            "two_bot_gateway_disconnects_total",
            "counter",
            values.disconnects,
        );
        scalar(
            &mut out,
            "two_bot_gateway_missed_events_total",
            "counter",
            values.missed_events,
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
            "two_bot_job_runs_total",
            "counter",
            "Completed job attempts by bounded job name and outcome.",
        );
        for (job, current) in JOBS.iter().zip(&values.jobs) {
            for (outcome, count) in JOB_OUTCOMES.iter().zip(current.runs) {
                writeln!(
                    out,
                    "two_bot_job_runs_total{{job=\"{job}\",outcome=\"{outcome}\"}} {count}"
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
        for (job, current) in JOBS.iter().zip(&values.jobs) {
            writeln!(
                out,
                "two_bot_job_last_success_timestamp_seconds{{job=\"{job}\"}} {}",
                current.last_success
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_job_consecutive_failures",
            "gauge",
            "Completed failures since the last success; resets on success.",
        );
        for (job, current) in JOBS.iter().zip(&values.jobs) {
            writeln!(
                out,
                "two_bot_job_consecutive_failures{{job=\"{job}\"}} {}",
                current.consecutive_failures
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_voice_operations_total",
            "counter",
            "Finished room lifecycle outcomes by bounded operation and outcome; retries are not outcomes.",
        );
        for (op, outcomes) in VOICE_OPERATIONS.iter().zip(values.voice_ops) {
            for (outcome, count) in VOICE_OUTCOMES.iter().zip(outcomes) {
                writeln!(
                    out,
                    "two_bot_voice_operations_total{{op=\"{op}\",outcome=\"{outcome}\"}} {count}"
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_voice_reconcile_actions_total",
            "counter",
            "Reconcile plan sizes by bounded action.",
        );
        for (action, count) in VOICE_RECONCILE_ACTIONS.iter().zip(values.voice_reconcile) {
            writeln!(
                out,
                "two_bot_voice_reconcile_actions_total{{action=\"{action}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_voice_dead_letters_total",
            "counter",
            "Queue writes that exhausted QUEUE_MAX_ATTEMPTS by bounded family.",
        );
        for (action, count) in VOICE_DEAD_ACTIONS.iter().zip(values.voice_dead) {
            writeln!(
                out,
                "two_bot_voice_dead_letters_total{{action=\"{action}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_voice_tracked_rooms",
            "gauge",
            "Rooms tracked in memory; compare with live Discord channels for ghosts.",
        );
        writeln!(out, "two_bot_voice_tracked_rooms {}", values.voice_tracked).unwrap();
        header(
            &mut out,
            "two_bot_voice_compensation_pending",
            "gauge",
            "Tracked rooms awaiting compensating delete after a failed write.",
        );
        writeln!(
            out,
            "two_bot_voice_compensation_pending {}",
            values.voice_compensation
        )
        .unwrap();
        scalar(
            &mut out,
            "two_bot_voice_orphans_total",
            "counter",
            values.voice_orphans,
        );
        header(
            &mut out,
            "two_bot_db_errors_total",
            "counter",
            "Storage-layer failures by bounded operation; pool gauges are pressure, this is errors.",
        );
        for (op, count) in DB_ERROR_OPS.iter().zip(values.db_errors) {
            writeln!(out, "two_bot_db_errors_total{{op=\"{op}\"}} {count}").unwrap();
        }
        header(
            &mut out,
            "two_bot_send_admissions_total",
            "counter",
            "Send-admission admit() decisions by bounded terminal outcome.",
        );
        for (outcome, count) in SEND_ADMISSION_OUTCOMES.iter().zip(values.send_admissions) {
            writeln!(
                out,
                "two_bot_send_admissions_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_dispatch_drops_total",
            "counter",
            "Dispatch-lane saturation drops by bounded lane (reactions lane also counts per-member fairness refusals); logs sample one warn per 60 s per runtime.",
        );
        for (lane, count) in DISPATCH_LANES.iter().zip(values.dispatch_drops) {
            writeln!(
                out,
                "two_bot_dispatch_drops_total{{lane=\"{lane}\"}} {count}"
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
            metrics.job_failure(&hostile);
            metrics.db_error(&hostile);
            metrics.send_admission(&hostile);
            metrics.dispatch_drop(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        assert!(!text.contains("secret"));
        assert!(text.contains("two_bot_gateway_events_total{event=\"other\"} 10000\n"));
        assert!(
            text.contains("two_bot_rest_requests_total{route=\"other\",result=\"429\"} 10000\n")
        );
        assert!(text.contains("two_bot_db_pool_configured 0\n"));
        for outcome in JOB_OUTCOMES {
            assert!(text.contains(&format!(
                "two_bot_job_runs_total{{job=\"other\",outcome=\"{outcome}\"}} 10000\n"
            )));
        }
        assert!(text.contains("two_bot_job_last_success_timestamp_seconds{job=\"other\"} 123\n"));
        assert!(text.contains("two_bot_job_consecutive_failures{job=\"other\"} 1\n"));
    }

    #[test]
    fn job_counters_saturate_and_success_resets_failures() {
        let metrics = Metrics::default();
        {
            let mut values = metrics.0.lock().unwrap();
            let current = &mut values.jobs[bounded_index("rank", JOBS)];
            current.runs = [u64::MAX; JOB_OUTCOMES.len()];
            current.consecutive_failures = u64::MAX;
        }
        metrics.job_failure("rank");
        assert!(metrics.render(None).contains(&format!(
            "two_bot_job_consecutive_failures{{job=\"rank\"}} {}\n",
            u64::MAX
        )));
        metrics.job_success("rank", 456);
        let text = metrics.render(None);
        for outcome in JOB_OUTCOMES {
            assert!(text.contains(&format!(
                "two_bot_job_runs_total{{job=\"rank\",outcome=\"{outcome}\"}} {}\n",
                u64::MAX
            )));
        }
        assert!(text.contains("two_bot_job_last_success_timestamp_seconds{job=\"rank\"} 456\n"));
        assert!(text.contains("two_bot_job_consecutive_failures{job=\"rank\"} 0\n"));
    }

    #[test]
    fn voice_signals_stay_bounded_and_saturate() {
        let metrics = Metrics::default();
        metrics.voice_operation("create", "success");
        metrics.voice_operation("move", "discord");
        metrics.voice_operation("delete", "persistence");
        metrics.voice_operation("create", "category_full");
        metrics.voice_operation("move", "cancelled");
        metrics.voice_reconcile("delete_enqueued", 2);
        metrics.voice_reconcile("suspended", 1);
        metrics.voice_reconcile("resumed", 0);
        metrics.voice_dead_letter("create");
        metrics.voice_dead_letter("other");
        metrics.voice_state(7, 2);
        metrics.voice_orphan();
        let text = metrics.render(None);
        assert!(
            text.contains("two_bot_voice_operations_total{op=\"create\",outcome=\"success\"} 1\n")
        );
        assert!(
            text.contains("two_bot_voice_operations_total{op=\"move\",outcome=\"discord\"} 1\n")
        );
        assert!(
            text.contains("two_bot_voice_reconcile_actions_total{action=\"delete_enqueued\"} 2\n")
        );
        assert!(text.contains("two_bot_voice_dead_letters_total{action=\"create\"} 1\n"));
        assert!(text.contains("two_bot_voice_tracked_rooms 7\n"));
        assert!(text.contains("two_bot_voice_compensation_pending 2\n"));
        assert!(text.contains("two_bot_voice_orphans_total 1\n"));
        // Fixed cardinality: 3x5 ops + 4 reconcile + 9 dead-letters.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn voice_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.voice_operation(&hostile, &hostile);
            metrics.voice_reconcile(&hostile, 1);
            metrics.voice_dead_letter(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        assert!(!text.contains("secret"));
    }

    #[test]
    fn db_and_admission_signals_stay_bounded_and_saturate() {
        let metrics = Metrics::default();
        metrics.db_error("admission");
        metrics.db_error("admission");
        metrics.send_admission("admitted");
        metrics.send_admission("blocked");
        metrics.send_admission("storage_error");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_db_errors_total{op=\"admission\"} 2\n"));
        assert!(text.contains("two_bot_db_errors_total{op=\"other\"} 0\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"admitted\"} 1\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"blocked\"} 1\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"storage_error\"} 1\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"other\"} 0\n"));
        // Fixed cardinality: 2 db-error ops + 4 admission outcomes.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn dispatch_drops_stay_bounded_and_saturate() {
        let metrics = Metrics::default();
        metrics.dispatch_drop("messages");
        metrics.dispatch_drop("messages");
        metrics.dispatch_drop("busy");
        metrics.dispatch_drop("reactions");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"messages\"} 2\n"));
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"busy\"} 1\n"));
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"reactions\"} 1\n"));
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"interactions\"} 0\n"));
        // Fixed cardinality: six lanes.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn dispatch_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.dispatch_drop(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        assert!(!text.contains("secret"));
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

    #[test]
    fn disconnects_and_missed_events_start_at_zero_and_saturate() {
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_disconnects_total 0\n"));
        assert!(text.contains("two_bot_gateway_missed_events_total 0\n"));
        metrics.gateway_disconnect();
        metrics.gateway_missed_events(0);
        metrics.gateway_missed_events(3);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_disconnects_total 1\n"));
        assert!(text.contains("two_bot_gateway_missed_events_total 3\n"));
        {
            let mut values = metrics.0.lock().unwrap();
            values.disconnects = u64::MAX;
            values.missed_events = u64::MAX;
        }
        metrics.gateway_disconnect();
        metrics.gateway_missed_events(9);
        let text = metrics.render(None);
        assert!(text.contains(&format!("two_bot_gateway_disconnects_total {}\n", u64::MAX)));
        assert!(text.contains(&format!(
            "two_bot_gateway_missed_events_total {}\n",
            u64::MAX
        )));
    }
}
