//! Deployment-only onboarding routing. No production IDs are inferred.

use std::collections::HashMap;

use two_bot_core::onboarding::{build_session_picks, OnboardingGates, OnboardingMode, SessionPick};

#[derive(Debug, Clone)]
pub struct OnboardingConfig {
    pub guild_id: u64,
    pub gates: OnboardingGates,
    pub landing_channel_ids: Vec<u64>,
    pub goodbye_channel_ids: Vec<u64>,
    pub anchor_channel_id: Option<u64>,
    pub session_picks: Vec<SessionPick>,
}

#[derive(Debug, thiserror::Error)]
pub enum OnboardingConfigError {
    #[error(transparent)]
    Gates(#[from] two_bot_core::onboarding::OnboardingGateError),
    #[error("{0} must contain a nonzero Discord snowflake")]
    InvalidId(&'static str),
    #[error("{0} is required for the selected onboarding mode")]
    Missing(&'static str),
}

fn id(value: &str, key: &'static str) -> Result<u64, OnboardingConfigError> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|v| *v != 0)
        .ok_or(OnboardingConfigError::InvalidId(key))
}

fn optional_id(
    vars: &HashMap<String, String>,
    key: &'static str,
) -> Result<Option<u64>, OnboardingConfigError> {
    vars.get(key)
        .filter(|v| !v.trim().is_empty())
        .map(|v| id(v, key))
        .transpose()
}

pub fn channel_ids(
    value: Option<&str>,
    key: &'static str,
) -> Result<Vec<u64>, OnboardingConfigError> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| id(v, key))
        .collect()
}

impl OnboardingConfig {
    /// Missing guild disables onboarding rather than falling back to a live ID.
    /// The bot passes only deployment fields; never retain its token here.
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Option<Self>, OnboardingConfigError> {
        let gates = OnboardingGates::from_map(vars)?;
        let Some(guild_id) = optional_id(vars, "DISCORD_GUILD_ID")? else {
            return Ok(None);
        };
        let anchor_channel_id = optional_id(vars, "DISCORD_ANCHOR_WELCOME_CHANNEL_ID")?;
        if gates.mode == OnboardingMode::Anchor && anchor_channel_id.is_none() {
            return Err(OnboardingConfigError::Missing(
                "DISCORD_ANCHOR_WELCOME_CHANNEL_ID",
            ));
        }
        let session_picks = if gates.mode == OnboardingMode::Session {
            let looking = optional_id(vars, "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID")?.ok_or(
                OnboardingConfigError::Missing("DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID"),
            )?;
            let lobby = optional_id(vars, "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID")?.ok_or(
                OnboardingConfigError::Missing("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID"),
            )?;
            build_session_picks(&looking.to_string(), &lobby.to_string()).to_vec()
        } else {
            Vec::new()
        };
        Ok(Some(Self {
            guild_id,
            gates,
            landing_channel_ids: channel_ids(
                vars.get("DISCORD_LANDING_CHANNEL_IDS").map(String::as_str),
                "DISCORD_LANDING_CHANNEL_IDS",
            )?,
            goodbye_channel_ids: channel_ids(
                vars.get("DISCORD_GOODBYE_CHANNEL_IDS").map(String::as_str),
                "DISCORD_GOODBYE_CHANNEL_IDS",
            )?,
            anchor_channel_id,
            session_picks,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_implicit_live_guild_or_channel() {
        assert!(OnboardingConfig::from_map(&HashMap::new())
            .unwrap()
            .is_none());
        let vars = HashMap::from([("DISCORD_GUILD_ID".into(), "2222".into())]);
        let cfg = OnboardingConfig::from_map(&vars).unwrap().unwrap();
        assert_eq!(cfg.guild_id, 2222);
        assert!(cfg.landing_channel_ids.is_empty());
        assert!(cfg.goodbye_channel_ids.is_empty());
        assert!(cfg.anchor_channel_id.is_none());
        assert!(cfg.session_picks.is_empty());
    }

    #[test]
    fn mode_specific_config_is_required() {
        let mut vars = HashMap::from([
            ("DISCORD_GUILD_ID".into(), "2222".into()),
            ("TWO_ONBOARDING_MODE".into(), "session".into()),
        ]);
        assert!(matches!(
            OnboardingConfig::from_map(&vars),
            Err(OnboardingConfigError::Missing(_))
        ));
        vars.insert(
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "10".into(),
        );
        vars.insert("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "11".into());
        vars.insert("DISCORD_LANDING_CHANNEL_IDS".into(), " 12, ,13 ".into());
        let cfg = OnboardingConfig::from_map(&vars).unwrap().unwrap();
        assert_eq!(cfg.landing_channel_ids, vec![12, 13]);
        assert_eq!(cfg.session_picks[0].channel_id, "10");
        assert_eq!(cfg.session_picks[1].channel_id, "11");
        vars.insert("TWO_ONBOARDING_MODE".into(), "anchor".into());
        assert!(matches!(
            OnboardingConfig::from_map(&vars),
            Err(OnboardingConfigError::Missing(
                "DISCORD_ANCHOR_WELCOME_CHANNEL_ID"
            ))
        ));
    }

    #[test]
    fn invalid_targets_fail_closed() {
        for invalid in ["0", "-1", "abc", "18446744073709551616"] {
            assert!(channel_ids(Some(invalid), "DISCORD_LANDING_CHANNEL_IDS").is_err());
        }
    }
}
