//! Inert encoding of six candidate and deployment request fields.
//!
//! This fragment is not a complete request or authentication. Owner, lifecycle,
//! target and tail fields, trusted identity checks, decoding and transport remain separate.

/// Untrusted candidate and deployment values, in canonical request order.
pub struct RequestCandidate<'a> {
    pub candidate_run_id: &'a str,
    pub candidate_run_attempt: &'a str,
    pub candidate_sha: &'a str,
    pub build_id: &'a str,
    pub worker_version_id: &'a str,
    pub image_digest: &'a str,
}

fn decimal(value: &str, max_len: usize, upper_bound: Option<&str>) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && matches!(value.as_bytes()[0], b'1'..=b'9')
        && value.bytes().all(|b| b.is_ascii_digit())
        && upper_bound.is_none_or(|bound| value.len() < bound.len() || value <= bound)
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn identifier(value: &str, build: bool) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'_' | b'-')
                || (build && matches!(b, b'.' | b':'))
        })
        && (!build || value != "unknown")
}

/// Encodes a comma-prefixed fragment through image_digest. This validates only
/// the representation, not the provenance or consistency of these identities.
pub fn encode_candidate(input: &RequestCandidate<'_>) -> Option<String> {
    if !decimal(input.candidate_run_id, 20, None)
        || !decimal(input.candidate_run_attempt, 16, Some("9007199254740991"))
        || !lower_hex(input.candidate_sha, 40)
        || !identifier(input.build_id, true)
        || !identifier(input.worker_version_id, false)
        || !input
            .image_digest
            .strip_prefix("sha256:")
            .is_some_and(|digest| lower_hex(digest, 64))
    {
        return None;
    }
    Some(format!(
        concat!(
            ",\"candidate_run_id\":\"{}\",\"candidate_run_attempt\":\"{}\",",
            "\"candidate_sha\":\"{}\",\"build_id\":\"{}\",",
            "\"worker_version_id\":\"{}\",\"image_digest\":\"{}\""
        ),
        input.candidate_run_id,
        input.candidate_run_attempt,
        input.candidate_sha,
        input.build_id,
        input.worker_version_id,
        input.image_digest
    ))
}

#[cfg(test)]
mod tests {
    use super::{encode_candidate, RequestCandidate};

    fn candidate() -> RequestCandidate<'static> {
        RequestCandidate {
            candidate_run_id: "99",
            candidate_run_attempt: "2",
            candidate_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            build_id: "99-2",
            worker_version_id: "fixture_deployment",
            image_digest: "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        }
    }

    #[test]
    fn contract_candidate_vector() {
        assert_eq!(encode_candidate(&candidate()).as_deref(), Some(concat!(
            ",\"candidate_run_id\":\"99\",\"candidate_run_attempt\":\"2\",",
            "\"candidate_sha\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"build_id\":\"99-2\",",
            "\"worker_version_id\":\"fixture_deployment\",",
            "\"image_digest\":\"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\""
        )));
    }

    #[test]
    fn refuses_invalid_representations() {
        let mut q = candidate();
        for value in ["", "0", "01", "-1", "1e2", "111111111111111111111", "é"] {
            q.candidate_run_id = value;
            assert!(encode_candidate(&q).is_none());
        }
        q = candidate();
        for value in [
            "",
            "0",
            "01",
            "+1",
            "1.0",
            "9007199254740992",
            "10000000000000000",
        ] {
            q.candidate_run_attempt = value;
            assert!(encode_candidate(&q).is_none());
        }
        q.candidate_run_attempt = "9007199254740991";
        q.candidate_run_id = "99999999999999999999";
        assert!(encode_candidate(&q).is_some());
        q = candidate();
        for value in [
            "",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "é",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\\",
        ] {
            q.candidate_sha = value;
            assert!(encode_candidate(&q).is_none());
        }
        q = candidate();
        let too_long = "a".repeat(129);
        for value in ["", "unknown", "bad\"id", "é", &too_long] {
            q.build_id = value;
            assert!(encode_candidate(&q).is_none());
        }
        q.build_id = "Build_1.-:ok";
        assert!(encode_candidate(&q).is_some());
        for value in ["", "bad.id", "bad:id", "bad/id", "é", &too_long] {
            q.worker_version_id = value;
            assert!(encode_candidate(&q).is_none());
        }
        q.worker_version_id = "VERSION_1-2";
        assert!(encode_candidate(&q).is_some());
        for value in [
            "",
            "SHA256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "sha256:CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            "sha256:abc",
            "sha256:\"",
            "é",
        ] {
            q.image_digest = value;
            assert!(encode_candidate(&q).is_none());
        }
        q.image_digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        assert!(encode_candidate(&q).is_some());
    }
}
