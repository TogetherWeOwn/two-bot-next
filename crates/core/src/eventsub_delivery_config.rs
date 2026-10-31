//! EventSub go-live delivery configuration (staging-only, default-dark).
//!
//! Delivery is a second, independent gate on top of the private EventSub
//! receiver ([`crate::eventsub_receiver_config`]): the receiver can run
//! ack-only while live posting stays dark. This config provisions the three
//! delivery inputs — the announcements channel, the Discord invite and the
//! broadcaster allowlist — as Operator-set environment, never
//! dashboard-stored (see the `TWO_EVENTSUB_*` rows in
//! [`crate::settings`]). `TWO_EVENTSUB_DELIVERY` unset or `"0"` is dark and
//! reads nothing else. Exactly `"1"` requires all three inputs to be
//! present and valid; any other enabled state is a startup error, never a
//! silent downgrade to ack-only. The broadcaster allowlist reuses
//! `TWO_TWITCH_BROADCASTERS`, the same variable the subscription reconcile
//! subscribes from, so the set Twitch delivers for and the set allowed to
//! announce cannot drift apart.

use std::{collections::HashMap, env};

use crate::{config::ConfigError, stream_funnel::validated_invite_url};

/// Master gate for go-live delivery: exactly `"1"` enables, unset/`"0"` is
/// dark, anything else refuses startup.
pub const TWO_EVENTSUB_DELIVERY: &str = "TWO_EVENTSUB_DELIVERY";
/// Announcements channel snowflake the go-live post lands in.
pub const TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID: &str = "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID";
/// Discord invite URL rendered into the go-live post.
pub const TWO_EVENTSUB_INVITE_URL: &str = "TWO_EVENTSUB_INVITE_URL";
/// Broadcaster allowlist, shared with the subscription reconcile.
pub const TWO_TWITCH_BROADCASTERS: &str = "TWO_TWITCH_BROADCASTERS";

/// Validated delivery inputs. Accessors only; construct via the parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsubDeliveryConfig {
    broadcasters: Vec<String>,
    invite_url: String,
    channel_id: String,
}

impl EventsubDeliveryConfig {
    /// Load from the process environment (see the module docs for the gate).
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(env::var)
    }

    /// Validate an explicit settings snapshot without process env.
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(|name| vars.get(name).cloned().ok_or(env::VarError::NotPresent))
    }

    fn from_lookup(
        mut lookup: impl FnMut(&'static str) -> Result<String, env::VarError>,
    ) -> Result<Option<Self>, ConfigError> {
        match lookup(TWO_EVENTSUB_DELIVERY) {
            Err(env::VarError::NotPresent) => return Ok(None),
            Ok(value) if value == "0" => return Ok(None),
            Ok(value) if value == "1" => {}
            _ => return Err(invalid(TWO_EVENTSUB_DELIVERY, "expected 0 or 1")),
        }

        let raw_broadcasters = required(&mut lookup, TWO_TWITCH_BROADCASTERS)?;
        let broadcasters = parse_broadcasters(&raw_broadcasters)?;

        let raw_channel = required(&mut lookup, TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID)?;
        let channel_id = parse_snowflake(
            &raw_channel,
            TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID,
            "expected a Discord channel snowflake",
        )?;

        let raw_invite = required(&mut lookup, TWO_EVENTSUB_INVITE_URL)?;
        let invite_url = validated_invite_url(&raw_invite).map_err(|_| {
            invalid(
                TWO_EVENTSUB_INVITE_URL,
                "value must be a postable https Discord invite URL",
            )
        })?;

        Ok(Some(Self {
            broadcasters,
            invite_url,
            channel_id,
        }))
    }

    /// Twitch broadcaster user ids allowed to announce (sorted, deduped).
    #[must_use]
    pub fn broadcasters(&self) -> &[String] {
        &self.broadcasters
    }

    /// The validated Discord invite URL rendered into the post.
    #[must_use]
    pub fn invite_url(&self) -> &str {
        &self.invite_url
    }

    /// The announcements channel snowflake (canonical decimal).
    #[must_use]
    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }
}

/// Split the broadcaster allowlist: comma-separated, trimmed, sorted and
/// deduplicated so the result is deterministic (the same shape the bot's
/// reconcile pass uses). Twitch sends `broadcaster_user_id` as a canonical
/// numeric string and the delivery allowlist match is exact, so a
/// non-canonical entry (a login name, a leading-zero id) would silently
/// never announce; refuse it loudly instead.
fn parse_broadcasters(raw: &str) -> Result<Vec<String>, ConfigError> {
    let mut ids: Vec<String> = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        ids.push(parse_snowflake(
            part,
            TWO_TWITCH_BROADCASTERS,
            "entries must be Twitch broadcaster user ids (canonical numeric)",
        )?);
    }
    if ids.is_empty() {
        return Err(invalid(
            TWO_TWITCH_BROADCASTERS,
            "allowlist must name at least one broadcaster",
        ));
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// A canonical nonzero numeric id: ASCII digits parsing to `1..=u64::MAX`
/// with no leading zeros, so the stored form is exactly what
/// Twitch/Discord put on the wire.
fn parse_snowflake(
    raw: &str,
    name: &'static str,
    reason: &'static str,
) -> Result<String, ConfigError> {
    let trimmed = raw.trim();
    let parsed: u64 = trimmed.parse().map_err(|_| invalid(name, reason))?;
    if parsed == 0 || parsed.to_string() != trimmed {
        return Err(invalid(name, reason));
    }
    Ok(trimmed.to_owned())
}

fn invalid(name: &'static str, reason: &'static str) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.to_owned(),
    }
}

