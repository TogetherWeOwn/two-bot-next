//! Inert encoding of the two owner fields, not a complete request or authentication.
//!
//! Lifecycle, target and tail fields, trusted identity checks, decoding and transport
//! remain separate. In particular, encoding does not establish that the owner
//! deployment matches the Worker or lifecycle, or that the epoch is current.

/// Untrusted owner values, in canonical request order.
pub struct RequestOwner<'a> {
    pub owner_deployment_id: &'a str,
    pub owner_epoch: &'a str,
}

/// Encodes a comma-prefixed fragment after image_digest, validating representation only.
pub fn encode_owner(input: &RequestOwner<'_>) -> Option<String> {
    let deployment = input.owner_deployment_id;
    let epoch = input.owner_epoch;
    if deployment.is_empty()
        || deployment.len() > 128
        || !deployment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        || epoch.is_empty()
        || epoch.len() > 16
        || !matches!(epoch.as_bytes()[0], b'1'..=b'9')
        || !epoch.bytes().all(|b| b.is_ascii_digit())
        || (epoch.len() == 16 && epoch > "9007199254740991")
    {
        return None;
    }
    Some(format!(
        ",\"owner_deployment_id\":\"{}\",\"owner_epoch\":\"{}\"",
        deployment, epoch
    ))
}

#[cfg(test)]
mod tests {
    use super::{encode_owner, RequestOwner};

    #[test]
    fn contract_owner_vector() {
        assert_eq!(
            encode_owner(&RequestOwner {
                owner_deployment_id: "fixture_deployment",
                owner_epoch: "1",
            })
            .as_deref(),
            Some(",\"owner_deployment_id\":\"fixture_deployment\",\"owner_epoch\":\"1\"")
        );
    }

    #[test]
    fn refuses_invalid_owner_fields() {
        let too_long = "a".repeat(129);
        let mut owner = RequestOwner {
            owner_deployment_id: "fixture_deployment",
            owner_epoch: "1",
        };
        for value in [
            "", "bad.id", "bad:id", "bad/id", "bad\"id", "bad\\id", "é", &too_long,
        ] {
            owner.owner_deployment_id = value;
            assert!(encode_owner(&owner).is_none());
        }
        let longest = "A".repeat(128);
        owner.owner_deployment_id = &longest;
        owner.owner_epoch = "9007199254740991";
        assert!(encode_owner(&owner).is_some());
        owner.owner_deployment_id = "VERSION_1-2";
        assert!(encode_owner(&owner).is_some());
        for value in [
            "",
            "0",
            "01",
            "-1",
            "+1",
            "1.0",
            "1e2",
            "9007199254740992",
            "10000000000000000",
            "é",
            "1\n",
        ] {
            owner.owner_epoch = value;
            assert!(encode_owner(&owner).is_none());
        }
    }
}
