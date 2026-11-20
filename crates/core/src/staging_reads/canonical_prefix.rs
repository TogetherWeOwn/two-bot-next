//! Inert encoding of the first ten request fields, not a complete request.
//!
//! Candidate, deployment, lifecycle and target fields, the tail, decoding,
//! trusted identity checks, digest and transport are not implemented here.

use super::canonical_tail::Verb;

/// Untrusted values for the variable fields in the request prefix.
pub struct RequestPrefix<'a> {
    pub verb: Verb,
    pub tooling_sha: &'a str,
    pub event_name: &'a str,
    pub caller_run_id: &'a str,
    pub caller_run_attempt: &'a str,
}

fn decimal(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && matches!(value.as_bytes()[0], b'1'..=b'9')
        && value.bytes().all(|b| b.is_ascii_digit())
}

/// Encodes through caller_run_attempt, without a closing brace. This fragment
/// proves neither the complete canonical request nor authentication.
pub fn encode_prefix(input: &RequestPrefix<'_>) -> Option<String> {
    let verb = match input.verb {
        Verb::Challenge => "challenge",
        Verb::Begin => "begin",
        Verb::Read => "read",
        Verb::Status => "status",
        Verb::Close => "close",
    };
    if input.tooling_sha.len() != 40
        || !input
            .tooling_sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !matches!(
            input.event_name,
            "workflow_dispatch" | "workflow_run" | "schedule"
        )
        || !decimal(input.caller_run_id, 20)
        || !decimal(input.caller_run_attempt, 16)
        || (input.caller_run_attempt.len() == 16 && input.caller_run_attempt > "9007199254740991")
    {
        return None;
    }
    Some(format!(
        concat!(
            "{{\"v\":\"1\",\"verb\":\"{}\",\"environment\":\"staging\",",
            "\"repository_id\":\"1396224242\",\"repository_owner_id\":\"319943574\",",
            "\"workflow_ref\":\"TogetherWeOwn/two-bot-next/.github/workflows/",
            "deploy-production.yml@refs/heads/main\",\"tooling_sha\":\"{}\",",
            "\"event_name\":\"{}\",\"caller_run_id\":\"{}\",\"caller_run_attempt\":\"{}\""
        ),
        verb, input.tooling_sha, input.event_name, input.caller_run_id, input.caller_run_attempt
    ))
}

#[cfg(test)]
mod tests {
    use super::{encode_prefix, RequestPrefix, Verb};

    fn prefix() -> RequestPrefix<'static> {
        RequestPrefix {
            verb: Verb::Challenge,
            tooling_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            event_name: "workflow_dispatch",
            caller_run_id: "100",
            caller_run_attempt: "1",
        }
    }

    #[test]
    fn ordered_prefix_and_verb_vectors() {
        let mut q = prefix();
        for (verb, name) in [
            (Verb::Challenge, "challenge"),
            (Verb::Begin, "begin"),
            (Verb::Read, "read"),
            (Verb::Status, "status"),
            (Verb::Close, "close"),
        ] {
            q.verb = verb;
            assert_eq!(encode_prefix(&q), Some(format!(concat!(
                "{{\"v\":\"1\",\"verb\":\"{}\",\"environment\":\"staging\",",
                "\"repository_id\":\"1396224242\",\"repository_owner_id\":\"319943574\",",
                "\"workflow_ref\":\"TogetherWeOwn/two-bot-next/.github/workflows/",
                "deploy-production.yml@refs/heads/main\",\"tooling_sha\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
                "\"event_name\":\"workflow_dispatch\",\"caller_run_id\":\"100\",\"caller_run_attempt\":\"1\""
            ), name)));
        }
        for event in ["workflow_dispatch", "workflow_run", "schedule"] {
            q.event_name = event;
            assert!(encode_prefix(&q).is_some());
        }
    }

    #[test]
    fn refuses_invalid_variable_fields() {
        let mut q = prefix();
        for sha in [
            "",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "é",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\",
        ] {
            q.tooling_sha = sha;
            assert!(encode_prefix(&q).is_none());
        }
        q = prefix();
        for event in ["", "push", "Schedule", "schedule\n"] {
            q.event_name = event;
            assert!(encode_prefix(&q).is_none());
        }
        q = prefix();
        for id in ["", "0", "01", "-1", "1e2", "111111111111111111111", "é"] {
            q.caller_run_id = id;
            assert!(encode_prefix(&q).is_none());
        }
        q.caller_run_id = "99999999999999999999";
        for attempt in [
            "",
            "0",
            "01",
            "+1",
            "1.0",
            "9007199254740992",
            "10000000000000000",
        ] {
            q.caller_run_attempt = attempt;
            assert!(encode_prefix(&q).is_none());
        }
        q.caller_run_attempt = "9007199254740991";
        assert!(encode_prefix(&q).is_some());
    }
}
