//! Inert encoding of the final five canonical request fields, not a request validator.
//!
//! This fragment is not a JSON object or an authenticated request. The remaining
//! schema, decoding, trusted identity checks, digest and transport are separate.

/// The request operation controls which tail fields are applicable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verb {
    Challenge,
    Begin,
    Read,
    Status,
    Close,
}

/// Untrusted values for the final five request fields.
pub struct RequestTail<'a> {
    pub verb: Verb,
    pub challenge: Option<&'a str>,
    pub incarnation: Option<&'a str>,
    pub manifest_sha256: &'a str,
    pub index: Option<u8>,
    pub query: Option<&'a str>,
}

fn lowercase_hex(value: &str, length: usize, nonzero: bool) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && (!nonzero || value.bytes().any(|b| b != b'0'))
}

/// Encodes only the five-field suffix, including its leading comma and final brace.
/// A result proves neither the rest of the request nor any authorization.
pub fn encode_tail(input: &RequestTail<'_>) -> Option<String> {
    if !lowercase_hex(input.manifest_sha256, 64, false) {
        return None;
    }
    let identity = match (input.verb, input.challenge, input.incarnation) {
        (Verb::Challenge, None, None) => "null,\"incarnation\":null".to_owned(),
        (Verb::Challenge, _, _) => return None,
        (_, Some(challenge), Some(incarnation))
            if lowercase_hex(challenge, 64, true) && lowercase_hex(incarnation, 32, true) =>
        {
            format!("\"{challenge}\",\"incarnation\":\"{incarnation}\"")
        }
        _ => return None,
    };
    let index = match (input.verb, input.index) {
        (Verb::Read, Some(index @ 0..=39)) => format!("\"{index}\""),
        (Verb::Read, _) | (_, Some(_)) => return None,
        (_, None) => "null".to_owned(),
    };
    let query = match (input.verb, input.query) {
        (Verb::Status, Some("measurement")) => "\"measurement\"",
        (Verb::Status, _) | (_, Some(_)) => return None,
        (_, None) => "null",
    };
    Some(format!(
        ",\"challenge\":{identity},\"manifest_sha256\":\"{}\",\"index\":{index},\"query\":{query}}}",
        input.manifest_sha256
    ))
}

#[cfg(test)]
mod tests {
    use super::{encode_tail, RequestTail, Verb};

    const MANIFEST: &str = "0f1f9821354531dc6aedd4ee10c3ddc365e0032ac557aace69a1a76d7fbd6e1c";
    const CHALLENGE: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const INCARNATION: &str = "000000010000000200000003ffffffff";

    fn tail(verb: Verb) -> RequestTail<'static> {
        RequestTail {
            verb,
            challenge: Some(CHALLENGE),
            incarnation: Some(INCARNATION),
            manifest_sha256: MANIFEST,
            index: None,
            query: None,
        }
    }

    #[test]
    fn canonical_suffix_vectors() {
        let mut q = tail(Verb::Challenge);
        q.challenge = None;
        q.incarnation = None;
        assert_eq!(encode_tail(&q), Some(format!(",\"challenge\":null,\"incarnation\":null,\"manifest_sha256\":\"{MANIFEST}\",\"index\":null,\"query\":null}}")));
        for verb in [Verb::Begin, Verb::Close, Verb::Read, Verb::Status] {
            let mut q = tail(verb);
            q.index = (verb == Verb::Read).then_some(0);
            q.query = (verb == Verb::Status).then_some("measurement");
            let index = if verb == Verb::Read { "\"0\"" } else { "null" };
            let query = if verb == Verb::Status {
                "\"measurement\""
            } else {
                "null"
            };
            assert_eq!(encode_tail(&q), Some(format!(",\"challenge\":\"{CHALLENGE}\",\"incarnation\":\"{INCARNATION}\",\"manifest_sha256\":\"{MANIFEST}\",\"index\":{index},\"query\":{query}}}")));
        }
        let mut q = tail(Verb::Read);
        q.index = Some(39);
        assert!(encode_tail(&q).is_some());
    }

    #[test]
    fn rejects_invalid_shapes_and_alphabets() {
        let zero64 = "0".repeat(64);
        let zero32 = "0".repeat(32);
        let upper64 = "A".repeat(64);
        let mut q = tail(Verb::Begin);
        for invalid in ["", &zero64, &upper64, "é", "\\u0061"] {
            q.challenge = Some(invalid);
            assert!(encode_tail(&q).is_none());
        }
        q.challenge = Some(CHALLENGE);
        for invalid in ["", &zero32, "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"] {
            q.incarnation = Some(invalid);
            assert!(encode_tail(&q).is_none());
        }
        q.incarnation = Some(INCARNATION);
        for invalid in ["", &upper64, "a\"", "é"] {
            q.manifest_sha256 = invalid;
            assert!(encode_tail(&q).is_none());
        }
        q.manifest_sha256 = MANIFEST;
        q.challenge = None;
        assert!(encode_tail(&q).is_none());
        q = tail(Verb::Challenge);
        assert!(encode_tail(&q).is_none());
        q = tail(Verb::Read);
        assert!(encode_tail(&q).is_none());
        q.index = Some(40);
        assert!(encode_tail(&q).is_none());
        q = tail(Verb::Begin);
        q.index = Some(0);
        assert!(encode_tail(&q).is_none());
        q.index = None;
        q.query = Some("measurement");
        assert!(encode_tail(&q).is_none());
        q.verb = Verb::Status;
        q.query = None;
        assert!(encode_tail(&q).is_none());
        q.query = Some("Measurement");
        assert!(encode_tail(&q).is_none());
    }
}
