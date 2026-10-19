//! Fuzz-derived regressions for the feed-fetch policy.
//!
//! Mirrors the `fuzz/feeds_http` harness field layout (`source\0location\0
//! content-type`, body from raw bytes): every input below is an arbitrary
//! byte/URL shape that must never panic, OOM or hang, and malicious sources
//! and redirect targets must refuse. These pins stay even though the fuzzer
//! itself only runs off-controller.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use two_bot_core::feeds_http::{
    is_public_address, redirect_target, validate_content_type, validate_source, FetchError,
    LimitedBody, MAX_FEED_BYTES, MAX_FEED_SOURCE_BYTES, MAX_REDIRECT_HOPS,
};

fn base_url() -> url::Url {
    validate_source("https://example.org/old").expect("fixed base is valid")
}

#[test]
fn fuzz_arbitrary_bytes_never_panic_and_stay_bounded() {
    let long_bytes = vec![b'a'; 70_000];
    let hostile: &[&[u8]] = &[
        b"",
        b"\x00",
        b"\xff\xfe\x00\x80",
        b"https://example.org/feed\x00/new\x00application/atom+xml",
        b"https://10.0.0.1/\x00https://127.0.0.1/\x00text/html",
        b"\n\r\t ",
        &long_bytes,
        "https://example.org/😀".as_bytes(),
        b"https://user:pass@example.org/",
        b"javascript:alert(1)",
    ];
    for raw in hostile {
        let text = String::from_utf8_lossy(raw);
        for candidate in text.split('\0').chain(std::iter::once(text.as_ref())) {
            let _ = validate_source(candidate);
            let _ = validate_content_type(Some(candidate));
            if let Ok(ip) = candidate.trim().parse::<IpAddr>() {
                let first = is_public_address(ip);
                assert_eq!(first, is_public_address(ip), "nondeterministic {ip}");
            }
        }
        let hops = usize::from(raw.first().copied().unwrap_or(0)) % 8;
        for location in text.split('\0') {
            let result = redirect_target(&base_url(), location, hops);
            if let Ok(target) = result {
                assert!(hops < MAX_REDIRECT_HOPS);
                assert_eq!(base_url().host(), target.host());
                assert!(validate_source(target.as_str()).is_ok());
            }
        }
        let content_len = if raw.len() >= 8 {
            let mut wire = [0u8; 8];
            wire.copy_from_slice(&raw[..8]);
            Some(u64::from_le_bytes(wire))
        } else {
            None
        };
        match LimitedBody::new(content_len) {
            Err(_) => assert!(content_len.is_some_and(|n| n > MAX_FEED_BYTES as u64)),
            Ok(mut body) => {
                for chunk in raw.chunks(1024) {
                    if body.push(chunk).is_err() {
                        break;
                    }
                }
                let _ = body.finish();
            }
        }
    }
    // Byte-derived IPs never panic and agree with the obvious std refusals.
    for window in [0u8, 1, 10, 127, 169, 192, 224, 255] {
        let inner = Ipv4Addr::new(window, 0, 0, 1);
        let v4 = IpAddr::V4(inner);
        let first = is_public_address(v4);
        assert_eq!(first, is_public_address(v4));
        if inner.is_private()
            || inner.is_loopback()
            || inner.is_link_local()
            || inner.is_multicast()
        {
            assert!(!first, "accepted {v4}");
        }
    }
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    assert!(!is_public_address(v6));
}

#[test]
fn fuzz_malicious_sources_refuse() {
    let long_source = format!("https://example.org/{}", "a".repeat(3000));
    for source in [
        "https://10.0.0.1/",
        "https://127.0.0.1/",
        "https://[::1]/",
        "https://[::ffff:7f00:1]/",
        "https://localhost/",
        "https://LOCALHOST./",
        "https://2130706433/",
        "https://0x7f000001/",
        "https://127.1/",
        "http://example.org/",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://user@example.org/",
        "https://example.org:8443/",
        long_source.as_str(),
    ] {
        assert!(
            validate_source(source).is_err(),
            "accepted malicious source {source:?}"
        );
    }
    // Control bytes in the path stay same-host under `Url` (percent-encoded
    // or stripped) and still satisfy the source contract, so they belong to
    // the no-panic property above, not to this refusal pin. Pin the behavior:
    // if accepted, the host is still the public base host.
    for raw in [
        "https://example.org/\u{0}/feed",
        "https://example.org/\u{7f}",
    ] {
        if let Ok(url) = validate_source(raw) {
            assert_eq!(url.host_str(), Some("example.org"));
            assert_eq!(url.scheme(), "https");
        }
    }
    // At-limit still accepts; one byte over refuses.
    let base = "https://example.org/";
    let at_limit = format!("{base}{}", "a".repeat(MAX_FEED_SOURCE_BYTES - base.len()));
    assert!(validate_source(&at_limit).is_ok());
    let over = format!(
        "{base}{}",
        "a".repeat(MAX_FEED_SOURCE_BYTES - base.len() + 1)
    );
    assert_eq!(validate_source(&over), Err(FetchError::InvalidSource));
}