fn required(
    lookup: &mut impl FnMut(&'static str) -> Result<String, env::VarError>,
    name: &'static str,
) -> Result<String, ConfigError> {
    match lookup(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Err(ConfigError::Missing(name)),
        Err(env::VarError::NotUnicode(_)) => Err(invalid(name, "value is not valid unicode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> HashMap<String, String> {
        HashMap::from([
            ("TWO_EVENTSUB_DELIVERY".to_owned(), "1".to_owned()),
            ("TWO_TWITCH_BROADCASTERS".to_owned(), "222,111".to_owned()),
            (
                "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID".to_owned(),
                "777".to_owned(),
            ),
            (
                "TWO_EVENTSUB_INVITE_URL".to_owned(),
                "https://discord.gg/example".to_owned(),
            ),
        ])
    }

    fn load(vars: &HashMap<String, String>) -> Result<Option<EventsubDeliveryConfig>, ConfigError> {
        EventsubDeliveryConfig::from_lookup(|name| {
            vars.get(name).cloned().ok_or(env::VarError::NotPresent)
        })
    }

    #[test]
    fn disabled_does_not_read_any_delivery_input() {
        for flag in [None, Some("0")] {
            assert!(EventsubDeliveryConfig::from_lookup(|name| {
                assert_eq!(name, TWO_EVENTSUB_DELIVERY);
                flag.map(str::to_owned).ok_or(env::VarError::NotPresent)
            })
            .unwrap()
            .is_none());
        }
        for value in ["", "true", "false", " 1", "2"] {
            let mut vars = vars();
            vars.insert("TWO_EVENTSUB_DELIVERY".to_owned(), value.to_owned());
            assert!(load(&vars).is_err(), "gate {value:?}");
        }
    }

    #[test]
    fn enabled_requires_channel_invite_and_broadcasters() {
        for missing in [
            "TWO_TWITCH_BROADCASTERS",
            "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID",
            "TWO_EVENTSUB_INVITE_URL",
        ] {
            let mut vars = vars();
            vars.remove(missing);
            assert!(load(&vars).is_err(), "missing {missing} must refuse");
            vars.insert(missing.to_owned(), "  ".to_owned());
            assert!(load(&vars).is_err(), "blank {missing} must refuse");
        }
    }

    #[test]
    fn enabled_validates_the_allowlist_contents() {
        for bad in ["", ",,", "some-login", "111,some-login", "0111", "0", "-1"] {
            let mut vars = vars();
            vars.insert("TWO_TWITCH_BROADCASTERS".to_owned(), bad.to_owned());
            assert!(load(&vars).is_err(), "allowlist {bad:?}");
        }
        let mut vars = vars();
        vars.insert(
            "TWO_TWITCH_BROADCASTERS".to_owned(),
            "222, 111, 222".to_owned(),
        );
        let config = load(&vars).unwrap().expect("enabled");
        assert_eq!(config.broadcasters(), ["111", "222"]);
    }

    #[test]
    fn enabled_validates_the_channel_snowflake() {
        for bad in ["0", "abc", "0777", "777.0", "18446744073709551616"] {
            let mut vars = vars();
            vars.insert(
                "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID".to_owned(),
                bad.to_owned(),
            );
            assert!(load(&vars).is_err(), "channel {bad:?}");
        }
        let mut vars = vars();
        vars.insert(
            "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID".to_owned(),
            " 777 ".to_owned(),
        );
        let config = load(&vars).unwrap().expect("enabled");
        assert_eq!(config.channel_id(), "777");
    }

    #[test]
    fn enabled_validates_the_invite_like_the_renderer() {
        for bad in [
            "http://discord.gg/example",
            "https://discord.gg/@everyone",
            "not a url",
        ] {
            let mut vars = vars();
            vars.insert("TWO_EVENTSUB_INVITE_URL".to_owned(), bad.to_owned());
            assert!(load(&vars).is_err(), "invite {bad:?}");
        }
    }

    #[test]
    fn from_map_accepts_a_valid_snapshot() {
        let config = EventsubDeliveryConfig::from_map(&vars())
            .unwrap()
            .expect("enabled");
        assert_eq!(config.broadcasters(), ["111", "222"]);
        assert_eq!(config.invite_url(), "https://discord.gg/example");
        assert_eq!(config.channel_id(), "777");
    }

    #[test]
    fn enabled_exposes_the_validated_inputs() {
        let config = load(&vars()).unwrap().expect("enabled");
        assert_eq!(config.broadcasters(), ["111", "222"]);
        assert_eq!(config.invite_url(), "https://discord.gg/example");
        assert_eq!(config.channel_id(), "777");
    }
}
