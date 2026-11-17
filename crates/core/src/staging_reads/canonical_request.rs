//! Inert canonical request assembly. This does not authenticate or admit a request.
//!
//! Decoding, digest computation, trusted provenance and transport remain separate.

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

    #[test]
    fn contract_request_vectors() {
        for (verb, name, identity, index, query, length) in [
            (Verb::Challenge, "challenge", "null,\"incarnation\":null", "null", "null", 1025),
            (Verb::Begin, "begin", "\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\",\"incarnation\":\"000000010000000200000003ffffffff\"", "null", "null", 1113),
            (Verb::Read, "read", "\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\",\"incarnation\":\"000000010000000200000003ffffffff\"", "\"0\"", "null", 1111),
            (Verb::Status, "status", "\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\",\"incarnation\":\"000000010000000200000003ffffffff\"", "null", "\"measurement\"", 1123),
            (Verb::Close, "close", "\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\",\"incarnation\":\"000000010000000200000003ffffffff\"", "null", "null", 1113),
        ] {
            let expected = format!(concat!(
                "{{\"v\":\"1\",\"verb\":\"{}\",\"environment\":\"staging\",\"repository_id\":\"1396224242\",",
                "\"repository_owner_id\":\"319943574\",\"workflow_ref\":\"TogetherWeOwn/two-bot-next/.github/workflows/deploy-production.yml@refs/heads/main\",",
                "\"tooling_sha\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"event_name\":\"workflow_dispatch\",",
                "\"caller_run_id\":\"100\",\"caller_run_attempt\":\"1\",\"candidate_run_id\":\"99\",\"candidate_run_attempt\":\"2\",",
                "\"candidate_sha\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"build_id\":\"99-2\",\"worker_version_id\":\"fixture_deployment\",",
                "\"image_digest\":\"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\",",
                "\"owner_deployment_id\":\"fixture_deployment\",\"owner_epoch\":\"1\",\"lifecycle_run_id\":\"100\",\"lifecycle_run_attempt\":\"1\",",
                "\"acquired_at_ms\":\"1794855600000\",\"expires_at_ms\":\"1794858000000\",\"staging_application_id\":\"1469137636663758888\",",
                "\"staging_guild_id\":\"1545644954272137297\",\"challenge\":{},\"manifest_sha256\":\"0f1f9821354531dc6aedd4ee10c3ddc365e0032ac557aace69a1a76d7fbd6e1c\",\"index\":{},\"query\":{}}}"
            ), name, identity, index, query);
            assert_eq!(expected.len(), length);
            assert_eq!(encode_request(&request(verb)).as_deref(), Some(expected.as_str()));
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
