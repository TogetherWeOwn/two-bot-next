//! The JSON evidence object a probe run prints to stdout.
//!
//! Field order is the contract the operator deploy card consumes; the
//! render test pins the exact bytes.

use serde::Serialize;

use crate::stats::RttSummary;

/// One completed probe run, serialized as a single JSON object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    /// The discovery exchange succeeded.
    pub discovery_ok: bool,
    /// Probe source as observed by the echo (`ip:port`), if discovered.
    pub observed_egress: Option<String>,
    /// RTT packets sent.
    pub sent: u64,
    /// RTT packets received back (unique sequences).
    pub received: u64,
    /// Receive shortfall as a percentage of sent.
    pub loss_pct: f64,
    /// RTT summary in milliseconds.
    pub rtt_ms: RttSummary,
    /// Wall-clock seconds of the RTT phase.
    pub duration_s: f64,
    /// Echo target as given (`host:port`).
    pub echo_host: String,
    /// Run start as RFC 3339 UTC.
    pub started_at: String,
}

/// Render the evidence object as one JSON line (no trailing newline; the
/// caller terminates it on write).
#[must_use]
pub fn render(evidence: &Evidence) -> String {
    serde_json::to_string(evidence).expect("evidence serializes to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::RttSummary;

    fn sample() -> Evidence {
        Evidence {
            discovery_ok: true,
            observed_egress: Some("203.0.113.7:52130".to_owned()),
            sent: 3000,
            received: 2991,
            loss_pct: 0.3,
            rtt_ms: RttSummary {
                min: Some(1.125),
                p50: Some(2.5),
                p95: Some(9.75),
                max: Some(41.0),
            },
            duration_s: 300.25,
            echo_host: "echo.example:9001".to_owned(),
            started_at: "2026-10-22T10:00:00Z".to_owned(),
        }
    }

    #[test]
    fn render_pins_the_evidence_contract() {
        let json = render(&sample());

        assert_eq!(
            json,
            r#"{"discovery_ok":true,"observed_egress":"203.0.113.7:52130","sent":3000,"received":2991,"loss_pct":0.3,"rtt_ms":{"min":1.125,"p50":2.5,"p95":9.75,"max":41.0},"duration_s":300.25,"echo_host":"echo.example:9001","started_at":"2026-10-22T10:00:00Z"}"#
        );
    }

    #[test]
    fn render_keeps_schema_keys_when_the_run_failed() {
        let failed = Evidence {
            discovery_ok: false,
            observed_egress: None,
            sent: 0,
            received: 0,
            loss_pct: 0.0,
            rtt_ms: RttSummary {
                min: None,
                p50: None,
                p95: None,
                max: None,
            },
            duration_s: 0.0,
            echo_host: "echo.example:9001".to_owned(),
            started_at: "2026-10-22T10:00:00Z".to_owned(),
        };

        let value: serde_json::Value = serde_json::from_str(&render(&failed)).unwrap();
        let object = value.as_object().unwrap();
        for key in [
            "discovery_ok",
            "observed_egress",
            "sent",
            "received",
            "loss_pct",
            "rtt_ms",
            "duration_s",
            "echo_host",
            "started_at",
        ] {
            assert!(object.contains_key(key), "missing key {key}");
        }
        assert_eq!(object["discovery_ok"], false);
        assert!(object["observed_egress"].is_null());
        assert!(object["rtt_ms"]["p50"].is_null());
    }
}
