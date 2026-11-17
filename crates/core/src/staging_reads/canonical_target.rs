//! Inert encoding of the staging application and guild fields only.
//!
//! This fragment does not establish trusted identity or authorize a request.
//! Complete request encoding, decoding, digest and integration remain separate.

/// Untrusted target identifiers in canonical request order.
pub struct RequestTarget<'a> {
    pub staging_application_id: &'a str,
    pub staging_guild_id: &'a str,
}

/// Encodes the comma-prefixed fields after expires_at_ms when both match v1.
pub fn encode_target(input: &RequestTarget<'_>) -> Option<&'static str> {
    if input.staging_application_id != "1469137636663758888"
        || input.staging_guild_id != "1545644954272137297"
    {
        return None;
    }
    Some(
        ",\"staging_application_id\":\"1469137636663758888\",\"staging_guild_id\":\"1545644954272137297\"",
    )
}

#[cfg(test)]
mod tests {
    use super::{encode_target, RequestTarget};

    const APPLICATION: &str = "1469137636663758888";
    const GUILD: &str = "1545644954272137297";

    #[test]
    fn encodes_exact_ordered_target_fields() {
        assert_eq!(
            encode_target(&RequestTarget {
                staging_application_id: APPLICATION,
                staging_guild_id: GUILD,
            }),
            Some(
                ",\"staging_application_id\":\"1469137636663758888\",\"staging_guild_id\":\"1545644954272137297\""
            )
        );
    }

    #[test]
    fn refuses_altered_or_noncanonical_identifiers() {
        for invalid in [
            "",
            "0",
            "01469137636663758888",
            "1469137636663758889",
            " 1469137636663758888",
            "1469137636663758888\n",
            "1469137636663758888\"",
            "\\u0031",
            "é",
        ] {
            assert!(encode_target(&RequestTarget {
                staging_application_id: invalid,
                staging_guild_id: GUILD,
            })
            .is_none());
        }
        for invalid in [
            "",
            "0",
            "01545644954272137297",
            "1545644954272137298",
            "1545644954272137297 ",
            "1545644954272137297\n",
            "1545644954272137297\\",
            "\\u0031",
            "é",
        ] {
            assert!(encode_target(&RequestTarget {
                staging_application_id: APPLICATION,
                staging_guild_id: invalid,
            })
            .is_none());
        }
        assert!(encode_target(&RequestTarget {
            staging_application_id: GUILD,
            staging_guild_id: APPLICATION,
        })
        .is_none());
    }
}
