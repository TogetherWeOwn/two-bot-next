//! Private EventSub receiver configuration (staging-only, default-dark).
//!
//! Loading this does not approve a deployment or provision keys. The Worker
//! route (`wrangler/src/eventsub.ts`) stays dark unless `EVENTSUB_INGRESS`
//! and `TWO_EVENTSUB` are both exactly `"1"`; this config enables the
//! container listener only when `TWO_EVENTSUB` is exactly `"1"` plus a valid
//! bind and the Operator-provisioned `TWO_TWITCH_WEBHOOK_SECRET`. An enabled
//! but invalid receiver must stop startup, never silently downgrade.

use std::{collections::HashMap, env, net::SocketAddr};

use crate::{
    config::ConfigError,
    internal_actions::{assert_private_bind, MIN_KEY_SECRET_LEN},
    secret::Secret,
    twitch_eventsub::WebhookSecret,
};

/// Validated configuration; no public constructor or secret-bearing Debug.
pub struct EventsubReceiverConfig {
    listen_addr: SocketAddr,
    secret: WebhookSecret,
    /// Dedicated Worker-to-receiver refresh key, when the Operator arms
    /// no-restart rotation. `None` keeps the legacy startup-secret behavior.
    reload_key: Option<Secret<Vec<u8>>>,
}

