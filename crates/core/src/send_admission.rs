//! Token-wide outbound admission, separate from inbound throttles and effect claims.
//!
//! Acquire immediately before each wire attempt. A permit has no expiry and no
//! drop-release: cancellation/crash leaves an occupied row pending reconciliation.
//! Finish only after observing a complete exchange, installing any 429 cooldown
//! before releasing admission. A permit is NOT an idempotency/execution lease.

use sha2::{Digest, Sha256};
use std::{fmt, future::Future, pin::Pin};

pub type AdmissionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Domain-separated fingerprint of the canonical Bot authorization credential.
/// Never accept caller-selected namespaces or store/log the credential itself.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenKey(String);

impl TokenKey {
    pub fn for_bot_token(token: &str) -> Result<Self, AdmissionError> {
        let token = token.strip_prefix("Bot ").unwrap_or(token);
        if token.is_empty()
            || token
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(AdmissionError::Configuration);
        }
        let mut hash = Sha256::new();
        hash.update(b"two-bot-next/discord-bot-token/v1\0");
        hash.update(token.as_bytes());
        Ok(Self(hex::encode(hash.finalize())))
    }
}

impl fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenKey([redacted])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("Discord send admission is blocked")]
    Blocked,
    #[error("Discord send admission storage unavailable")]
    Storage,
    #[error("Discord send admission configuration invalid")]
    Configuration,
    #[error("Discord send admission claim is stale")]
    StaleClaim,
}

/// All channel/bucket cooldowns are conservatively promoted to token-wide.
/// Finite waits are never capped downward. Missing timing is indefinite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendCooldown {
    FiniteMs(u64),
    Indefinite,
}

/// Take the longest usable header/body delay, not the legacy capped sleep.
#[must_use]
pub fn cooldown_from_delays(header: Option<f64>, body: Option<f64>) -> SendCooldown {
    fn millis(seconds: f64) -> Option<u64> {
        let ms = (seconds * 1000.0).ceil();
        (seconds.is_finite() && seconds >= 0.0 && ms < u64::MAX as f64).then_some(ms as u64)
    }
    match header.and_then(millis).max(body.and_then(millis)) {
        Some(ms) => SendCooldown::FiniteMs(ms),
        None => SendCooldown::Indefinite,
    }
}

pub trait SendAdmission: Send + Sync + fmt::Debug {
    fn token_key(&self) -> &TokenKey;
    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>>;
}

pub trait SendCompletion: Send {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>>;
}

/// Consuming completion prevents reuse; dropping does nothing intentionally.
pub struct AdmissionPermit(Box<dyn SendCompletion>);

impl AdmissionPermit {
    #[must_use]
    pub fn new(completion: Box<dyn SendCompletion>) -> Self {
        Self(completion)
    }

    pub async fn complete(self, cooldown: Option<SendCooldown>) -> Result<(), AdmissionError> {
        self.0.complete(cooldown).await
    }
}

/// Unguarded fixtures can target only explicit loopback HTTP, never Discord.
#[must_use]
pub fn is_loopback_http(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

#[cfg(feature = "db")]
mod postgres;
#[cfg(feature = "db")]
pub use postgres::PgSendAdmission;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_namespace_and_closed_delay_rules() {
        assert_eq!(
            TokenKey::for_bot_token("fixture"),
            TokenKey::for_bot_token("Bot fixture")
        );
        assert_ne!(
            TokenKey::for_bot_token("fixture"),
            TokenKey::for_bot_token("other")
        );
        assert!(TokenKey::for_bot_token("Bot ").is_err());
        assert!(TokenKey::for_bot_token("fixture\n").is_err());
        assert_eq!(
            format!("{:?}", TokenKey::for_bot_token("fixture").unwrap()),
            "TokenKey([redacted])"
        );
        assert_eq!(
            cooldown_from_delays(Some(100.0), Some(0.1)),
            SendCooldown::FiniteMs(100_000)
        );
        assert_eq!(
            cooldown_from_delays(None, Some(-1.0)),
            SendCooldown::Indefinite
        );
        assert_eq!(
            cooldown_from_delays(Some(f64::INFINITY), None),
            SendCooldown::Indefinite
        );
        assert_eq!(
            cooldown_from_delays(None, Some(0.0001)),
            SendCooldown::FiniteMs(1)
        );
        assert!(is_loopback_http("http://127.0.0.1:1234"));
        assert!(!is_loopback_http("https://discord.com"));
        assert!(!is_loopback_http("http://localhost.evil:1234"));
        assert!(!is_loopback_http("http://secret@localhost:1234"));
    }
}
