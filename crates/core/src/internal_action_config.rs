//! Explicit, private-only receiver configuration. Loading this does not approve
//! a deployment or provision keys. Call separately from health-only fallbacks:
//! an enabled but invalid receiver must stop startup, never silently downgrade.

use std::{collections::HashMap, env, net::SocketAddr};

use crate::{
    config::ConfigError,
    internal_actions::{assert_private_bind, build_channel_keys, parse_keys, KeyRing},
};

/// Validated configuration; no public constructor or secret-bearing Debug.
/// Caller identities, unlike signing-key IDs, must survive key rotation.
pub struct InternalActionConfig {
    listen_addr: SocketAddr,
    keys: KeyRing,
    callers: HashMap<String, String>,
    channel_keys: HashMap<String, String>,
}

impl std::fmt::Debug for InternalActionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalActionConfig")
            .field("listen_addr", &self.listen_addr)
            .field("key_count", &self.keys.len())
            .field("channel_count", &self.channel_keys.len())
            .finish_non_exhaustive()
    }
}

impl InternalActionConfig {
    /// Unset/`0` is dark. `1` requires every receiver-specific setting. No
    /// default endpoint, signing key, caller identity or channel is invented.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(env::var)
    }

    /// Validate explicit settings through the same parser, without mutating
    /// process environment. Useful for isolated configuration/receiver tests.
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(|name| vars.get(name).cloned().ok_or(env::VarError::NotPresent))
    }

    fn from_lookup(
        mut lookup: impl FnMut(&'static str) -> Result<String, env::VarError>,
    ) -> Result<Option<Self>, ConfigError> {
        match lookup("TWO_INTERNAL_ACTIONS") {
            Err(env::VarError::NotPresent) => return Ok(None),
            Ok(value) if value == "0" => return Ok(None),
            Ok(value) if value == "1" => {}
            _ => return Err(invalid("TWO_INTERNAL_ACTIONS", "expected 0 or 1")),
        }

        let bind = required(&mut lookup, "TWO_INTERNAL_BIND")?;
        let listen_addr: SocketAddr = bind
            .parse()
            .map_err(|_| invalid("TWO_INTERNAL_BIND", "expected a literal IP:port"))?;
        if listen_addr.ip().is_unspecified() {
            // TOG-16851: the Containers port check and containerFetch cannot
            // reach a loopback-only socket, so the Worker binds the wildcard
            // inside the private container network. A wildcard bind is accepted
            // only with the Worker-set container marker; everywhere else the
            // bind must stay a specific private IP.
            match lookup("TWO_INTERNAL_CONTAINER") {
                Ok(marker) if marker == "1" => {}
                _ => {
                    return Err(invalid(
                        "TWO_INTERNAL_BIND",
                        "expected a specific private IP",
                    ));
                }
            }
        } else {
            assert_private_bind(&listen_addr.ip().to_string())
                .map_err(|_| invalid("TWO_INTERNAL_BIND", "expected a specific private IP"))?;
        }
        if listen_addr.port() == 0 {
            return Err(invalid("TWO_INTERNAL_BIND", "expected a nonzero port"));
        }

        let keys = parse_keys(&required(&mut lookup, "TWO_INTERNAL_KEYS")?)
            // KeySpecError may contain an untrusted key ID. Do not forward it.
            .map_err(|_| invalid("TWO_INTERNAL_KEYS", "invalid signing-key specification"))?;
        if keys.len() > 64 {
            return Err(invalid("TWO_INTERNAL_KEYS", "at most 64 signing keys"));
        }
        let mut ids = std::collections::HashSet::new();
        for key in &keys {
            if !valid_name(&key.id) || !ids.insert(key.id.as_str()) {
                return Err(invalid("TWO_INTERNAL_KEYS", "invalid or duplicate key ID"));
            }
        }

        let callers = parse_callers(&required(&mut lookup, "TWO_INTERNAL_CALLERS")?)?;
        if callers.len() != keys.len() || keys.iter().any(|key| !callers.contains_key(&key.id)) {
            return Err(invalid(
                "TWO_INTERNAL_CALLERS",
                "map every signing key exactly once, with no extra entries",
            ));
        }
        // The key ID is not part of the signature. Reusing one secret across
        // different principals would let a caller select the other's identity.
        for (index, key) in keys.iter().enumerate() {
            if keys[..index].iter().any(|other| {
                key.secret.expose() == other.secret.expose()
                    && callers[&key.id] != callers[&other.id]
            }) {
                return Err(invalid(
                    "TWO_INTERNAL_KEYS",
                    "a signing secret cannot identify different callers",
                ));
            }
        }

        let channels = required(&mut lookup, "TWO_INTERNAL_CHANNEL_KEYS")?;
        let channel_keys = build_channel_keys(&channels).map_err(|_| {
            invalid(
                "TWO_INTERNAL_CHANNEL_KEYS",
                "invalid channel-key specification",
            )
        })?;
        let mut channel_names = std::collections::HashSet::new();
        for entry in channels.split(',') {
            let Some((name, id)) = entry.trim().split_once(':') else {
                return Err(invalid(
                    "TWO_INTERNAL_CHANNEL_KEYS",
                    "invalid channel-key entry",
                ));
            };
            if !valid_name(name.trim()) || id.contains(':') || !channel_names.insert(name.trim()) {
                return Err(invalid(
                    "TWO_INTERNAL_CHANNEL_KEYS",
                    "invalid or duplicate channel key",
                ));
            }
        }
        if channel_keys.is_empty()
            || channel_keys.values().any(|id| {
                id.parse::<u64>()
                    .ok()
                    .is_none_or(|value| value == 0 || value.to_string() != *id)
            })
        {
            return Err(invalid(
                "TWO_INTERNAL_CHANNEL_KEYS",
                "expected canonical nonzero Discord channel IDs",
            ));
        }

        Ok(Some(Self {
            listen_addr,
            keys: KeyRing::new(keys),
            callers,
            channel_keys,
        }))
    }

    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    #[must_use]
    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// Resolve only after signature verification. Never use a raw key ID as
    /// the durable caller identity: rotating that key must not bypass replay.
    #[must_use]
    pub fn caller_for(&self, key_id: &str) -> Option<&str> {
        self.callers.get(key_id).map(String::as_str)
    }

    #[must_use]
    pub fn channel_keys(&self) -> &HashMap<String, String> {
        &self.channel_keys
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

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn parse_callers(spec: &str) -> Result<HashMap<String, String>, ConfigError> {
    let mut callers = HashMap::new();
    for entry in spec.split(',') {
        let Some((key, caller)) = entry.trim().split_once(':') else {
            return Err(invalid(
                "TWO_INTERNAL_CALLERS",
                "expected key-id:caller entries",
            ));
        };
        let (key, caller) = (key.trim(), caller.trim());
        if !valid_name(key)
            || !valid_name(caller)
            || callers.insert(key.to_owned(), caller.to_owned()).is_some()
        {
            return Err(invalid(
                "TWO_INTERNAL_CALLERS",
                "invalid or duplicate caller mapping",
            ));
        }
    }
    Ok(callers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_secret(index: usize) -> String {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/internal-action-signing.json"
        ))
        .unwrap();
        fixture["vectors"][index]["secret"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn vars() -> HashMap<&'static str, String> {
        HashMap::from([
            ("TWO_INTERNAL_ACTIONS", "1".to_owned()),
            ("TWO_INTERNAL_BIND", "127.0.0.1:8091".to_owned()),
            ("TWO_INTERNAL_KEYS", format!("old:{}", fixture_secret(0))),
            ("TWO_INTERNAL_CALLERS", "old:website-staging".to_owned()),
            (
                "TWO_INTERNAL_CHANNEL_KEYS",
                "ann:333333333333333333".to_owned(),
            ),
        ])
    }

    fn load(
        vars: &HashMap<&'static str, String>,
    ) -> Result<Option<InternalActionConfig>, ConfigError> {
        InternalActionConfig::from_lookup(|name| {
            vars.get(name).cloned().ok_or(env::VarError::NotPresent)
        })
    }

    #[test]
    fn disabled_does_not_read_keys_or_bind() {
        for flag in [None, Some("0")] {
            assert!(InternalActionConfig::from_lookup(|name| {
                assert_eq!(name, "TWO_INTERNAL_ACTIONS");
                flag.map(str::to_owned).ok_or(env::VarError::NotPresent)
            })
            .unwrap()
            .is_none());
        }
        for value in ["", "true", "false", " 1", "2"] {
            let mut vars = vars();
            vars.insert("TWO_INTERNAL_ACTIONS", value.to_owned());
            assert!(load(&vars).is_err());
        }
    }

    #[test]
    fn enabled_requires_all_settings_and_unicode() {
        for name in vars().keys() {
            let mut missing = vars();
            missing.remove(name);
            if *name == "TWO_INTERNAL_ACTIONS" {
                assert!(load(&missing).unwrap().is_none());
                continue;
            }
            assert!(load(&missing).is_err(), "{name}");
            missing.insert(name, "".to_owned());
            assert!(load(&missing).is_err(), "{name}");
        }
        let vars = vars();
        for bad_name in vars.keys() {
            assert!(InternalActionConfig::from_lookup(|name| {
                if name == *bad_name {
                    Err(env::VarError::NotUnicode("secret-sentinel".into()))
                } else {
                    vars.get(name).cloned().ok_or(env::VarError::NotPresent)
                }
            })
            .is_err());
        }
    }

    #[test]
    fn requires_specific_private_literal_and_fixed_port() {
        let mut vars = vars();
        for addr in [
            "0.0.0.0:8091",
            "[::]:8091",
            "8.8.8.8:8091",
            "[2001:4860:4860::8888]:8091",
            "localhost:8091",
            "127.0.0.1:0",
            "127.0.0.1",
            "http://127.0.0.1:8091",
        ] {
            vars.insert("TWO_INTERNAL_BIND", addr.to_owned());
            assert!(load(&vars).is_err(), "{addr}");
        }
        for addr in [
            "127.0.0.1:8091",
            "10.0.0.2:8091",
            "[::1]:8091",
            "[fd00::2]:8091",
        ] {
            vars.insert("TWO_INTERNAL_BIND", addr.to_owned());
            assert_eq!(
                load(&vars).unwrap().unwrap().listen_addr().to_string(),
                addr
            );
        }
    }

    #[test]
    fn wildcard_bind_requires_the_container_marker() {
        // TOG-16851: the Worker binds 0.0.0.0 inside the private container
        // network because the port check cannot reach loopback. The marker is
        // Worker-set; without exactly "1" the wildcard stays refused.
        for addr in ["0.0.0.0:8091", "[::]:8091"] {
            let mut vars = vars();
            vars.insert("TWO_INTERNAL_BIND", addr.to_owned());
            assert!(load(&vars).is_err(), "{addr} without the marker");
            for marker in ["", "0", "true", " 1", "cloudflare"] {
                let mut marked = vars.clone();
                marked.insert("TWO_INTERNAL_CONTAINER", marker.to_owned());
                assert!(load(&marked).is_err(), "{addr} with {marker:?}");
            }
            let mut marked = vars.clone();
            marked.insert("TWO_INTERNAL_CONTAINER", "1".to_owned());
            assert_eq!(
                load(&marked).unwrap().unwrap().listen_addr().to_string(),
                addr,
                "{addr} with the marker"
            );
        }
        // The marker changes nothing for a specific private bind.
        let mut vars = vars();
        vars.insert("TWO_INTERNAL_CONTAINER", "1".to_owned());
        assert_eq!(
            load(&vars).unwrap().unwrap().listen_addr().to_string(),
            "127.0.0.1:8091"
        );
    }

    #[test]
    fn rotation_preserves_caller_identity_without_secret_debug() {
        let mut vars = vars();
        let secret = fixture_secret(0);
        let new_secret = fixture_secret(1);
        assert_ne!(secret, new_secret);
        vars.insert(
            "TWO_INTERNAL_KEYS",
            format!("old:{secret},new:{new_secret}"),
        );
        vars.insert(
            "TWO_INTERNAL_CALLERS",
            "old:website-staging,new:website-staging".to_owned(),
        );
        let config = load(&vars).unwrap().unwrap();
        assert_eq!(config.caller_for("old"), config.caller_for("new"));
        assert_eq!(config.caller_for("new"), Some("website-staging"));
        assert_eq!(config.caller_for("unknown"), None);
        assert_eq!(config.keys().len(), 2);
        assert_eq!(config.channel_keys()["ann"], "333333333333333333");
        let debug = format!("{config:?}");
        assert!(!debug.contains(&secret));
        assert!(!debug.contains(&new_secret));
        assert!(!debug.contains("website-staging"));
        // Rotation uses distinct secrets even when the logical caller is the
        // same. The shared parser refuses aliases before caller resolution.
        vars.insert("TWO_INTERNAL_KEYS", format!("old:{secret},new:{secret}"));
        for callers in [
            "old:website-staging,new:website-staging",
            "old:website-staging,new:other-caller",
        ] {
            vars.insert("TWO_INTERNAL_CALLERS", callers.to_owned());
            let error = load(&vars).unwrap_err();
            let debug = format!("{error:?} {error}");
            assert!(!debug.contains(&secret));
            assert!(!debug.contains("website-staging"));
            assert!(!debug.contains("other-caller"));
        }
    }

    #[test]
    fn rejects_ambiguous_keys_and_callers_without_echoing_input() {
        let mut vars = vars();
        for callers in [
            "old:a,old:b",
            "old:a,extra:a",
            "missing:a",
            "old:",
            "old:a:b",
            "old:bad/name",
        ] {
            vars.insert("TWO_INTERNAL_CALLERS", callers.to_owned());
            assert!(load(&vars).is_err());
        }
        vars.insert("TWO_INTERNAL_CALLERS", "old:website-staging".to_owned());
        let secret = fixture_secret(0);
        for spec in [
            format!("old:{secret},old:{secret}"),
            "secret-sentinel:short".to_owned(),
            format!("bad/name:{secret}"),
        ] {
            vars.insert("TWO_INTERNAL_KEYS", spec.clone());
            let error = load(&vars).unwrap_err();
            assert!(!format!("{error:?} {error}").contains(&spec));
            assert!(!format!("{error:?} {error}").contains("secret-sentinel"));
        }
    }

    #[test]
    fn channels_are_explicit_unique_canonical_snowflakes() {
        let mut vars = vars();
        for spec in [
            "",
            "ann:333333333333333333,ann:444444444444444444",
            "ann:00000000000000000",
            "ann:033333333333333333",
            "ann:18446744073709551616",
            "ann:42",
            "ann:333333333333333333,",
            "ann:333333333333333333:ignored-suffix",
            "bad/name:333333333333333333",
        ] {
            vars.insert("TWO_INTERNAL_CHANNEL_KEYS", spec.to_owned());
            assert!(load(&vars).is_err(), "{spec}");
        }
    }
}
