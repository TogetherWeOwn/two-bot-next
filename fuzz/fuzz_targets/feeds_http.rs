#![no_main]

//! Arbitrary feed-fetch policy inputs: source URLs, redirect targets,
//! content types and body bytes.
//!
//! Field layout is NUL-separated `source\0location\0content-type`; the body
//! seam uses the raw input bytes so invalid UTF-8 stays reachable through
//! lossy conversion on the string seams. No network, database, Discord
//! client or secret is touched here; only these pure validators run.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use libfuzzer_sys::fuzz_target;
use two_bot_core::feeds_http::{
    is_public_address, redirect_target, validate_content_type, validate_source, LimitedBody,
    MAX_FEED_BYTES, MAX_FEED_SOURCE_BYTES, MAX_REDIRECT_HOPS,
};

/// Allowed media types, written from the documented feed contract rather than
/// by calling the production validator, so agreement proves the rule holds.
fn oracle_media_allowed(media: &str) -> bool {
    matches!(
        media,
        "application/rss+xml"
            | "application/x-rss+xml"
            | "text/rss+xml"
            | "application/atom+xml"
            | "application/x-atom+xml"
            | "application/rdf+xml"
            | "application/xml"
            | "text/xml"
    )
}

fn check_source(text: &str) {
    let Ok(url) = validate_source(text) else {
        return;
    };
    // Acceptance implies the full source contract.
    assert!(
        text.len() <= MAX_FEED_SOURCE_BYTES,
        "overlong source accepted"
    );
    assert!(
        url.as_str().len() <= MAX_FEED_SOURCE_BYTES,
        "overlong canonical source accepted"
    );
    assert_eq!(url.scheme(), "https", "non-https source accepted");
    assert!(url.username().is_empty(), "credentialed source accepted");
    assert!(url.password().is_none(), "credentialed source accepted");
    assert!(url.host().is_some(), "hostless source accepted");
    assert_eq!(
        url.port_or_known_default(),
        Some(443),
        "non-443 source accepted"
    );
    // An accepted literal must be public; an accepted domain must not be
    // localhost (trailing-dot and case-insensitive, like production).
    // Match the typed host: `Host` displays IPv6 with brackets, so a
    // string round-trip through `IpAddr::from_str` never parses IPv6.
    match url.host().expect("host checked") {
        url::Host::Ipv4(ip) => {
            assert!(
                is_public_address(ip.into()),
                "private literal accepted: {text:?}"
            );
        }
        url::Host::Ipv6(ip) => {
            assert!(
                is_public_address(ip.into()),
                "private literal accepted: {text:?}"
            );
        }
        url::Host::Domain(host) => {
            assert!(
                !host.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
                "localhost accepted: {text:?}"
            );
        }
    }
}

fn check_public(ip: IpAddr) {
    let first = is_public_address(ip);
    assert_eq!(first, is_public_address(ip), "nondeterministic {ip}");
    // The obvious std refusals must never read as public. The production
    // blocklist is wider (CGNAT, TEST-NET, NAT64, 6to4, documentation); the
    // unit suite pins those ranges exactly.
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
            {
                assert!(!first, "private IPv4 accepted: {ip}");
            }
        }
        IpAddr::V6(v6) => {
            // ULA (fc00::/7) and link-local (fe80::/10) read from the first
            // segment so the ipv6-link-local seed can actually fail.
            let unique_local = (v6.segments()[0] & 0xfe00) == 0xfc00;
            let link_local = (v6.segments()[0] & 0xffc0) == 0xfe80;
            if v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || unique_local
                || link_local
            {
                assert!(!first, "non-unicast IPv6 accepted: {ip}");
            }
        }
    }
}

fn check_content_type(text: &str) {
    let result = validate_content_type(Some(text));
    let media = text
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let expected = text.trim().is_empty() || oracle_media_allowed(&media);
    assert_eq!(
        result.is_ok(),
        expected,
        "content-type oracle diverged for {text:?}"
    );
}

fn check_body(data: &[u8], content_length: Option<u64>) {
    match LimitedBody::new(content_length) {
        Err(_) => assert!(
            content_length.is_some_and(|size| size > MAX_FEED_BYTES as u64),
            "bounded length refused"
        ),
        Ok(mut body) => {
            for chunk in data.chunks(1024) {
                if body.push(chunk).is_err() {
                    break;
                }
            }
            let _ = body.finish();
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // Fixed matrix, independent of the fuzzer input: boundary refusals and
    // acceptances stay reachable instead of starving behind random bytes.
    assert!(LimitedBody::new(Some(MAX_FEED_BYTES as u64 + 1)).is_err());
    assert!(LimitedBody::new(Some(MAX_FEED_BYTES as u64)).is_ok());
    assert!(validate_content_type(None).is_ok());
    assert!(validate_content_type(Some("")).is_ok());
    assert!(validate_source("https://example.org:443/feed").is_ok());
    assert!(validate_source("https://localhost/").is_err());
    let base = validate_source("https://example.org/old").expect("fixed base is valid");
    assert!(redirect_target(&base, "/new", MAX_REDIRECT_HOPS).is_err());

    // Arbitrary bytes reach the string seams through lossy conversion, never
    // dropped, so hostile bytes stay reachable.
    let text = String::from_utf8_lossy(data);
    let mut parts = text.split('\0');
    let source = parts.next().unwrap_or("");
    let location = parts.next().unwrap_or("");
    let content_type = parts.next().unwrap_or("");

    for candidate in [source, &text] {
        check_source(candidate);
    }
    for field in text.split('\0').take(8) {
        check_source(field);
        check_content_type(field);
        if let Ok(ip) = field.trim().parse::<IpAddr>() {
            check_public(ip);
        }
    }
    check_content_type(content_type);
    check_content_type(&text);

    // Byte-derived socket addresses: the first windows cover IPv4 and IPv6
    // without waiting for the fuzzer to spell a printable literal.
    if data.len() >= 4 {
        check_public(IpAddr::V4(Ipv4Addr::new(
            data[0], data[1], data[2], data[3],
        )));
    }
    if data.len() >= 8 {
        check_public(IpAddr::V4(Ipv4Addr::new(
            data[4], data[5], data[6], data[7],
        )));
    }
    if data.len() >= 16 {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&data[..16]);
        check_public(IpAddr::V6(Ipv6Addr::from(octets)));
    }

    // Redirects: the low hops byte covers both valid (< 3) and overflow
    // (>= 3) hops on every input; a fuzzed valid base keeps the arbitrary
    // current-URL path reachable alongside the fixed base.
    let hops = usize::from(data.first().copied().unwrap_or(0)) % 8;
    for target in [location, &text] {
        if let Ok(url) = redirect_target(&base, target, hops) {
            assert!(hops < MAX_REDIRECT_HOPS);
            assert_eq!(base.host(), url.host(), "cross-host redirect accepted");
            assert!(validate_source(url.as_str()).is_ok());
        }
    }
    if let Ok(fuzz_base) = validate_source(source) {
        if let Ok(url) = redirect_target(&fuzz_base, location, hops) {
            assert_eq!(fuzz_base.host(), url.host());
            assert!(validate_source(url.as_str()).is_ok());
        }
    }

    // Bodies: a byte-derived content length keeps the oversized-header path
    // reachable; chunked pushes prove the cap counts decompressed bytes.
    let content_length = if data.len() >= 8 {
        let mut wire = [0u8; 8];
        wire.copy_from_slice(&data[..8]);
        Some(u64::from_le_bytes(wire))
    } else {
        None
    };
    check_body(data, content_length);
    check_body(data, None);
});
