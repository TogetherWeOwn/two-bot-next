//! Gateway observations are independent of durable dispatch/checkpoint decisions.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use twilight_gateway::{Message, Shard};
use two_bot_core::metrics;

#[derive(Default)]
pub(crate) struct Observer {
    saw_hello: bool,
}

#[derive(serde::Deserialize)]
struct Packet<'a> {
    op: u8,
    #[serde(borrow)]
    t: Option<&'a str>,
}

impl Observer {
    pub(crate) fn observe(&mut self, message: &Message, shard: &Shard) {
        match message {
            Message::Text(text) => self.observe_text(
                text,
                shard.latency().recent().first().copied(),
                metrics::global(),
            ),
            Message::Close(_) => metrics::global().gateway_event("GATEWAY_CLOSE"),
        }
    }

    fn observe_text(&mut self, text: &str, latency: Option<Duration>, metrics: &metrics::Metrics) {
        if let Ok(packet) = serde_json::from_str::<Packet<'_>>(text) {
            match packet.op {
                0 => metrics.gateway_event(packet.t.unwrap_or("other")),
                10 => {
                    if self.saw_hello {
                        metrics.gateway_reconnect();
                    }
                    self.saw_hello = true;
                }
                11 => {
                    metrics.gateway_event("HEARTBEAT_ACK");
                    // Most recent completed heartbeat, not an average or elapsed uptime.
                    // https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Latency.html#method.recent
                    if let Some(latency) = latency {
                        metrics.gateway_latency(latency);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Full dispatch handling (pipeline + durable checkpoint), including failures.
pub(crate) struct DispatchTimer(Instant);

impl DispatchTimer {
    pub(crate) fn start() -> Self {
        Self(Instant::now())
    }

    pub(crate) fn committed(&self) {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        metrics::global().job_success("session_checkpoint", seconds);
    }
}

impl Drop for DispatchTimer {
    fn drop(&mut self) {
        metrics::global().handler_duration(self.0.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_hello_is_not_a_reconnect_and_ack_is_not_zero() {
        let metrics = metrics::Metrics::default();
        let mut observer = Observer::default();
        observer.observe_text(
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#,
            None,
            &metrics,
        );
        observer.observe_text(r#"{"op":11}"#, None, &metrics);
        assert!(metrics
            .render(None)
            .contains("two_bot_gateway_reconnects_total 0\n"));
        assert!(metrics
            .render(None)
            .contains("two_bot_gateway_latency_seconds NaN\n"));
        observer.observe_text(r#"{"op":11}"#, Some(Duration::from_millis(42)), &metrics);
        assert!(metrics
            .render(None)
            .contains("two_bot_gateway_latency_seconds 0.042\n"));
        observer.observe_text(r#"{"op":10}"#, None, &metrics);
        observer.observe_text(r#"{"op":0,"t":"RESUMED","s":7,"d":{}}"#, None, &metrics);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_reconnects_total 1\n"));
        assert!(text.contains("two_bot_gateway_resumes_total 1\n"));
        assert!(text.contains("two_bot_gateway_latency_seconds NaN\n"));
    }

    #[test]
    fn emitted_series_keep_stable_event_label_spellings() {
        // Rename-proofing: scrapers and alert rules match these exact label
        // values, so the observer must emit the canonical spellings already
        // defined in crates/core/src/metrics.rs.
        let metrics = metrics::Metrics::default();
        let mut observer = Observer::default();
        observer.observe_text(r#"{"op":0,"t":"RESUMED","s":7,"d":{}}"#, None, &metrics);
        observer.observe_text(
            r#"{"op":0,"t":"BOGUS_FUTURE_TYPE","s":8,"d":{}}"#,
            None,
            &metrics,
        );
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_events_total{event=\"RESUMED\"} 1\n"));
        assert!(text.contains("two_bot_gateway_resumes_total 1\n"));
        assert!(text.contains("two_bot_gateway_events_total{event=\"other\"} 1\n"));
    }

    #[test]
    fn reaction_dispatches_increment_named_labels_not_other() {
        // Self-role reactions ride their own bounded lane; the observer must
        // count the Discord dispatch names the bot actually receives
        // (`GUILD_MESSAGE_REACTIONS` intent, `ReactionAdd`/`ReactionRemove`
        // handling) under their own labels instead of `other`.
        let metrics = metrics::Metrics::default();
        let mut observer = Observer::default();
        observer.observe_text(
            r#"{"op":0,"t":"MESSAGE_REACTION_ADD","s":9,"d":{}}"#,
            None,
            &metrics,
        );
        observer.observe_text(
            r#"{"op":0,"t":"MESSAGE_REACTION_REMOVE","s":10,"d":{}}"#,
            None,
            &metrics,
        );
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_events_total{event=\"MESSAGE_REACTION_ADD\"} 1\n"));
        assert!(
            text.contains("two_bot_gateway_events_total{event=\"MESSAGE_REACTION_REMOVE\"} 1\n")
        );
        assert!(text.contains("two_bot_gateway_events_total{event=\"other\"} 0\n"));
    }

    #[test]
    fn dispatch_commit_records_session_checkpoint_success() {
        // DispatchTimer commits durable gateway checkpoints under the stable
        // `session_checkpoint` job label. Supervisor outcomes (TOG-11144)
        // are separately owned and not asserted here.
        let timer = DispatchTimer::start();
        timer.committed();
        let text = metrics::global().render(None);
        let line = text
            .lines()
            .find(|line| {
                line.starts_with(
                    "two_bot_job_last_success_timestamp_seconds{job=\"session_checkpoint\"} ",
                )
            })
            .expect("session_checkpoint series");
        let value: u64 = line.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(value > 0, "committed checkpoint must record success");
    }
}
