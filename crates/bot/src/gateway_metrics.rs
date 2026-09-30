//! Gateway observations are independent of durable dispatch/checkpoint decisions.

use std::time::{Instant, SystemTime, UNIX_EPOCH};
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
            Message::Text(text) => {
                if let Ok(packet) = serde_json::from_str::<Packet<'_>>(text) {
                    match packet.op {
                        0 => metrics::global().gateway_event(packet.t.unwrap_or("other")),
                        10 => {
                            if self.saw_hello {
                                metrics::global().gateway_reconnect();
                            }
                            self.saw_hello = true;
                        }
                        11 => {
                            metrics::global().gateway_event("HEARTBEAT_ACK");
                            // Most recent completed heartbeat, not an average or elapsed uptime.
                            // https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Latency.html#method.recent
                            if let Some(latency) = shard.latency().recent().first() {
                                metrics::global().gateway_latency(*latency);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Close(_) => metrics::global().gateway_event("GATEWAY_CLOSE"),
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
