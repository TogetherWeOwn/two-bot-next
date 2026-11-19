//! Inert encoding of request time fields, not a grant or request validator.
//!
//! Callers must independently establish the original stored values, grant limits,
//! current time, identity and authorization. Target fields, complete encoding,
//! decoding and transport remain separate.

/// Untrusted original grant timestamps in epoch milliseconds, in request order.
pub struct RequestTime<'a> {
    pub acquired_at_ms: &'a str,
    pub expires_at_ms: &'a str,
}

/// Encodes a comma-prefixed fragment after lifecycle_run_attempt.
pub fn encode_time(input: &RequestTime<'_>) -> Option<String> {
    let acquired = safe_decimal(input.acquired_at_ms)?;
    let expires = safe_decimal(input.expires_at_ms)?;
    if acquired >= expires {
        return None;
    }
    Some(format!(
        ",\"acquired_at_ms\":\"{}\",\"expires_at_ms\":\"{}\"",
        input.acquired_at_ms, input.expires_at_ms
    ))
}

fn safe_decimal(value: &str) -> Option<u64> {
    if value.is_empty()
        || value.len() > 16
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let number = value.parse::<u64>().ok()?;
    (number <= 9_007_199_254_740_991).then_some(number)
}

#[cfg(test)]
mod tests {
    use super::{encode_time, RequestTime};

    #[test]
    fn contract_vector_and_safe_bounds() {
        let input = RequestTime {
            acquired_at_ms: "1794855600000",
            expires_at_ms: "1794858000000",
        };
        assert_eq!(
            encode_time(&input).as_deref(),
            Some(",\"acquired_at_ms\":\"1794855600000\",\"expires_at_ms\":\"1794858000000\"")
        );
        assert_eq!(
            encode_time(&RequestTime {
                acquired_at_ms: "0",
                expires_at_ms: "9007199254740991",
            })
            .as_deref(),
            Some(",\"acquired_at_ms\":\"0\",\"expires_at_ms\":\"9007199254740991\"")
        );
    }

    #[test]
    fn refuses_invalid_values_and_order() {
        for value in [
            "",
            "00",
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
            assert!(
                encode_time(&RequestTime {
                    acquired_at_ms: value,
                    expires_at_ms: "9007199254740991",
                })
                .is_none(),
                "acquired: {value:?}"
            );
            assert!(
                encode_time(&RequestTime {
                    acquired_at_ms: "0",
                    expires_at_ms: value,
                })
                .is_none(),
                "expires: {value:?}"
            );
        }
        for (acquired, expires) in [("0", "0"), ("42", "42"), ("100", "99")] {
            assert!(encode_time(&RequestTime {
                acquired_at_ms: acquired,
                expires_at_ms: expires,
            })
            .is_none());
        }
    }
}