#[test]
fn fuzz_redirect_targets_hold_same_host() {
    let current = base_url();
    let long_location = "a".repeat(70_000);
    for location in [
        "https://127.0.0.1/",
        "https://[::1]/",
        "https://10.0.0.1/",
        "https://www.example.org/",
        "http://example.org/",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://user@example.org/",
        "//other.example/",
        "",
        "   ",
        long_location.as_str(),
    ] {
        assert!(
            redirect_target(&current, location, 0).is_err(),
            "accepted redirect {location:?}"
        );
    }
    // A NUL in a relative path stays same-host, so it is not a refusal case:
    // if accepted, the target keeps the base host and validates as a source.
    if let Ok(nul) = redirect_target(&current, "/\u{0}/evil", 0) {
        assert_eq!(current.host(), nul.host());
        assert!(validate_source(nul.as_str()).is_ok());
    }
    assert!(redirect_target(&current, "/new", MAX_REDIRECT_HOPS).is_err());
    assert!(redirect_target(&current, "/new", MAX_REDIRECT_HOPS + 1).is_err());
    let target = redirect_target(&current, "/new", 0).expect("relative same-host");
    assert_eq!(target.as_str(), "https://example.org/new");
    let target = redirect_target(&current, "https://example.org/other", 2).expect("same-host");
    assert!(validate_source(target.as_str()).is_ok());
}

#[test]
fn fuzz_content_type_parameter_injection_refused() {
    for valid in [
        "application/rss+xml",
        "application/atom+xml; charset=UTF-8",
        "text/xml",
        "  application/rss+xml  ;  charset=utf-8  ",
    ] {
        assert!(
            validate_content_type(Some(valid)).is_ok(),
            "rejected {valid:?}"
        );
    }
    let long_type = "a".repeat(70_000);
    for hostile in [
        "text/html",
        "text/html; foo=atom",
        "text/html; foo=x-rss+xml",
        "application/json; xml=true",
        "image/svg+xml",
        "; foo=atom",
        "text/html\x00application/rss+xml",
        long_type.as_str(),
    ] {
        assert!(
            validate_content_type(Some(hostile)).is_err(),
            "accepted hostile type {hostile:?}"
        );
    }
    assert!(validate_content_type(None).is_ok());
    assert!(validate_content_type(Some("   ")).is_ok());
}

#[test]
fn fuzz_limited_body_fails_bounded() {
    assert!(matches!(
        LimitedBody::new(Some(MAX_FEED_BYTES as u64 + 1)),
        Err(FetchError::TooLarge)
    ));
    assert!(LimitedBody::new(Some(MAX_FEED_BYTES as u64)).is_ok());
    let mut body = LimitedBody::new(None).expect("unbounded start");
    body.push(&vec![b'a'; MAX_FEED_BYTES]).expect("exact cap");
    assert_eq!(body.push(b"b"), Err(FetchError::TooLarge));
    // Split multi-byte sequence across pushes reassembles.
    let mut body = LimitedBody::default();
    body.push(&[0xe2]).unwrap();
    body.push(&[0x82, 0xac]).unwrap();
    assert_eq!(body.finish().unwrap(), "€");
    // Invalid UTF-8 surfaces as encoding failure, not panic or OOM.
    let mut body = LimitedBody::default();
    body.push(&[0xff, 0xfe]).unwrap();
    assert_eq!(body.finish(), Err(FetchError::InvalidEncoding));
    // Empty chunks never grow the body.
    let mut body = LimitedBody::new(None).unwrap();
    body.push(&[]).unwrap();
    assert_eq!(body.finish().unwrap(), "");
}
