//! Token-wide outbound admission, separate from inbound throttles and effect claims.
//!
//! Acquire immediately before each wire attempt. A permit has no drop-release:
//! cancellation/crash leaves an occupied row, and a take stamps the database
//! clock so a holder dead past [`IN_FLIGHT_LEASE_MS`] is reclaimed by the next
//! `admit` (fresh holders still block; indefinite/cooldown holds never
//! reclaim). Complete only after proving a definite effect/no-effect at the
//! mutation boundary, installing any 429 cooldown before releasing admission.
//! A complete 5xx or invalid success receipt is still uncertain. Reads have no
//! mutation effect. A permit is NOT an idempotency/execution lease.

use sha2::{Digest, Sha256};
use std::{fmt, future::Future, pin::Pin};

pub type AdmissionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ClaimFuture<'a> = AdmissionFuture<'a, Result<AdmissionPermit, AdmissionError>>;

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
    #[error("Discord send admission deadline expired")]
    Deadline,
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

    /// Bounded ordinary claim. Stores may recover a ready permit at cutoff
    /// only if no new claim write can start in that poll.
    fn admit_until(&self, at: tokio::time::Instant) -> ClaimFuture<'_> {
        Box::pin(async move { before_claim_cutoff(at, self.admit()).await? })
    }

    /// Strict admission must never fall back to an ordinary, lease-reclaimable claim.
    fn admit_strict(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { Err(AdmissionError::Configuration) })
    }
}

async fn before_claim_cutoff<F: Future>(
    at: tokio::time::Instant,
    work: F,
) -> Result<F::Output, AdmissionError> {
    let mut work = std::pin::pin!(work);
    let guarded = std::future::poll_fn(|cx| {
        if tokio::time::Instant::now() >= at {
            return std::task::Poll::Ready(Err(AdmissionError::Deadline));
        }
        work.as_mut().poll(cx).map(Ok)
    });
    tokio::time::timeout_at(at, guarded)
        .await
        .map_err(|_| AdmissionError::Deadline)?
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
        && match url.host() {
            Some(url::Host::Domain(host)) => host == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}

#[cfg(feature = "db")]
mod postgres;
#[cfg(feature = "db")]
pub use postgres::{PgSendAdmission, IN_FLIGHT_LEASE_MS};

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct OrdinaryOnly(TokenKey);

    impl SendAdmission for OrdinaryOnly {
        fn token_key(&self) -> &TokenKey {
            &self.0
        }

        fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
            Box::pin(async { Err(AdmissionError::Blocked) })
        }
    }

    #[tokio::test]
    async fn unsupported_strict_admission_fails_closed() {
        let gate = OrdinaryOnly(TokenKey::for_bot_token("fixture").unwrap());
        assert!(matches!(
            gate.admit_strict().await,
            Err(AdmissionError::Configuration)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn claim_wait_ready_at_cutoff_does_not_poll_write() {
        use tokio::time::{sleep_until, Duration, Instant};
        let at = Instant::now() + Duration::from_secs(1);
        let write = async {
            sleep_until(at).await;
            panic!("must not submit the write");
        };
        let result = before_claim_cutoff(at, write).await;
        assert!(matches!(result, Err(AdmissionError::Deadline)));
    }

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
        assert!(is_loopback_http("http://[::1]:1234"));
        assert!(is_loopback_http("http://localhost:1234"));
        for origin in [
            "http://[2001:db8::1]:1234",
            "http://secret@[::1]:1234",
            "http://user:secret@[::1]:1234",
            "https://[::1]:1234",
            "http://[::1]:1234/path",
            "http://[::1]:1234?secret=fixture",
            "http://[::1]:1234#fixture",
        ] {
            assert!(!is_loopback_http(origin));
        }
    }
}
