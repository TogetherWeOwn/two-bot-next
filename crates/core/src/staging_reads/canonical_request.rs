//! Inert canonical request assembly. This does not authenticate or admit a request.
//!
//! Trusted provenance, authentication and transport remain separate.

use sha2::{Digest, Sha256};

use super::{
    canonical_candidate::{encode_candidate, RequestCandidate},
    canonical_lifecycle::{encode_lifecycle, RequestLifecycle},
    canonical_owner::{encode_owner, RequestOwner},
    canonical_prefix::{encode_prefix, RequestPrefix},
    canonical_tail::{encode_tail, RequestTail},
    canonical_target::{encode_target, RequestTarget},
    canonical_time::{encode_time, RequestTime},
};

/// Untrusted fields for a complete request; equality is not trusted provenance.
pub struct CanonicalRequest<'a> {
    pub prefix: RequestPrefix<'a>,
    pub candidate: RequestCandidate<'a>,
    pub owner: RequestOwner<'a>,
    pub lifecycle: RequestLifecycle<'a>,
    pub time: RequestTime<'a>,
    pub target: RequestTarget<'a>,
    pub tail: RequestTail<'a>,
}

/// Validate and assemble the canonical object, without authorizing any action.
pub fn encode_request(input: &CanonicalRequest<'_>) -> Option<String> {
    if input.prefix.verb != input.tail.verb
        || input.prefix.caller_run_id != input.lifecycle.lifecycle_run_id
        || input.prefix.caller_run_attempt != input.lifecycle.lifecycle_run_attempt
        || input.candidate.worker_version_id != input.owner.owner_deployment_id
    {
        return None;
    }
    let mut result = encode_prefix(&input.prefix)?;
    result.push_str(&encode_candidate(&input.candidate)?);
    result.push_str(&encode_owner(&input.owner)?);
    result.push_str(&encode_lifecycle(&input.lifecycle)?);
    result.push_str(&encode_time(&input.time)?);
    result.push_str(&encode_target(&input.target)?);
    result.push_str(&encode_tail(&input.tail)?);
    Some(result)
}

/// Decode only canonical Q bytes. This does not authenticate or admit a request.
/// Challenge envelopes, HTTP framing, status query decoding and provenance are separate.
pub fn decode_request(bytes: &[u8]) -> Option<CanonicalRequest<'_>> {
    use super::canonical_tail::Verb;

    if bytes.len() > 16_384 || !bytes.is_ascii() {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut cursor = Cursor(text);
    cursor.literal("{\"v\":\"1\",\"verb\":")?;
    let verb = match cursor.string()? {
        "challenge" => Verb::Challenge,
        "begin" => Verb::Begin,
        "read" => Verb::Read,
        "status" => Verb::Status,
        "close" => Verb::Close,
        _ => return None,
    };
    cursor.literal(concat!(
        ",\"environment\":\"staging\",\"repository_id\":\"1396224242\",",
        "\"repository_owner_id\":\"319943574\",\"workflow_ref\":",
        "\"TogetherWeOwn/two-bot-next/.github/workflows/deploy-production.yml@refs/heads/main\""
    ))?;
    let prefix = RequestPrefix {
        verb,
        tooling_sha: cursor.field("tooling_sha")?,
        event_name: cursor.field("event_name")?,
        caller_run_id: cursor.field("caller_run_id")?,
        caller_run_attempt: cursor.field("caller_run_attempt")?,
    };
    let candidate = RequestCandidate {
        candidate_run_id: cursor.field("candidate_run_id")?,
        candidate_run_attempt: cursor.field("candidate_run_attempt")?,
        candidate_sha: cursor.field("candidate_sha")?,
        build_id: cursor.field("build_id")?,
        worker_version_id: cursor.field("worker_version_id")?,
        image_digest: cursor.field("image_digest")?,
    };
    let owner = RequestOwner {
        owner_deployment_id: cursor.field("owner_deployment_id")?,
        owner_epoch: cursor.field("owner_epoch")?,
    };
    let lifecycle = RequestLifecycle {
        lifecycle_run_id: cursor.field("lifecycle_run_id")?,
        lifecycle_run_attempt: cursor.field("lifecycle_run_attempt")?,
    };
    let time = RequestTime {
        acquired_at_ms: cursor.field("acquired_at_ms")?,
        expires_at_ms: cursor.field("expires_at_ms")?,
    };
    let target = RequestTarget {
        staging_application_id: cursor.field("staging_application_id")?,
        staging_guild_id: cursor.field("staging_guild_id")?,
    };
    let challenge = cursor.nullable_field("challenge")?;
    let incarnation = cursor.nullable_field("incarnation")?;
    let manifest_sha256 = cursor.field("manifest_sha256")?;
    let index = cursor
        .nullable_field("index")?
        .map(str::parse)
        .transpose()
        .ok()?;
    let query = cursor.nullable_field("query")?;
    cursor.literal("}")?;
    if !cursor.0.is_empty() {
        return None;
    }
    let request = CanonicalRequest {
        prefix,
        candidate,
        owner,
        lifecycle,
        time,
        target,
        tail: RequestTail {
            verb,
            challenge,
            incarnation,
            manifest_sha256,
            index,
            query,
        },
    };
    (encode_request(&request)?.as_bytes() == bytes).then_some(request)
}

