//! Pure feed-fetch policy. The transport must dial the validated socket addresses,
//! not resolve the hostname a second time, and must disable proxies/auto-redirects.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use thiserror::Error;
use url::{Host, Url};

pub const MAX_FEED_BYTES: usize = 2_000_000;
pub const MAX_FEED_SOURCE_BYTES: usize = 2048;
pub const MAX_REDIRECT_HOPS: usize = 3;
pub const FEED_READ_TIMEOUT_MS: u64 = 15_000;
pub const FEED_USER_AGENT: &str = "Owen/1.0 (+https://two.gg)";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error("Feed source must be an HTTPS URL on port 443, without credentials, and at most 2048 bytes.")]
    InvalidSource,
    #[error("Feed source must resolve only to public IP addresses.")]
    NonPublicAddress,
    #[error("Feed redirect refused.")]
    RedirectRefused,
    #[error("Feed is larger than 2 MB.")]
    TooLarge,
    #[error("Feed body is not UTF-8.")]
    InvalidEncoding,
    #[error("Feed returned an unsupported content type.")]
    UnsupportedContentType,
}

pub fn validate_source(source: &str) -> Result<Url, FetchError> {
    if source.len() > MAX_FEED_SOURCE_BYTES {
        return Err(FetchError::InvalidSource);
    }
    let url = Url::parse(source.trim()).map_err(|_| FetchError::InvalidSource)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
        || url.port_or_known_default() != Some(443)
        || url.as_str().len() > MAX_FEED_SOURCE_BYTES
    {
        return Err(FetchError::InvalidSource);
    }
    match url.host() {
        Some(Host::Ipv4(ip)) if !is_public_address(ip.into()) => {
            return Err(FetchError::NonPublicAddress)
        }
        Some(Host::Ipv6(ip)) if !is_public_address(ip.into()) => {
            return Err(FetchError::NonPublicAddress)
        }
        Some(Host::Domain(host))
            if host.trim_end_matches('.').eq_ignore_ascii_case("localhost") =>
        {
            return Err(FetchError::NonPublicAddress)
        }
        _ => {}
    }
    Ok(url)
}

pub fn is_public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let blocked = [
                (Ipv4Addr::new(0, 0, 0, 0), 8),
                (Ipv4Addr::new(10, 0, 0, 0), 8),
                (Ipv4Addr::new(100, 64, 0, 0), 10),
                (Ipv4Addr::new(127, 0, 0, 0), 8),
                (Ipv4Addr::new(169, 254, 0, 0), 16),
                (Ipv4Addr::new(172, 16, 0, 0), 12),
                (Ipv4Addr::new(192, 0, 0, 0), 24),
                (Ipv4Addr::new(192, 0, 2, 0), 24),
                (Ipv4Addr::new(192, 88, 99, 0), 24),
                (Ipv4Addr::new(192, 168, 0, 0), 16),
                (Ipv4Addr::new(198, 18, 0, 0), 15),
                (Ipv4Addr::new(198, 51, 100, 0), 24),
                (Ipv4Addr::new(203, 0, 113, 0), 24),
                (Ipv4Addr::new(224, 0, 0, 0), 4),
                (Ipv4Addr::new(240, 0, 0, 0), 4),
            ];
            !blocked.iter().any(|(base, prefix)| {
                let mask = u32::MAX << (32 - prefix);
                u32::from(ip) & mask == u32::from(*base) & mask
            })
        }
        IpAddr::V6(ip) => {
            let bits = u128::from(ip);
            // Fail closed outside global unicast; includes mapped IPv4, NAT64,
            // ULA, link-local, multicast and IPv4-compatible representations.
            if bits >> 125 != 1 {
                return false;
            }
            let blocked = [
                (0x2001_u128 << 112, 23),
                (0x2001_0db8_u128 << 96, 32),
                (0x2002_u128 << 112, 16),
                (0x3fff_u128 << 112, 20),
            ];
            !blocked.iter().any(|(base, prefix)| {
                let mask = u128::MAX << (128 - prefix);
                bits & mask == base & mask
            })
        }
    }
}

/// No credentials and no second DNS lookup: preserve URL hostname for TLS/SNI
/// and Host, but connect ONLY to these socket addresses. Rebuild per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicRequest {
    url: Url,
    addresses: Vec<SocketAddr>,
}

impl PublicRequest {
    pub fn prepare(url: Url, resolved: &[IpAddr]) -> Result<Self, FetchError> {
        validate_source(url.as_str())?;
        if resolved.is_empty() || resolved.iter().any(|ip| !is_public_address(*ip)) {
            return Err(FetchError::NonPublicAddress);
        }
        let literal = match url.host() {
            Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        if literal.is_some_and(|ip| resolved.iter().any(|resolved| *resolved != ip)) {
            return Err(FetchError::NonPublicAddress);
        }
        let port = url
            .port_or_known_default()
            .ok_or(FetchError::InvalidSource)?;
        Ok(Self {
            url,
            addresses: resolved
                .iter()
                .map(|ip| SocketAddr::new(*ip, port))
                .collect(),
        })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

/// Relative/same-host redirects only, with a fresh PublicRequest at every hop.
pub fn redirect_target(current: &Url, location: &str, hops: usize) -> Result<Url, FetchError> {
    if hops >= MAX_REDIRECT_HOPS || location.trim().is_empty() {
        return Err(FetchError::RedirectRefused);
    }
    let target = current
        .join(location)
        .map_err(|_| FetchError::RedirectRefused)?;
    validate_source(target.as_str()).map_err(|_| FetchError::RedirectRefused)?;
    if current.host() != target.host() {
        return Err(FetchError::RedirectRefused);
    }
    Ok(target)
}

pub fn validate_content_type(content_type: Option<&str>) -> Result<(), FetchError> {
    let kind = content_type.unwrap_or("").trim().to_ascii_lowercase();
    if kind.is_empty() || kind.contains("xml") || kind.contains("rss") || kind.contains("atom") {
        Ok(())
    } else {
        Err(FetchError::UnsupportedContentType)
    }
}

/// Feed chunks must be *decompressed* bytes. Check before allocating/copying;
/// the adapter must drop/cancel the response stream on any error.
#[derive(Debug, Default)]
pub struct LimitedBody {
    bytes: Vec<u8>,
}

impl LimitedBody {
    pub fn new(content_length: Option<u64>) -> Result<Self, FetchError> {
        if content_length.is_some_and(|size| size > MAX_FEED_BYTES as u64) {
            return Err(FetchError::TooLarge);
        }
        Ok(Self::default())
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), FetchError> {
        if chunk.len() > MAX_FEED_BYTES - self.bytes.len() {
            return Err(FetchError::TooLarge);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub fn finish(self) -> Result<String, FetchError> {
        String::from_utf8(self.bytes).map_err(|_| FetchError::InvalidEncoding)
    }
}
