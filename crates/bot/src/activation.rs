//! Boot-time capability policy. Identity comes only from the Discord token.

use two_bot_core::{
    activation::{evaluate_activation, ActivationDecision, LiveCapability},
    backup::guild_config::application_id_from_token,
    Config, FeatureGates, RouterGates,
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

    /// The supervised jobs that post under the bot identity (the scheduled-message
    /// ticker and the feed poller) obey the same fence as the router verbs that
    /// feed them. Narrowing only: a refused capability turns the gate off, a
    /// permitted one leaves the configured value alone.
    #[must_use]
    pub(crate) fn constrain_features(&self, mut gates: FeatureGates) -> FeatureGates {
        gates.automations &= self.permitted(LiveCapability::Automations);
        gates.announcements &= self.permitted(LiveCapability::Announcements);
        // Text commands are only effective while automations are on.
        gates.text_commands &= gates.automations;
        gates
    }
}

/// Identity pairs for the job-registration tests in sibling modules, written
/// out so a mutated constant cannot move their expectations. The tokens carry
/// the public application ids as their first segment and are not credentials.
/// The module is inline `cfg(test)`, which the snowflake gate treats as test-only.
#[cfg(test)]
pub mod fixtures {
    use super::BootActivation;

    pub const STAGING_GUILD: u64 = 1545644954272137297;
    pub const STAGING_TOKEN: &str = "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature";
    pub const LIVE_GUILD: u64 = 326474832151838730;
    pub const LIVE_TOKEN: &str = "MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature";
    const THIRD_GUILD: u64 = 1555555555555555555;

    #[must_use]
    pub fn staging() -> BootActivation {
        BootActivation::from_token(Some(STAGING_GUILD), Some(STAGING_TOKEN))
    }

    #[must_use]
    pub fn live() -> BootActivation {
        BootActivation::from_token(Some(LIVE_GUILD), Some(LIVE_TOKEN))
    }

    /// Every identity that must not run a job gated on automations or
    /// announcements: the live pair (cleared for self_roles only), an unknown
    /// guild, a mismatched pair, and a missing or unparseable token or guild.
    #[must_use]
    pub fn refused() -> Vec<(&'static str, BootActivation)> {
        vec![
            ("live pair", live()),
            (
                "unknown guild",
                BootActivation::from_token(Some(THIRD_GUILD), Some(STAGING_TOKEN)),
            ),
            (
                "staging guild with live token",
                BootActivation::from_token(Some(STAGING_GUILD), Some(LIVE_TOKEN)),
            ),
            (
                "live guild with staging token",
                BootActivation::from_token(Some(LIVE_GUILD), Some(STAGING_TOKEN)),
            ),
            (
                "no guild",
                BootActivation::from_token(None, Some(STAGING_TOKEN)),
            ),
            (
                "unparseable token",
                BootActivation::from_token(Some(STAGING_GUILD), Some("not-a-token")),
            ),
            (
                "no token",
                BootActivation::from_token(Some(STAGING_GUILD), None),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{live, refused, staging};
    use super::*;

    fn all_on() -> FeatureGates {
        FeatureGates::from_map(&std::collections::HashMap::from([
            ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
            ("TWO_ANNOUNCEMENTS".to_owned(), "1".to_owned()),
            ("TWO_TEXT_COMMANDS".to_owned(), "1".to_owned()),
            ("TWO_FEED_POLL_SECONDS".to_owned(), "120".to_owned()),
        ]))
        .expect("gates parse")
    }

    #[test]
    fn constrain_features_follows_identity_and_only_narrows() {
        let on = all_on();
        // Staging pair: every capability is permitted, so nothing narrows.
        assert_eq!(staging().constrain_features(on), on);
        // Live pair: only self_roles is cleared, so the posting jobs turn off and
        // the unrelated poll interval is untouched.
        let narrowed = live().constrain_features(on);
        assert!(!narrowed.automations && !narrowed.announcements && !narrowed.text_commands);
        assert_eq!(narrowed.feed_poll_seconds, 120);
        for (label, activation) in refused() {
            let narrowed = activation.constrain_features(on);
            assert!(
                !narrowed.automations && !narrowed.announcements && !narrowed.text_commands,
                "{label} must narrow every posting gate"
            );
        }
        // Permission never enables a gate the environment left off.
        let off = FeatureGates::from_map(&std::collections::HashMap::new()).unwrap();
        assert_eq!(staging().constrain_features(off), off);
        // The fixtures are the identities the fence itself recognizes.
        for capability in LiveCapability::ALL {
            assert!(staging().permitted(capability));
            assert_eq!(
                live().permitted(capability),
                capability == LiveCapability::SelfRoles
            );
        }
    }

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
