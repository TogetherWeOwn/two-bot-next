//! Runtime configuration, loaded from the environment.
//!
//! Every secret (Discord token, database URL) arrives as a plain environment
//! variable injected by the Worker/Container wrapper; this crate never reads
//! secret files and never logs secret values.

use std::env;

use thiserror::Error;

use crate::Secret;

/// Configuration errors: messages never include secret values.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid {name}: {reason}")]
    Invalid { name: &'static str, reason: String },
}

/// Runtime configuration for the bot container.
#[derive(Debug, Clone)]
pub struct Config {
    /// Discord bot token. Required unless running health-only (S1 skeleton
    /// boots without it but reports the gateway as down on /readyz).
    pub discord_token: Option<Secret<String>>,
    /// Postgres connection URL (Neon, pooled via sqlx, max 5 — ADR 0001).
    pub database_url: Option<Secret<String>>,
    /// HTTP listen address for /health and /readyz. Defaults to 0.0.0.0:8080.
    pub listen_addr: String,
    /// Guild under management. Single-guild deployment (ADR 0001).
    pub guild_id: Option<u64>,
}

impl Config {
    /// Load configuration from the environment.
    ///
    /// Recognised variables: `DISCORD_TOKEN`, `DATABASE_URL`, `LISTEN_ADDR`,
    /// `GUILD_ID`. Missing optionals stay `None`; the /readyz gate reports
    /// which required pieces are absent instead of failing at boot (S1).
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(env::var)
    }

    fn from_lookup(
        mut lookup: impl FnMut(&'static str) -> Result<String, env::VarError>,
    ) -> Result<Self, ConfigError> {
        let guild_id = match lookup("GUILD_ID") {
            Ok(raw) => Some(raw.parse::<u64>().map_err(|_| ConfigError::Invalid {
                name: "GUILD_ID",
                reason: "expected a numeric Discord snowflake".to_owned(),
            })?),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::Invalid {
                    name: "GUILD_ID",
                    reason: "value is not valid unicode".to_owned(),
                });
            }
        };

        Ok(Self {
            discord_token: lookup("DISCORD_TOKEN").ok().map(Secret::new),
            database_url: lookup("DATABASE_URL").ok().map(Secret::new),
            listen_addr: lookup("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned()),
            guild_id,
        })
    }

    /// S1 skeleton gate: the bot can serve traffic when a token exists.
    /// Later slices add the database and gateway-session requirements.
    #[must_use]
    pub fn gateway_configured(&self) -> bool {
        self.discord_token
            .as_ref()
            .is_some_and(|t| !t.expose().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_vars(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(env::VarError::NotPresent)
        })
    }

    #[test]
    fn debug_redacts_token_and_entire_database_url() {
        let cfg = from_vars(&[
            ("DISCORD_TOKEN", "fixture-discord-token"),
            (
                "DATABASE_URL",
                "postgres://fixture-user:fixture-password@localhost/db?key=fixture-query",
            ),
        ])
        .unwrap();
        for output in [
            format!("{cfg:?}"),
            format!("{cfg:#?}"),
            format!("{:?}", cfg.clone()),
        ] {
            for secret in [
                "fixture-discord-token",
                "fixture-user",
                "fixture-password",
                "fixture-query",
                "postgres://",
            ] {
                assert!(!output.contains(secret));
            }
            assert!(output.contains("[REDACTED]"));
        }
    }

    #[test]
    fn defaults_without_env() {
        let cfg = from_vars(&[]).expect("defaults must parse");
        assert_eq!(cfg.listen_addr, "0.0.0.0:8080");
        assert!(cfg.discord_token.is_none());
        assert!(cfg.database_url.is_none());
        assert!(cfg.guild_id.is_none());
        assert!(!cfg.gateway_configured());
    }

    #[test]
    fn invalid_guild_id_rejected() {
        for value in ["not-a-snowflake", "", "-1", "18446744073709551616"] {
            let err = from_vars(&[("GUILD_ID", value)]).expect_err("invalid guild must fail");
            assert!(matches!(
                err,
                ConfigError::Invalid {
                    name: "GUILD_ID",
                    reason,
                } if reason == "expected a numeric Discord snowflake"
            ));
        }
    }

    #[test]
    fn configured_values_are_preserved() {
        let cfg = from_vars(&[
            ("DISCORD_TOKEN", "test-token"),
            ("DATABASE_URL", "test-database-url"),
            ("LISTEN_ADDR", "127.0.0.1:9090"),
            ("GUILD_ID", "18446744073709551615"),
        ])
        .expect("configured values must parse");
        assert_eq!(
            cfg.discord_token
                .as_ref()
                .map(|secret| secret.expose().as_str()),
            Some("test-token")
        );
        assert_eq!(
            cfg.database_url
                .as_ref()
                .map(|secret| secret.expose().as_str()),
            Some("test-database-url")
        );
        assert_eq!(cfg.listen_addr, "127.0.0.1:9090");
        assert_eq!(cfg.guild_id, Some(u64::MAX));
        assert!(cfg.gateway_configured());
    }

    #[test]
    fn empty_token_does_not_configure_gateway() {
        let cfg = from_vars(&[("DISCORD_TOKEN", "")]).expect("empty token must parse");
        assert_eq!(
            cfg.discord_token
                .as_ref()
                .map(|secret| secret.expose().as_str()),
            Some("")
        );
        assert!(!cfg.gateway_configured());
    }

    #[test]
    fn non_unicode_guild_id_rejected() {
        let err = Config::from_lookup(|_| Err(env::VarError::NotUnicode("fixture".into())))
            .expect_err("non-unicode guild must fail");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "GUILD_ID",
                reason,
            } if reason == "value is not valid unicode"
        ));
    }

    #[test]
    fn non_unicode_optionals_use_defaults() {
        let cfg = Config::from_lookup(|name| match name {
            "GUILD_ID" => Err(env::VarError::NotPresent),
            _ => Err(env::VarError::NotUnicode("fixture".into())),
        })
        .expect("non-unicode optionals must use defaults");
        assert!(cfg.discord_token.is_none());
        assert!(cfg.database_url.is_none());
        assert_eq!(cfg.listen_addr, "0.0.0.0:8080");
        assert!(cfg.guild_id.is_none());
        assert!(!cfg.gateway_configured());
    }
}
