//! Inert encoding of lifecycle run fields, not a complete request or authentication.
//!
//! Representation validation does not establish the original grant, caller identity,
//! or equality with the caller's run ID and attempt. Time, target and tail fields,
//! trusted identity checks, decoding and transport remain separate.

/// Untrusted lifecycle values, in canonical request order.
pub struct RequestLifecycle<'a> {
    pub lifecycle_run_id: &'a str,
    pub lifecycle_run_attempt: &'a str,
}

/// Encodes a comma-prefixed fragment after owner_epoch, validating representation only.
pub fn encode_lifecycle(input: &RequestLifecycle<'_>) -> Option<String> {
    let id = input.lifecycle_run_id;
    let attempt = input.lifecycle_run_attempt;
    if !decimal(id, 20)
        || !decimal(attempt, 16)
        || (attempt.len() == 16 && attempt > "9007199254740991")
    {
        return None;
    }
    Some(format!(
        ",\"lifecycle_run_id\":\"{}\",\"lifecycle_run_attempt\":\"{}\"",
        id, attempt
    ))
}

fn decimal(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && matches!(value.as_bytes()[0], b'1'..=b'9')
        && value.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{encode_lifecycle, RequestLifecycle};

    #[test]
    fn contract_lifecycle_vector_and_bounds() {
        let mut input = RequestLifecycle {
            lifecycle_run_id: "100",
            lifecycle_run_attempt: "1",
        };
        assert_eq!(
            encode_lifecycle(&input).as_deref(),
            Some(",\"lifecycle_run_id\":\"100\",\"lifecycle_run_attempt\":\"1\"")
        );
        input.lifecycle_run_id = "99999999999999999999";
        input.lifecycle_run_attempt = "9007199254740991";
        assert_eq!(encode_lifecycle(&input).as_deref(), Some(
            ",\"lifecycle_run_id\":\"99999999999999999999\",\"lifecycle_run_attempt\":\"9007199254740991\""
        ));
    }

    #[test]
    fn refuses_invalid_lifecycle_representations() {
        let mut input = RequestLifecycle {
            lifecycle_run_id: "100",
            lifecycle_run_attempt: "1",
        };
        for value in [
            "",
            "0",
            "01",
            "-1",
            "+1",
            "1.0",
            "1e2",
            "1 ",
            "1\n",
            "é",
            "1\"",
            "1\\",
            "111111111111111111111",
        ] {
            input.lifecycle_run_id = value;
            assert!(encode_lifecycle(&input).is_none(), "run ID: {value:?}");
        }
        input.lifecycle_run_id = "100";
        for value in [
            "",
            "0",
            "01",
            "-1",
            "+1",
            "1.0",
            "1e2",
            "1 ",
            "1\n",
            "é",
            "1\"",
            "1\\",
            "9007199254740992",
            "10000000000000000",
        ] {
            input.lifecycle_run_attempt = value;
            assert!(encode_lifecycle(&input).is_none(), "attempt: {value:?}");
        }
    }
}
