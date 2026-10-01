//! Identity and capability fence shared by bot boot adapters.
//!
//! Only the staging guild/application pair may activate every capability. The
//! live pair may activate only the reviewed clearance below. Callers derive the
//! application id from the token used by their Discord client, never config.

use crate::backup::guild_config::{
    LIVE_BOT_APPLICATION_ID, LIVE_GUILD_ID, STAGING_BOT_APPLICATION_ID, TWO_STAGING_GUILD_ID,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveCapability {
    SelfRoles,
    Announcements,
    Automations,
    Automod,
    Moderation,
    /// Added with the tickets lifecycle slice; live denial is the default
    /// until the cleared allowlist below is reviewed and widened.
    Tickets,
}

impl LiveCapability {
    pub const ALL: [Self; 6] = [
        Self::SelfRoles,
        Self::Announcements,
        Self::Automations,
        Self::Automod,
        Self::Moderation,
        Self::Tickets,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfRoles => "self_roles",
            Self::Announcements => "announcements",
            Self::Automations => "automations",
            Self::Automod => "automod",
            Self::Moderation => "moderation",
            Self::Tickets => "tickets",
        }
    }
}

/// Widening live activation requires a reviewed change to this one allowlist.
pub const LIVE_CLEARED_CAPABILITIES: &[LiveCapability] = &[LiveCapability::SelfRoles];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationEnvironment {
    Staging,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ActivationRefusal {
    #[error("the bot token is missing or unparseable, so its application is unknown")]
    UnknownApplication,
    #[error("expected the staging guild/application pair or the live guild/application pair")]
    UnknownIdentity,
    #[error("capability is not cleared for the live guild")]
    NotClearedForLive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationDecision {
    Permitted(ActivationEnvironment),
    Refused(ActivationRefusal),
}

/// Pure decision: no environment reads, client calls, or caller-supplied clearance.
#[must_use]
pub fn evaluate_activation(
    guild_id: Option<&str>,
    app_id: Option<&str>,
    capability: LiveCapability,
) -> ActivationDecision {
    let Some(app_id) = app_id.filter(|id| !id.is_empty()) else {
        return ActivationDecision::Refused(ActivationRefusal::UnknownApplication);
    };
    if guild_id == Some(TWO_STAGING_GUILD_ID) && app_id == STAGING_BOT_APPLICATION_ID {
        return ActivationDecision::Permitted(ActivationEnvironment::Staging);
    }
    if guild_id == Some(LIVE_GUILD_ID) && app_id == LIVE_BOT_APPLICATION_ID {
        return if LIVE_CLEARED_CAPABILITIES.contains(&capability) {
            ActivationDecision::Permitted(ActivationEnvironment::Live)
        } else {
            ActivationDecision::Refused(ActivationRefusal::NotClearedForLive)
        };
    }
    ActivationDecision::Refused(ActivationRefusal::UnknownIdentity)
}

pub fn assert_activation_permitted(
    guild_id: Option<&str>,
    app_id: Option<&str>,
    capability: LiveCapability,
) -> Result<ActivationEnvironment, ActivationRefusal> {
    match evaluate_activation(guild_id, app_id, capability) {
        ActivationDecision::Permitted(environment) => Ok(environment),
        ActivationDecision::Refused(reason) => Err(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Written out so a mutation to an identity constant cannot move expectations.
    const STAGING_GUILD: &str = "1545644954272137297";
    const STAGING_APP: &str = "1469137636663758888";
    const LIVE_GUILD: &str = "326474832151838730";
    const LIVE_APP: &str = "1539711683898118154";
    const THIRD_GUILD: &str = "1555555555555555555";
    const THIRD_APP: &str = "1555555555555555556";

    #[test]
    fn activation_identity_capability_matrix() {
        for guild in [
            Some(STAGING_GUILD),
            Some(LIVE_GUILD),
            Some(THIRD_GUILD),
            None,
            Some(""),
        ] {
            for app in [
                Some(STAGING_APP),
                Some(LIVE_APP),
                Some(THIRD_APP),
                None,
                Some(""),
            ] {
                for capability in LiveCapability::ALL {
                    let expected = if app.is_none() || app == Some("") {
                        Err(ActivationRefusal::UnknownApplication)
                    } else if guild == Some(STAGING_GUILD) && app == Some(STAGING_APP) {
                        Ok(ActivationEnvironment::Staging)
                    } else if guild == Some(LIVE_GUILD) && app == Some(LIVE_APP) {
                        if capability == LiveCapability::SelfRoles {
                            Ok(ActivationEnvironment::Live)
                        } else {
                            Err(ActivationRefusal::NotClearedForLive)
                        }
                    } else {
                        Err(ActivationRefusal::UnknownIdentity)
                    };
                    assert_eq!(
                        assert_activation_permitted(guild, app, capability),
                        expected,
                        "guild={guild:?}, app={app:?}, capability={capability:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn activation_shipped_clearance_is_self_roles_only() {
        assert_eq!(LIVE_CLEARED_CAPABILITIES, &[LiveCapability::SelfRoles]);
    }

    #[test]
    fn live_pair_refuses_uncleared_tickets() {
        // The tickets slice joined after the fence; live denial is the
        // shipped default until the allowlist is deliberately widened.
        assert_eq!(
            assert_activation_permitted(Some(LIVE_GUILD), Some(LIVE_APP), LiveCapability::Tickets),
            Err(ActivationRefusal::NotClearedForLive)
        );
        assert_eq!(
            assert_activation_permitted(
                Some(STAGING_GUILD),
                Some(STAGING_APP),
                LiveCapability::Tickets
            ),
            Ok(ActivationEnvironment::Staging)
        );
    }
}
