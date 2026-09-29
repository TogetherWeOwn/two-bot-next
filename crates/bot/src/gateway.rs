//! Gateway shard supervisor seam (S1 skeleton).
//!
//! Owns the shard lifecycle state the /readyz gate reads. S3 fills in the
//! twilight `Shard` runner, session persistence for RESUME across Container
//! restarts, and event dispatch through [`two_bot_discord::event_to_core`].
//! Until then the shard is parked: configured-but-disconnected when a token
//! exists, down when it does not.

use two_bot_core::{ComponentStatus, Config};

/// Supervisor-visible gateway state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayState {
    /// No token configured; the shard will not start.
    Unconfigured,
    /// Token present; S3 connects the shard from here.
    Armed,
    /// Shard connected and identified (S3+; constructed by the supervisor,
    /// exercised by the /readyz test).
    #[cfg_attr(not(test), allow(dead_code))]
    Connected,
}

impl GatewayState {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        if config.gateway_configured() {
            Self::Armed
        } else {
            Self::Unconfigured
        }
    }

    #[must_use]
    pub fn status(&self) -> ComponentStatus {
        match self {
            Self::Unconfigured => ComponentStatus::Down,
            Self::Armed => ComponentStatus::Starting,
            Self::Connected => ComponentStatus::Ready,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> Config {
        Config {
            discord_token: Some("token".to_owned()),
            database_url: None,
            listen_addr: "0.0.0.0:8080".to_owned(),
            guild_id: None,
        }
    }

    #[test]
    fn armed_reports_starting_not_ready() {
        let state = GatewayState::new(&configured());
        assert_eq!(state, GatewayState::Armed);
        assert_eq!(state.status(), ComponentStatus::Starting);
    }

    #[test]
    fn unconfigured_reports_down() {
        let state = GatewayState::new(&Config {
            discord_token: None,
            database_url: None,
            listen_addr: "0.0.0.0:8080".to_owned(),
            guild_id: None,
        });
        assert_eq!(state.status(), ComponentStatus::Down);
    }
}
