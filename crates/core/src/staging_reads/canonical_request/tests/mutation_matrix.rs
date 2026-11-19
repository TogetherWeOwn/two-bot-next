//! Additional CR-C14N-1 section 7 Q-byte negatives. Transport and trusted state are separate.

use super::{decode_request, request_fixture, Verb, CHALLENGE, INCARNATION, MANIFEST};

const VERBS: [Verb; 5] = [
    Verb::Challenge,
    Verb::Begin,
    Verb::Read,
    Verb::Status,
    Verb::Close,
];

fn accepted_fixture(verb: Verb) -> &'static str {
    let fixture = request_fixture(verb);
    assert_eq!(
        decode_request(fixture.as_bytes()).map(|request| request.prefix.verb),
        Some(verb),
        "unmutated fixture"
    );
    fixture
}

fn assert_refused(verb: Verb, label: &str, bytes: &[u8]) {
    assert!(decode_request(bytes).is_none(), "{verb:?}: {label}");
}

fn replace_once(fixture: &str, from: &str, to: &str) -> String {
    assert_ne!(from, to, "mutation must change bytes");
    assert_eq!(fixture.match_indices(from).count(), 1, "target: {from:?}");
    fixture.replacen(from, to, 1)
}

fn reject_edit(verb: Verb, fixture: &str, from: &str, to: &str) {
    let mutated = replace_once(fixture, from, to);
    assert_refused(verb, &format!("{from:?} became {to:?}"), mutated.as_bytes());
}

fn field(key: &str, value: &str) -> String {
    format!(r#""{key}":"{value}""#)
}

fn reject_field(verb: Verb, fixture: &str, key: &str, old: &str, new: &str) {
    reject_edit(verb, fixture, &field(key, old), &field(key, new));
}

#[test]
fn refuses_late_field_and_byte_mutations_for_every_verb() {
    let digest = field("manifest_sha256", MANIFEST);
    let epoch = r#""owner_epoch":"1""#;
    for verb in VERBS {
        let fixture = accepted_fixture(verb);
        for (from, to) in [
            (epoch, r#""owner_epoch":"1","owner_epoch":"1""#),
            (epoch, r#""owner_epoch":"1","extra":"x""#),
            (
                r#""owner_epoch":"1","lifecycle_run_id":"100""#,
                r#""lifecycle_run_id":"100","owner_epoch":"1""#,
            ),
            (r#""owner_epoch""#, r#""owner_\u0065poch""#),
            (&format!(",{digest}"), ""),
            (&digest, r#""manifest_sha256":null"#),
            (epoch, r#""owner_epoch":true"#),
            (epoch, r#""owner_epoch":1"#),
        ] {
            reject_edit(verb, fixture, from, to);
        }
        for replacement in ["99\0-2", "99\x7f-2", "99é-2"] {
            reject_field(verb, fixture, "build_id", "99-2", replacement);
        }
        let mut invalid_utf8 = fixture.as_bytes().to_vec();
        let marker = r#""build_id":""#;
        let start = fixture.find(marker).expect("build field") + marker.len();
        invalid_utf8[start] = 0xff;
        assert_refused(verb, "invalid UTF-8 inside a field", &invalid_utf8);
        assert_refused(
            verb,
            "missing closing brace",
            &fixture.as_bytes()[..fixture.len() - 1],
        );
        assert_refused(verb, "trailing object", format!("{fixture}{{}}").as_bytes());
    }
}

#[test]
fn refuses_invalid_primitives_and_internal_identity_for_every_verb() {
    let long_id = "x".repeat(129);
    let candidate_sha = "b".repeat(40);
    let image_hash = "c".repeat(64);
    let image = format!("sha256:{image_hash}");
    let cases: &[(&str, &str, &[&str])] = &[
        ("event_name", "workflow_dispatch", &["push"]),
        ("candidate_run_id", "99", &["123456789012345678901"]),
        ("candidate_run_attempt", "2", &["0", "9007199254740992"]),
        (
            "candidate_sha",
            &candidate_sha,
            &[&candidate_sha[..39], &candidate_sha.to_ascii_uppercase()],
        ),
        (
            "image_digest",
            &image,
            &[
                &format!("sha512:{image_hash}"),
                &image[..image.len() - 1],
                &format!("sha256:{}", image_hash.to_ascii_uppercase()),
            ],
        ),
        ("build_id", "99-2", &["99/2", &long_id]),
        ("lifecycle_run_id", "100", &["101"]),
        ("lifecycle_run_attempt", "1", &["2"]),
        (
            "acquired_at_ms",
            "1794855600000",
            &["01794855600000", "1e0"],
        ),
        (
            "expires_at_ms",
            "1794858000000",
            &["1794855600000", "1794855599999"],
        ),
        (
            "manifest_sha256",
            MANIFEST,
            &[&MANIFEST.to_ascii_uppercase()],
        ),
        ("manifest_sha256", MANIFEST, &[&MANIFEST[..63]]),
    ];
    for verb in VERBS {
        let fixture = accepted_fixture(verb);
        for &(key, old, replacements) in cases {
            for new in replacements {
                reject_field(verb, fixture, key, old, new);
            }
        }

        // Preserve Worker/owner equality so the request reaches representation validation.
        let mut mutated = fixture.to_owned();
        for key in ["worker_version_id", "owner_deployment_id"] {
            mutated = replace_once(
                &mutated,
                &field(key, "fixture_deployment"),
                &field(key, &long_id),
            );
        }
        assert_refused(verb, "matching deployment IDs too long", mutated.as_bytes());
    }
}

#[test]
fn refuses_incompatible_known_verb_and_malformed_applicable_tail() {
    for (verb, name, incompatible) in [
        (Verb::Challenge, "challenge", "begin"),
        (Verb::Begin, "begin", "read"),
        (Verb::Read, "read", "status"),
        (Verb::Status, "status", "close"),
        (Verb::Close, "close", "challenge"),
    ] {
        let fixture = accepted_fixture(verb);
        reject_field(verb, fixture, "verb", name, incompatible);

        if verb != Verb::Challenge {
            for (key, old) in [("challenge", CHALLENGE), ("incarnation", INCARNATION)] {
                reject_field(verb, fixture, key, old, &old[..old.len() - 1]);
                reject_field(verb, fixture, key, old, &old.to_ascii_uppercase());
            }
        }

        let (from, to) = match verb {
            Verb::Read => (r#""index":"0""#, r#""index":0"#),
            Verb::Status => (r#""query":"measurement""#, r#""query":false"#),
            _ => continue,
        };
        reject_edit(verb, fixture, from, to);
    }
}
