//! Runtime configuration, loaded from the environment.
//!
//! Every secret (Discord token, database URL) arrives as a plain environment
//! variable injected by the Worker/Container wrapper; this crate never reads
//! secret files and never logs secret values.

use std::env;

use thiserror::Error;

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
    pub discord_token: Option<String>,
    /// Postgres connection URL (Neon, pooled via sqlx, max 5 — ADR 0001).
    pub database_url: Option<String>,
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
        let guild_id = match env::var("GUILD_ID") {
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
            discord_token: env::var("DISCORD_TOKEN").ok(),
            database_url: env::var("DATABASE_URL").ok(),
            listen_addr: env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned()),
            guild_id,
        })
    }

    /// S1 skeleton gate: the bot can serve traffic when a token exists.
    /// Later slices add the database and gateway-session requirements.
    #[must_use]
    pub fn gateway_configured(&self) -> bool {
        self.discord_token.as_ref().is_some_and(|t| !t.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn defaults_without_env() {
        let _guard = ENV_LOCK.lock().expect("test environment lock");
        for key in ["DISCORD_TOKEN", "DATABASE_URL", "LISTEN_ADDR", "GUILD_ID"] {
            // SAFETY: environment tests serialize mutations under ENV_LOCK.
            unsafe { env::remove_var(key) };
        }
        let cfg = Config::from_env().expect("defaults must parse");
        assert_eq!(cfg.listen_addr, "0.0.0.0:8080");
        assert!(cfg.discord_token.is_none());
        assert!(cfg.guild_id.is_none());
        assert!(!cfg.gateway_configured());
    }

    #[test]
    fn invalid_guild_id_rejected() {
        let _guard = ENV_LOCK.lock().expect("test environment lock");
        // SAFETY: environment tests serialize mutations under ENV_LOCK.
        unsafe { env::set_var("GUILD_ID", "not-a-snowflake") };
        let err = Config::from_env().expect_err("non-numeric guild must fail");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "GUILD_ID",
                ..
            }
        ));
        // SAFETY: cleanup so later tests see a clean environment.
        unsafe { env::remove_var("GUILD_ID") };
    }
}
