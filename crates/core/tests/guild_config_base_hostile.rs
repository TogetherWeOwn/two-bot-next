//! Hostile base-URL acceptance suite for the backup guild-config test seam.
//!
//! Pins `checked_base` as a closed allow-matrix, driven only through its
//! public signature: an explicit base is accepted only when it is an
//! `http`/`https` URL whose host is exactly `127.0.0.1`, exactly `[::1]`,
//! or exactly lowercase `localhost`, with no userinfo, no query, and no
//! fragment. Everything that merely resembles loopback — dotted suffixes,
//! trailing dots, case or percent-encoding escapes, Unicode lookalikes,
//! whitespace/control escapes, userinfo tricks, other schemes, and every
//! plain off-loopback host — is refused. `None` keeps returning the
//! production default verbatim, and refusals use fixed vocabulary: they
//! never echo the submitted URL, host, userinfo, query, or fragment.
//!
//! Pure and offline: no network, no database, no Discord client.

use std::collections::BTreeSet;

use two_bot_core::backup::guild_config_api::{checked_base, GuildConfigApiError};

const SEAM: &str = "GUILD_CONFIG_API_BASE";
const PRODUCTION: &str = "https://discord.com/api/v10";

fn checked(raw: &str) -> Result<String, GuildConfigApiError> {
    checked_base(Some(raw), SEAM, PRODUCTION)
}

fn assert_refused(raw: &str) -> GuildConfigApiError {
    match checked(raw) {
        Ok(accepted) => {
            panic!("hostile base must be refused: {raw:?} was accepted as {accepted:?}")
        }
        Err(error) => {
            // A refusal must never fall back to the production default, and
            // must not echo the submitted value back in either rendering.
            // (Empty/whitespace submissions carry nothing that could echo.)
            if !raw.trim().is_empty() {
                assert!(
                    !error.to_string().contains(raw),
                    "refusal Display echoes the submitted base {raw:?}: {error}"
                );
                assert!(
                    !format!("{error:?}").contains(raw),
                    "refusal Debug echoes the submitted base {raw:?}: {error:?}"
                );
            }
            error
        }
    }
}

// ---------------------------------------------------------------------------
// Green path: the loopback allow-matrix and the production default.
// ---------------------------------------------------------------------------

#[test]
fn none_returns_the_production_default_verbatim() {
    for production in [
        "https://discord.com/api/v10",
        "https://cdn.discordapp.com",
        "https://discord.com",
        "",
    ] {
        assert_eq!(
            checked_base(None, SEAM, production).unwrap(),
            production,
            "None must yield the production default unchanged"
        );
    }
}

#[test]
fn loopback_allow_matrix_passes_verbatim() {
    // (submitted, expected) — accepted bases are returned unchanged apart
    // from a stripped trailing slash, never rewritten to the default.
    let allowed = [
        ("http://127.0.0.1", "http://127.0.0.1"),
        ("http://127.0.0.1:9000", "http://127.0.0.1:9000"),
        ("https://127.0.0.1:8443", "https://127.0.0.1:8443"),
        ("http://[::1]", "http://[::1]"),
        ("http://[::1]:9", "http://[::1]:9"),
        ("https://[::1]:8443", "https://[::1]:8443"),
        ("http://localhost", "http://localhost"),
        ("http://localhost:9000", "http://localhost:9000"),
        ("https://localhost:8443", "https://localhost:8443"),
        (
            "http://localhost:9000/api/v10",
            "http://localhost:9000/api/v10",
        ),
        (
            "http://127.0.0.1:9000/api/v10",
            "http://127.0.0.1:9000/api/v10",
        ),
        ("http://localhost:9000/", "http://localhost:9000"),
        (
            "http://127.0.0.1:9000/api/v10/",
            "http://127.0.0.1:9000/api/v10",
        ),
    ];
    for (raw, expected) in allowed {
        assert_eq!(
            checked(raw).unwrap(),
            expected,
            "loopback base {raw:?} must be accepted"
        );
    }
}