impl std::fmt::Debug for EventsubReceiverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventsubReceiverConfig")
            .field("listen_addr", &self.listen_addr)
            .field("secret", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl EventsubReceiverConfig {
    /// Unset/`0` is dark. `1` requires bind plus webhook secret. No default
    /// endpoint or secret is invented.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(env::var)
    }

    /// Validate explicit settings without mutating process environment.
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(|name| vars.get(name).cloned().ok_or(env::VarError::NotPresent))
    }

    fn from_lookup(
        mut lookup: impl FnMut(&'static str) -> Result<String, env::VarError>,
    ) -> Result<Option<Self>, ConfigError> {
        match lookup("TWO_EVENTSUB") {
            Err(env::VarError::NotPresent) => return Ok(None),
            Ok(value) if value == "0" => return Ok(None),
            Ok(value) if value == "1" => {}
            _ => return Err(invalid("TWO_EVENTSUB", "expected 0 or 1")),
        }

        let bind = required(&mut lookup, "TWO_EVENTSUB_BIND")?;
        let listen_addr: SocketAddr = bind
            .parse()
            .map_err(|_| invalid("TWO_EVENTSUB_BIND", "expected a literal IP:port"))?;
        if listen_addr.ip().is_unspecified() {
            // Same rule as the internal-actions receiver (TOG-16851): the
            // Containers port check and `containerFetch` cannot reach a
            // loopback-only socket, so the Worker binds the wildcard inside
            // the private container network. A wildcard bind is accepted only
            // with the Worker-set container marker; everywhere else the bind
            // must stay a specific private IP.
            match lookup("TWO_INTERNAL_CONTAINER") {
                Ok(marker) if marker == "1" => {}
                _ => {
                    return Err(invalid(
                        "TWO_EVENTSUB_BIND",
                        "expected a specific private IP",
                    ));
                }
            }
        } else {
            assert_private_bind(&listen_addr.ip().to_string())
                .map_err(|_| invalid("TWO_EVENTSUB_BIND", "expected a specific private IP"))?;
        }
        if listen_addr.port() == 0 {
            return Err(invalid("TWO_EVENTSUB_BIND", "expected a nonzero port"));
        }

        let raw = required(&mut lookup, "TWO_TWITCH_WEBHOOK_SECRET")?;
        let secret = WebhookSecret::new(raw).map_err(|_| {
            invalid(
                "TWO_TWITCH_WEBHOOK_SECRET",
                "value must be 10-100 printable ASCII characters",
            )
        })?;

        // Optional arming for no-restart rotation: absent keeps the legacy
        // startup-secret behavior, but a present-but-short key refuses
        // startup rather than silently running unrefreshable.
        let reload_key = match lookup("TWO_EVENTSUB_RELOAD_KEY") {
            Err(env::VarError::NotPresent) => None,
            Ok(value) if value.len() >= MIN_KEY_SECRET_LEN => Some(Secret::new(value.into_bytes())),
            Ok(_) => {
                return Err(invalid(
                    "TWO_EVENTSUB_RELOAD_KEY",
                    "value must be at least 32 bytes",
                ));
            }
            Err(env::VarError::NotUnicode(_)) => {
                return Err(invalid(
                    "TWO_EVENTSUB_RELOAD_KEY",
                    "value is not valid unicode",
                ));
            }
        };

        Ok(Some(Self {
            listen_addr,
            secret,
            reload_key,
        }))
    }

    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    #[must_use]
    pub fn secret(&self) -> &WebhookSecret {
        &self.secret
    }

    /// The refresh HMAC key, when rotation is armed. Expose only to the
    /// refresh verifier, never to diagnostics.
    #[must_use]
    pub fn reload_key(&self) -> Option<&Secret<Vec<u8>>> {
        self.reload_key.as_ref()
    }
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
            ("TWO_EVENTSUB".to_owned(), "1".to_owned()),
            ("TWO_EVENTSUB_BIND".to_owned(), "127.0.0.1:8092".to_owned()),
            (
                "TWO_TWITCH_WEBHOOK_SECRET".to_owned(),
                "synthetic-webhook-secret-01".to_owned(),
            ),
        ])
    }

    fn load(vars: &HashMap<String, String>) -> Result<Option<EventsubReceiverConfig>, ConfigError> {
        EventsubReceiverConfig::from_lookup(|name| {
            vars.get(name).cloned().ok_or(env::VarError::NotPresent)
        })
    }

    #[test]
    fn disabled_does_not_read_bind_or_secret() {
        for flag in [None, Some("0")] {
            assert!(EventsubReceiverConfig::from_lookup(|name| {
                assert_eq!(name, "TWO_EVENTSUB");
                flag.map(str::to_owned).ok_or(env::VarError::NotPresent)
            })
            .unwrap()
            .is_none());
        }
        for value in ["", "true", "false", " 1", "2"] {
            let mut vars = vars();
            vars.insert("TWO_EVENTSUB".to_owned(), value.to_owned());
            assert!(load(&vars).is_err());
        }
    }

    #[test]
    fn enabled_requires_bind_and_secret() {
        let mut vars = vars();
        vars.remove("TWO_EVENTSUB_BIND");
        assert!(load(&vars).is_err());
        let mut vars_no_secret = vars.clone();
        vars_no_secret.remove("TWO_TWITCH_WEBHOOK_SECRET");
        assert!(load(&vars_no_secret).is_err());
    }

    #[test]
    fn rejects_short_secret_and_zero_port() {
        let mut vars = vars();
        vars.insert("TWO_TWITCH_WEBHOOK_SECRET".to_owned(), "short".to_owned());
        assert!(load(&vars).is_err());
        let mut vars_bad_bind = vars.clone();
        vars_bad_bind.insert("TWO_EVENTSUB_BIND".to_owned(), "127.0.0.1:0".to_owned());
        assert!(load(&vars_bad_bind).is_err());
    }

    #[test]
    fn wildcard_needs_container_marker() {
        let mut vars = vars();
        vars.insert("TWO_EVENTSUB_BIND".to_owned(), "0.0.0.0:8092".to_owned());
        assert!(load(&vars).is_err());
        vars.insert("TWO_INTERNAL_CONTAINER".to_owned(), "1".to_owned());
        let config = load(&vars).unwrap().expect("enabled");
        assert_eq!(config.listen_addr().port(), 8092);
    }

    #[test]
    fn private_loopback_binds_without_marker() {
        let config = load(&vars()).unwrap().expect("enabled");
        assert_eq!(config.listen_addr(), "127.0.0.1:8092".parse().unwrap());
    }

    #[test]
    fn debug_never_carries_the_secret() {
        let config = load(&vars()).unwrap().expect("enabled");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("synthetic-webhook-secret-01"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn reload_key_is_optional_but_validated() {
        // Absent: legacy startup-secret behavior, reload disarmed.
        let config = load(&vars()).unwrap().expect("enabled");
        assert!(config.reload_key().is_none());
        // Present and long enough: armed, bytes intact.
        let mut configured_vars = vars();
        configured_vars.insert(
            "TWO_EVENTSUB_RELOAD_KEY".to_owned(),
            "synthetic-reload-key-00000000000001".to_owned(),
        );
        let config = load(&configured_vars).unwrap().expect("enabled");
        assert_eq!(
            config.reload_key().expect("armed").expose(),
            b"synthetic-reload-key-00000000000001"
        );
        // Present but short or empty: refuse startup, never silently disarm.
        let short_key = "x".repeat(MIN_KEY_SECRET_LEN - 1);
        for short in ["", "short", short_key.as_str()] {
            let mut vars = vars();
            vars.insert("TWO_EVENTSUB_RELOAD_KEY".to_owned(), short.to_owned());
            assert!(load(&vars).is_err(), "{short:?}");
        }
    }

    #[test]
    fn debug_never_carries_the_reload_key() {
        let mut vars = vars();
        vars.insert(
            "TWO_EVENTSUB_RELOAD_KEY".to_owned(),
            "synthetic-reload-key-00000000000001".to_owned(),
        );
        let config = load(&vars).unwrap().expect("enabled");
        let rendered = format!("{config:?} {config:#?}");
        assert!(!rendered.contains("synthetic-reload-key-00000000000001"));
        assert!(!rendered.contains("synthetic-webhook-secret-01"));
    }
}
