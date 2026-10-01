//! Boot-time capability policy. Identity comes only from the Discord token.

use two_bot_core::{
    activation::{evaluate_activation, ActivationDecision, LiveCapability},
    backup::guild_config::application_id_from_token,
    Config, RouterGates,
};

#[derive(Clone)]
pub(crate) struct BootActivation {
    guild_id: Option<u64>,
    application_id: Option<String>,
}

impl BootActivation {
    #[must_use]
    pub(crate) fn from_config(config: &Config) -> Self {
        Self::from_token(
            config.guild_id,
            config
                .discord_token
                .as_ref()
                .map(|token| token.expose().as_str()),
        )
    }

    #[must_use]
    pub(crate) fn from_token(guild_id: Option<u64>, token: Option<&str>) -> Self {
        Self {
            guild_id,
            application_id: token.and_then(application_id_from_token),
        }
    }

    pub(crate) fn log_refusals(&self) {
        let guild = self.guild_id.map(|id| id.to_string());
        for capability in LiveCapability::ALL {
            if let ActivationDecision::Refused(reason) =
                evaluate_activation(guild.as_deref(), self.application_id.as_deref(), capability)
            {
                tracing::warn!(
                    capability = capability.as_str(),
                    guild_id = ?self.guild_id,
                    application_id = ?self.application_id,
                    reason = %reason,
                    "live activation refused"
                );
            }
        }
    }

    #[must_use]
    pub(crate) fn permitted(&self, capability: LiveCapability) -> bool {
        let guild = self.guild_id.map(|id| id.to_string());
        matches!(
            evaluate_activation(guild.as_deref(), self.application_id.as_deref(), capability),
            ActivationDecision::Permitted(_)
        )
    }

    #[must_use]
    pub(crate) fn application_id(&self) -> Option<u64> {
        self.application_id.as_deref()?.parse().ok()
    }

    /// Identity permission can only narrow configured gates, never enable them.
    #[must_use]
    pub(crate) fn constrain_router(&self, mut gates: RouterGates) -> RouterGates {
        gates.automations &= self.permitted(LiveCapability::Automations);
        gates.announcements &= self.permitted(LiveCapability::Announcements);
        gates.moderation &= self.permitted(LiveCapability::Moderation);
        gates.self_roles &= self.permitted(LiveCapability::SelfRoles);
        gates.tickets &= self.permitted(LiveCapability::Tickets);
        gates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_bare_and_prefixed_token_decisions_match() {
        for guild in [1545644954272137297, 326474832151838730] {
            for token in [
                "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
                "MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature",
            ] {
                let bare = BootActivation::from_token(Some(guild), Some(token));
                let prefixed =
                    BootActivation::from_token(Some(guild), Some(&format!("Bot {token}")));
                assert_eq!(bare.application_id(), prefixed.application_id());
                for capability in LiveCapability::ALL {
                    assert_eq!(bare.permitted(capability), prefixed.permitted(capability));
                }
            }
        }
    }

    #[test]
    fn activation_token_identity_not_configured_application() {
        // Synthetic first segment for public staging application id, not a credential.
        let config = Config {
            guild_id: Some(1545644954272137297),
            discord_token: Some(two_bot_core::Secret::new(
                "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature".to_owned(),
            )),
            database_url: None,
            listen_addr: "127.0.0.1:0".to_owned(),
        };
        let activation = BootActivation::from_config(&config);
        assert_eq!(activation.application_id(), Some(1469137636663758888));
        for capability in LiveCapability::ALL {
            assert!(activation.permitted(capability));
        }
        for token in [None, Some(""), Some("not-a-token"), Some("b3dlbg.x.y")] {
            let activation = BootActivation::from_token(Some(1545644954272137297), token);
            for capability in LiveCapability::ALL {
                assert!(!activation.permitted(capability));
            }
        }
    }
}