#[test]
fn allow_and_refuse_hold_under_both_seam_names() {
    for seam in ["GUILD_CONFIG_API_BASE", "GUILD_CONFIG_CDN_BASE", "test"] {
        assert_eq!(
            checked_base(Some("http://localhost:9000"), seam, PRODUCTION).unwrap(),
            "http://localhost:9000",
            "loopback must pass for seam {seam}"
        );
        for raw in [
            "https://evil.example.com",
            "http://localhost.evil.example.com",
            "http://LOCALHOST:9000",
            "http://user:fixture-password@localhost:9000",
        ] {
            assert!(
                checked_base(Some(raw), seam, PRODUCTION).is_err(),
                "hostile base {raw:?} must be refused for seam {seam}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Red path: every hostile class is refused.
// ---------------------------------------------------------------------------

const OFF_LOOPBACK: &[&str] = &[
    "https://evil.example.com",
    "https://example.com",
    // The real Discord hosts are for the None default, never for an
    // explicit test-seam override.
    "https://discord.com",
    "https://discord.com/api/v10",
    "https://cdn.discordapp.com",
    "http://8.8.8.8:9000",
    "http://192.168.1.10:9000",
    "http://10.0.0.5:9000",
    "http://172.16.0.5:9000",
    // Cloud metadata and other special-use targets are not loopback.
    "http://169.254.169.254",
    "http://169.254.169.254:9000",
    "http://[fe80::1]:9000",
    "http://[2001:db8::1]:9000",
    "http://[::ffff:8.8.8.8]:9000",
    // Unspecified addresses are not in the allow-matrix either.
    "http://0.0.0.0:9000",
    "http://[::]:9000",
];

const LOOKALIKE_HOSTS: &[&str] = &[
    // Dotted suffix: loopback-looking prefix, attacker-controlled suffix.
    "http://localhost.evil.example.com:9000",
    "http://localhost.example.com:9000",
    "http://127.0.0.1.evil.example.com:9000",
    "http://127.0.0.1.example.com:9000",
    // Subdomains of localhost are not the exact allowlisted name.
    "http://sub.localhost:9000",
    "http://api.localhost.evil.example.com:9000",
    // DNS names that resolve to loopback only via public DNS trickery.
    "http://lvh.me:9000",
    "http://localtest.me:9000",
    "http://127.0.0.1.nip.io:9000",
    // Near-miss spellings.
    "http://localhost1:9000",
    "http://localhost-1:9000",
    "http://notlocalhost:9000",
    "http://mylocalhost.example.com:9000",
    "http://localhos:9000",
    "http://localhostt:9000",
    "http://local-host:9000",
    "http://local_host:9000",
];

const TRAILING_DOT_HOSTS: &[&str] = &[
    "http://localhost.:9000",
    "http://localhost.",
    "http://127.0.0.1.:9000",
    "http://127.0.0.1..:9000",
    "http://localhost..:9000",
    "https://discord.com.:9000",
    "http://evil.example.com.:9000",
];

const CASE_ESCAPES: &[&str] = &[
    "http://LOCALHOST:9000",
    "http://Localhost:9000",
    "https://lOcAlHoSt:8443",
    "http://LOCALHOST",
    "http://localhosT:9000",
];

const PERCENT_ENCODED_HOSTS: &[&str] = &[
    // Percent-encoded letters of "localhost".
    "http://l%6fcalhost:9000",
    "http://%6cocalhost:9000",
    "http://lo%63alhost:9000",
    "http://localhos%74:9000",
    "http://%6c%6f%63%61%6c%68%6f%73%74:9000",
    // Double encoding is still encoding.
    "http://%256cocalhost:9000",
    // Percent-encoded dots inside an IPv4-looking host.
    "http://127%2e0%2e0%2e1:9000",
    "http://127.0%2e0.1:9000",
    "http://127%2E0.0.1:9000",
    // Percent-encoded digits.
    "http://%31%32%37.0.0.1:9000",
    // Percent-encoded separators welded onto a lookalike host.
    "http://localhost%2e.evil.example.com:9000",
    "http://localhost%2eevil.example.com:9000",
    "http://localhost%40evil.example.com:9000",
    "http://localhost%2fevil.example.com:9000",
    "http://localhost%00.evil.example.com:9000",
];

const UNICODE_LOOKALIKES: &[&str] = &[
    // Cyrillic small letter o (U+043E) wearing a Latin costume.
    "http://l\u{43e}calhost:9000",
    // Fullwidth Latin small letter l (U+FF4C).
    "http://\u{ff4c}ocalhost:9000",
    // Ideographic full stop (U+3002) as an IPv4 separator.
    "http://127\u{3002}0\u{3002}0\u{3002}1:9000",
    // Fullwidth full stop (U+FF0E) as a label separator.
    "http://localhost\u{ff0e}evil.example.com:9000",
];

const WHITESPACE_AND_CONTROL_ESCAPES: &[&str] = &[
    " http://localhost:9000",
    "http://localhost:9000 ",
    "http://loca\thost:9000",
    "http://local\nhost:9000",
    "http://local\rhost:9000",
    "http://local host:9000",
    "http:// localhost:9000",
    "\u{1}http://localhost:9000",
];

const USERINFO_BASES: &[&str] = &[
    // Userinfo is refused outright, even when the real host is loopback.
    "http://user@localhost:9000",
    "http://user:fixture-password@127.0.0.1:9000",
    "http://user:fixture-password@[::1]:9000",
    "https://evil.example.com@localhost:9000",
    "http://:@localhost:9000",
    "http://@127.0.0.1:9000",
    // Loopback-looking userinfo hiding the real (remote) host.
    "http://localhost@evil.example.com:9000",
    "http://127.0.0.1:9@evil.example.com",
    "https://localhost:9@evil.example.com",
    "http://127.0.0.1@localhost.evil.example.com:9000",
];

const QUERY_AND_FRAGMENT_BASES: &[&str] = &[
    "http://localhost:9000?redirect=elsewhere",
    "http://127.0.0.1:9000?",
    "http://127.0.0.1:9?redirect=elsewhere",
    "http://[::1]:9000?x=1",
    "http://localhost:9000/api?x=1",
    "http://localhost:9000#fragment",
    "http://127.0.0.1:9#fragment",
    "http://[::1]:9000#",
    "http://localhost:9000?x=1#fragment",
];

const BAD_SCHEME_BASES: &[&str] = &[
    "ftp://localhost:9000",
    "ws://localhost:9000",
    "wss://127.0.0.1:9000",
    "file:///localhost",
    "file://localhost/etc/passwd",
    "gopher://localhost:9000",
    "httpsx://localhost:9000",
    // Scheme-relative and schemeless values are not http(s) bases.
    "//localhost:9000",
    "localhost:9000",
    "http:localhost:9000",
    "http:///localhost:9000",
    "://localhost:9000",
];

const MALFORMED_BASES: &[&str] = &[
    "",
    "   ",
    "not a url",
    "http://",
    "http://localhost:99999",
    "http://localhost:abc",
    "http://localhost:9000:9000",
    "http://[::1",
    "http://]localhost[:9000",
    "///",
];

#[test]
fn plain_off_loopback_hosts_are_refused() {
    for raw in OFF_LOOPBACK {
        assert_refused(raw);
    }
}

#[test]
fn lookalike_and_dotted_suffix_hosts_are_refused() {
    for raw in LOOKALIKE_HOSTS {
        assert_refused(raw);
    }
}

#[test]
fn trailing_dot_hosts_are_refused() {
    for raw in TRAILING_DOT_HOSTS {
        assert_refused(raw);
    }
}

#[test]
fn localhost_case_escapes_are_refused() {
    for raw in CASE_ESCAPES {
        assert_refused(raw);
    }
}

#[test]
fn percent_encoded_hosts_are_refused() {
    for raw in PERCENT_ENCODED_HOSTS {
        assert_refused(raw);
    }
}

#[test]
fn unicode_lookalike_hosts_are_refused() {
    for raw in UNICODE_LOOKALIKES {
        assert_refused(raw);
    }
}

#[test]
fn whitespace_and_control_escapes_are_refused() {
    for raw in WHITESPACE_AND_CONTROL_ESCAPES {
        assert_refused(raw);
    }
}

#[test]
fn userinfo_bases_are_refused() {
    for raw in USERINFO_BASES {
        assert_refused(raw);
    }
}

#[test]
fn query_and_fragment_bases_are_refused() {
    for raw in QUERY_AND_FRAGMENT_BASES {
        assert_refused(raw);
    }
}

#[test]
fn non_http_scheme_bases_are_refused() {
    for raw in BAD_SCHEME_BASES {
        assert_refused(raw);
    }
}

#[test]
fn malformed_bases_are_refused() {
    for raw in MALFORMED_BASES {
        assert_refused(raw);
    }
}

// ---------------------------------------------------------------------------
// Refusal contract: fixed vocabulary, no submitted-value echo.
// ---------------------------------------------------------------------------

#[test]
fn refusals_never_echo_host_userinfo_query_or_fragment_markers() {
    // Every distinguishing token contains "marker"; no refusal rendering
    // may contain any of them, the raw URL, or its credentials.
    let cases = [
        "https://marker-user:marker-password@marker-host.invalid:8443",
        "http://marker-user:marker-password@localhost:9000",
        "http://localhost.marker-host.invalid:9000",
        "http://markeruser@marker-host.invalid:9000",
        "http://localhost:9000?token=marker-query",
        "http://127.0.0.1:9000#marker-fragment",
        "http://localhost:9000/marker-path?x=marker-query",
        "ftp://marker-host.invalid:9000",
    ];
    for raw in cases {
        let error = assert_refused(raw);
        let display = error.to_string();
        let debug = format!("{error:?}");
        for token in [
            "marker",
            "marker-user",
            "marker-password",
            "marker-host.invalid",
        ] {
            assert!(
                !display.contains(token),
                "refusal Display echoes {token:?} for {raw:?}: {display}"
            );
            assert!(
                !debug.contains(token),
                "refusal Debug echoes {token:?} for {raw:?}: {debug}"
            );
        }
    }
}

#[test]
fn refusal_vocabulary_is_fixed_across_the_hostile_corpus() {
    // A hostile submission must not be able to mint novel error text: the
    // whole corpus collapses to a handful of fixed refusal messages.
    let corpus = OFF_LOOPBACK
        .iter()
        .chain(LOOKALIKE_HOSTS)
        .chain(TRAILING_DOT_HOSTS)
        .chain(CASE_ESCAPES)
        .chain(PERCENT_ENCODED_HOSTS)
        .chain(UNICODE_LOOKALIKES)
        .chain(WHITESPACE_AND_CONTROL_ESCAPES)
        .chain(USERINFO_BASES)
        .chain(QUERY_AND_FRAGMENT_BASES)
        .chain(BAD_SCHEME_BASES)
        .chain(MALFORMED_BASES);
    let mut distinct = BTreeSet::new();
    for raw in corpus {
        distinct.insert(assert_refused(raw).to_string());
    }
    assert!(
        distinct.len() <= 4,
        "refusals must be fixed-vocabulary, saw {} distinct messages: {distinct:?}",
        distinct.len()
    );
}

#[test]
fn hostile_corpus_size_stays_meaningful() {
    // Guards the suite itself: shrinking the corpus below a hostile
    // breadth floor is a visible test change, not a quiet edit.
    let total = OFF_LOOPBACK.len()
        + LOOKALIKE_HOSTS.len()
        + TRAILING_DOT_HOSTS.len()
        + CASE_ESCAPES.len()
        + PERCENT_ENCODED_HOSTS.len()
        + UNICODE_LOOKALIKES.len()
        + WHITESPACE_AND_CONTROL_ESCAPES.len()
        + USERINFO_BASES.len()
        + QUERY_AND_FRAGMENT_BASES.len()
        + BAD_SCHEME_BASES.len()
        + MALFORMED_BASES.len();
    assert!(total >= 100, "hostile corpus too small: {total} cases");
}
