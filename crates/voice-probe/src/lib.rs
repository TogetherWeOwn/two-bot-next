//! UDP probe for the music voice-node spike: Discord-shaped IP discovery
//! plus an RTT/loss run against a controlled echo server.
//!
//! Cloudflare Containers are documented as unable to *receive* non-HTTP
//! UDP. Whether *outbound* UDP plus replies (what Discord voice needs for
//! IP discovery and RTP) works is undocumented. This crate proves or
//! disproves it without touching Discord: a minimal probe exchanges
//! Discord-shaped UDP with an echo server the operator controls, then
//! prints one JSON evidence object to stdout.
//!
//! The crate is standalone by design: it depends only on `tokio`, `serde`,
//! `serde_json` and `time`, and touches no gateway or Discord code paths.

pub mod cli;
pub mod discovery;
pub mod evidence;
pub mod runner;
pub mod stats;