/// Hash strictly canonical Q bytes with the request domain, without authorization.
/// Manifest validation, interoperability, authentication, accounting and runtime
/// integration remain separate obligations; this function does not admit a request.
pub fn request_digest(bytes: &[u8]) -> Option<String> {
    decode_request(bytes)?;
    let mut hash = Sha256::new();
    hash.update(b"two.bot.staging-reads.request.v1\n");
    hash.update(bytes);
    Some(hex::encode(hash.finalize()))
}

struct Cursor<'a>(&'a str);

impl<'a> Cursor<'a> {
    fn literal(&mut self, expected: &str) -> Option<()> {
        self.0 = self.0.strip_prefix(expected)?;
        Some(())
    }

    fn string(&mut self) -> Option<&'a str> {
        self.literal("\"")?;
        let end = self.0.find('"')?;
        let value = &self.0[..end];
        if value.bytes().any(|b| b <= b' ' || b == b'\\' || b == 0x7f) {
            return None;
        }
        self.0 = &self.0[end + 1..];
        Some(value)
    }

    fn key(&mut self, name: &str) -> Option<()> {
        self.literal(",\"")?;
        self.literal(name)?;
        self.literal("\":")
    }

    fn field(&mut self, name: &str) -> Option<&'a str> {
        self.key(name)?;
        self.string()
    }

    fn nullable_field(&mut self, name: &str) -> Option<Option<&'a str>> {
        self.key(name)?;
        if let Some(rest) = self.0.strip_prefix("null") {
            self.0 = rest;
            Some(None)
        } else {
            Some(Some(self.string()?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging_reads::canonical_tail::Verb;

    const MANIFEST: &str = "0f1f9821354531dc6aedd4ee10c3ddc365e0032ac557aace69a1a76d7fbd6e1c";
    const CHALLENGE: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const INCARNATION: &str = "000000010000000200000003ffffffff";

    fn request(verb: Verb) -> CanonicalRequest<'static> {
        CanonicalRequest {
            prefix: RequestPrefix {
                verb,
                tooling_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                event_name: "workflow_dispatch",
                caller_run_id: "100",
                caller_run_attempt: "1",
            },
            candidate: RequestCandidate {
                candidate_run_id: "99",
                candidate_run_attempt: "2",
                candidate_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                build_id: "99-2",
                worker_version_id: "fixture_deployment",
                image_digest:
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            },
            owner: RequestOwner {
                owner_deployment_id: "fixture_deployment",
                owner_epoch: "1",
            },
            lifecycle: RequestLifecycle {
                lifecycle_run_id: "100",
                lifecycle_run_attempt: "1",
            },
            time: RequestTime {
                acquired_at_ms: "1794855600000",
                expires_at_ms: "1794858000000",
            },
            target: RequestTarget {
                staging_application_id: "1469137636663758888",
                staging_guild_id: "1545644954272137297",
            },
            tail: RequestTail {
                verb,
                challenge: (verb != Verb::Challenge).then_some(CHALLENGE),
                incarnation: (verb != Verb::Challenge).then_some(INCARNATION),
                manifest_sha256: MANIFEST,
                index: (verb == Verb::Read).then_some(0),
                query: (verb == Verb::Status).then_some("measurement"),
            },
        }
    }

    // These files are the exact synthetic packet bytes, with no final LF.
    const BEGIN: &str = include_str!("../../tests/fixtures/counted_read_begin.json");
    const READ: &str = include_str!("../../tests/fixtures/counted_read_read.json");
    const REQUEST_VECTORS: [(Verb, &str, usize, &str); 5] = [
        (
            Verb::Challenge,
            include_str!("../../tests/fixtures/counted_read_challenge.json"),
            1025,
            "ebb43f5d18e6bdc4dbe2ba88f7718e91f9a3511744ef783e362acf21c1c4204d",
        ),
        (
            Verb::Begin,
            BEGIN,
            1113,
            "7382de091efcf9cadbc53e0a5a579ef7a203a3d31424ef8bb79bb1444a86cbf9",
        ),
        (
            Verb::Read,
            READ,
            1111,
            "26eb806bdc541bdb431e37bdf373a7c56fb5cb1ab62d004674094ab413aa429f",
        ),
        (
            Verb::Status,
            include_str!("../../tests/fixtures/counted_read_status.json"),
            1123,
            "cd08bf49580dd2f80df6b53c28e517abf5073ab654cf001e517677c9c3d0ae1e",
        ),
        (
            Verb::Close,
            include_str!("../../tests/fixtures/counted_read_close.json"),
            1113,
            "35f99ad1084416feb0a915d20ee1675cdb88acfa2bbf26bd132daaa8f91936b3",
        ),
    ];

    #[test]
    fn contract_request_vectors() {
        for (verb, expected, length, _) in REQUEST_VECTORS {
            assert_eq!(expected.len(), length, "{verb:?}");
            assert_eq!(
                encode_request(&request(verb)).as_deref(),
                Some(expected),
                "{verb:?}"
            );
            let decoded = decode_request(expected.as_bytes()).expect("canonical vector");
            let input = request(verb);
            assert_eq!(decoded.prefix.verb, verb);
            assert_eq!(decoded.tail.verb, verb);
            assert_eq!(decoded.tail.challenge, input.tail.challenge);
            assert_eq!(decoded.tail.incarnation, input.tail.incarnation);
            assert_eq!(decoded.tail.manifest_sha256, MANIFEST);
            assert_eq!(decoded.tail.index, input.tail.index);
            assert_eq!(decoded.tail.query, input.tail.query);
            assert_eq!(
                encode_request(&decoded).as_deref(),
                Some(expected),
                "{verb:?}"
            );
        }
    }

    #[test]
    fn request_domain_digest_vectors_and_wrong_domains() {
        for (verb, fixture, _, expected) in REQUEST_VECTORS {
            let bytes = fixture.as_bytes();
            let digest = request_digest(bytes).expect("canonical vector digest");
            assert_eq!(digest, expected, "{verb:?}");
            assert_ne!(digest, hex::encode(Sha256::digest(bytes)));
            let mut manifest_domain = Sha256::new();
            manifest_domain.update(b"two.bot.staging-reads.manifest.v1\n");
            manifest_domain.update(bytes);
            assert_ne!(digest, hex::encode(manifest_domain.finalize()));
        }
        assert_eq!(
            hex::encode(Sha256::digest(BEGIN.as_bytes())),
            "ca42a52aa0bce2edd1750b03192ebf73008ed7b101eb75db9adfe9e06a847975"
        );
    }

    #[test]
    fn request_digest_refuses_noncanonical_exact_vectors() {
        for (label, invalid, length, expected_hash) in [
            (
                "N1-final-LF",
                format!("{BEGIN}\n"),
                1114,
                "f6d9714b2b91ddcffb6cb78da4b56fae860ca57791450667a6eb9ab82044d478",
            ),
            (
                "N2-escaped-v",
                BEGIN.replacen(r#""v":"1""#, r#""v":"\u0031""#, 1),
                1118,
                "58cfe5a37b206022e1b61b5cf943e7c05e81b4009ecb74a21eb38c0aa1cad4e0",
            ),
            (
                "N3-numeric-epoch",
                BEGIN.replacen(r#""owner_epoch":"1""#, r#""owner_epoch":1"#, 1),
                1111,
                "680f1dceb6686cd1d2e52bb18f72b4debd43e5214e5b3a80f65ee686038fc623",
            ),
            (
                "N4-duplicate-v",
                BEGIN.replacen('{', r#"{"v":"1","#, 1),
                1121,
                "0722d1252ae714fa811e50781597338430c27c26fcd79e8d009e3aca3644440f",
            ),
            (
                "N5-space",
                BEGIN.replacen('{', "{ ", 1),
                1114,
                "3b9bb442eebf088e5884d52d90125ea71c71756669cfcb313ed389ef52ef5d62",
            ),
        ] {
            let bytes = invalid.as_bytes();
            assert_eq!(bytes.len(), length, "{label}");
            // A raw domain hash exists even for malformed bytes; the protocol API must refuse.
            let mut hash = Sha256::new();
            hash.update(b"two.bot.staging-reads.request.v1\n");
            hash.update(bytes);
            assert_eq!(hex::encode(hash.finalize()), expected_hash, "{label}");
            assert!(decode_request(bytes).is_none(), "{label}");
            assert!(request_digest(bytes).is_none(), "{label}");
        }
        assert!(request_digest(&vec![b' '; 16_385]).is_none());
        assert!(request_digest(&[0xff]).is_none());
    }

    #[test]
    fn decoder_rejects_noncanonical_and_invalid_inputs_for_every_verb() {
        for (verb, valid, _, _) in REQUEST_VECTORS {
            let mutations = [
                valid.replacen("\"v\":\"1\",", "", 1),
                valid.replacen("\"v\":\"1\",", "\"v\":\"1\",\"v\":\"1\",", 1),
                valid.replacen("\"v\":\"1\",", "\"extra\":\"x\",\"v\":\"1\",", 1),
                valid.replacen(
                    &format!(
                        "\"v\":\"1\",\"verb\":\"{}\"",
                        match verb {
                            Verb::Challenge => "challenge",
                            Verb::Begin => "begin",
                            Verb::Read => "read",
                            Verb::Status => "status",
                            Verb::Close => "close",
                        }
                    ),
                    &format!(
                        "\"verb\":\"{}\",\"v\":\"1\"",
                        match verb {
                            Verb::Challenge => "challenge",
                            Verb::Begin => "begin",
                            Verb::Read => "read",
                            Verb::Status => "status",
                            Verb::Close => "close",
                        }
                    ),
                    1,
                ),
                valid.replacen(":", ": ", 1),
                format!("{valid}\n"),
                format!("\u{feff}{valid}"),
                valid.replacen("\"v\":\"1\"", "\"v\":\"\\u0031\"", 1),
                valid.replacen("\"v\":\"1\"", "\"v\":1", 1),
                valid.replacen("\"caller_run_id\":\"100\"", "\"caller_run_id\":\"0100\"", 1),
                valid.replacen(
                    "\"owner_epoch\":\"1\"",
                    "\"owner_epoch\":\"9007199254740992\"",
                    1,
                ),
                valid.replacen(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                    1,
                ),
                valid.replacen(
                    "fixture_deployment\",\"owner_epoch",
                    "other\",\"owner_epoch",
                    1,
                ),
            ];
            for invalid in mutations {
                assert_ne!(invalid, valid);
                assert!(decode_request(invalid.as_bytes()).is_none(), "{invalid}");
            }
            let common = [
                ("\"v\":\"1\"", "\"v\":\"2\""),
                ("\"build_id\":\"99-2\"", "\"build_id\":\"\\u00399-2\""),
                (
                    "\"environment\":\"staging\"",
                    "\"environment\":\"production\"",
                ),
                (
                    "\"repository_id\":\"1396224242\"",
                    "\"repository_id\":\"1396224243\"",
                ),
                (
                    "\"repository_owner_id\":\"319943574\"",
                    "\"repository_owner_id\":\"319943575\"",
                ),
                (
                    "deploy-production.yml@refs/heads/main",
                    "deploy-production.yml@refs/heads/other",
                ),
                (
                    "\"staging_application_id\":\"1469137636663758888\"",
                    "\"staging_application_id\":\"1\"",
                ),
                (
                    "\"staging_guild_id\":\"1545644954272137297\"",
                    "\"staging_guild_id\":\"1\"",
                ),
                (
                    "\"caller_run_id\":\"100\"",
                    "\"caller_run_id\":\"123456789012345678901\"",
                ),
                (
                    "\"caller_run_attempt\":\"1\"",
                    "\"caller_run_attempt\":\"0\"",
                ),
                (
                    "\"caller_run_attempt\":\"1\"",
                    "\"caller_run_attempt\":\"9007199254740992\"",
                ),
                (
                    "\"caller_run_attempt\":\"1\"",
                    "\"caller_run_attempt\":\"1e0\"",
                ),
                (
                    "\"caller_run_attempt\":\"1\"",
                    "\"caller_run_attempt\":\"+1\"",
                ),
                (
                    "\"caller_run_attempt\":\"1\"",
                    "\"caller_run_attempt\":\"1.0\"",
                ),
                ("\"owner_epoch\":\"1\"", "\"owner_epoch\":\"0\""),
                ("\"owner_epoch\":\"1\"", "\"owner_epoch\":\"01\""),
                (
                    "\"expires_at_ms\":\"1794858000000\"",
                    "\"expires_at_ms\":\"9007199254740992\"",
                ),
                ("\"build_id\":\"99-2\"", "\"build_id\":\"unknown\""),
                (
                    "\"worker_version_id\":\"fixture_deployment\"",
                    "\"worker_version_id\":\"bad/version\"",
                ),
            ];
            for (from, to) in common {
                let invalid = valid.replacen(from, to, 1);
                assert_ne!(invalid, valid, "missing mutation target: {from}");
                assert!(decode_request(invalid.as_bytes()).is_none(), "{invalid}");
            }
            for (first, second, old, new) in [
                (
                    "caller_run_id",
                    "lifecycle_run_id",
                    "100",
                    "123456789012345678901",
                ),
                ("caller_run_attempt", "lifecycle_run_attempt", "1", "0"),
                (
                    "caller_run_attempt",
                    "lifecycle_run_attempt",
                    "1",
                    "9007199254740992",
                ),
                ("caller_run_attempt", "lifecycle_run_attempt", "1", "1e0"),
                ("caller_run_attempt", "lifecycle_run_attempt", "1", "+1"),
                ("caller_run_attempt", "lifecycle_run_attempt", "1", "1.0"),
                (
                    "worker_version_id",
                    "owner_deployment_id",
                    "fixture_deployment",
                    "bad/version",
                ),
            ] {
                let mut invalid = valid.to_owned();
                for field in [first, second] {
                    let from = format!("\"{field}\":\"{old}\"");
                    let to = format!("\"{field}\":\"{new}\"");
                    let changed = invalid.replacen(&from, &to, 1);
                    assert_ne!(changed, invalid, "missing mutation target: {from}");
                    invalid = changed;
                }
                assert!(decode_request(invalid.as_bytes()).is_none(), "{invalid}");
            }
            let verb_name = match verb {
                Verb::Challenge => "challenge",
                Verb::Begin => "begin",
                Verb::Read => "read",
                Verb::Status => "status",
                Verb::Close => "close",
            };
            let mut cases = vec![(
                format!("\"verb\":\"{verb_name}\""),
                "\"verb\":\"unknown\"".to_owned(),
            )];
            for field in ["challenge", "incarnation", "index", "query"] {
                let value = match field {
                    "challenge" if verb != Verb::Challenge => format!("\"{CHALLENGE}\""),
                    "incarnation" if verb != Verb::Challenge => format!("\"{INCARNATION}\""),
                    "index" if verb == Verb::Read => "\"0\"".to_owned(),
                    "query" if verb == Verb::Status => "\"measurement\"".to_owned(),
                    _ => "null".to_owned(),
                };
                let from = format!(",\"{field}\":{value}");
                cases.push((from.clone(), String::new()));
                let wrong = if value == "null" {
                    match field {
                        "challenge" => format!("\"{CHALLENGE}\""),
                        "incarnation" => format!("\"{INCARNATION}\""),
                        "index" => "\"0\"".to_owned(),
                        _ => "\"measurement\"".to_owned(),
                    }
                } else {
                    "null".to_owned()
                };
                cases.push((from, format!(",\"{field}\":{wrong}")));
            }
            if verb != Verb::Challenge {
                cases.push((CHALLENGE.to_owned(), "0".repeat(64)));
                cases.push((INCARNATION.to_owned(), "0".repeat(32)));
            }
            if verb == Verb::Read {
                cases.push(("\"index\":\"0\"".to_owned(), "\"index\":\"-1\"".to_owned()));
            }
            if verb == Verb::Status {
                cases.push((
                    "\"query\":\"measurement\"".to_owned(),
                    "\"query\":\"other\"".to_owned(),
                ));
            }
            for (from, to) in cases {
                let invalid = valid.replacen(&from, &to, 1);
                assert_ne!(invalid, valid, "missing mutation target: {from}");
                assert!(decode_request(invalid.as_bytes()).is_none(), "{invalid}");
            }
            assert!(decode_request(&[0xff]).is_none());
            assert!(decode_request(&vec![b' '; 16_385]).is_none());
        }
        for index in ["00", "+0", "40", "256"] {
            let invalid = READ.replace("\"index\":\"0\"", &format!("\"index\":\"{index}\""));
            assert!(decode_request(invalid.as_bytes()).is_none());
        }
    }

    #[test]
    fn refuses_cross_fragment_mismatches_and_invalid_fragments() {
        let mut q = request(Verb::Begin);
        q.tail.verb = Verb::Close;
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.lifecycle.lifecycle_run_id = "101";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.lifecycle.lifecycle_run_attempt = "2";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.owner.owner_deployment_id = "other";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.prefix.tooling_sha = "bad";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.candidate.build_id = "unknown";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.owner.owner_epoch = "0";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.prefix.caller_run_id = "01";
        q.lifecycle.lifecycle_run_id = "01";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.time.expires_at_ms = q.time.acquired_at_ms;
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.target.staging_guild_id = "1";
        assert!(encode_request(&q).is_none());
        q = request(Verb::Begin);
        q.tail.index = Some(0);
        assert!(encode_request(&q).is_none());
    }
}
