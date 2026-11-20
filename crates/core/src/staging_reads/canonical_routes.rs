//! Inert canonical routes array encoding, not manifest validation or authorization.
//!
//! A digest's syntax does not establish reviewed evidence. Callers must separately
//! verify its source, required coverage and trusted expected manifest. A null read
//! operation does not waive the route's separately authorized evidence.

use super::ReadOperation;
use std::fmt::Write;

/// An untrusted route projection; operation is restricted to the fixed vocabulary.
pub struct ManifestRoute<'a> {
    pub id: &'a str,
    pub evidence_contract_sha256: &'a str,
    pub read_operation: Option<ReadOperation>,
}

/// Encodes only a bounded, strictly ordered routes array. It does not establish
/// completeness, evidence provenance, or permission to perform a read.
pub fn encode_routes(routes: &[ManifestRoute<'_>]) -> Option<String> {
    if !(1..=64).contains(&routes.len()) {
        return None;
    }
    for (position, route) in routes.iter().enumerate() {
        let id = route.id.as_bytes();
        let hash = route.evidence_contract_sha256.as_bytes();
        if id.is_empty()
            || id.len() > 64
            || !id[0].is_ascii_lowercase()
            || !id[1..]
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
            || hash.len() != 64
            || !hash
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            || (position > 0 && routes[position - 1].id >= route.id)
        {
            return None;
        }
    }

    let mut output = String::from("[");
    for (position, route) in routes.iter().enumerate() {
        if position != 0 {
            output.push(',');
        }
        let operation = match route.read_operation {
            Some(ReadOperation::Identity) => "\"identity\"",
            Some(ReadOperation::GuildContext) => "\"guild_context\"",
            Some(ReadOperation::BotMembership) => "\"bot_membership\"",
            Some(ReadOperation::EventList) => "\"event_list\"",
            None => "null",
        };
        write!(
            output,
            "{{\"id\":\"{}\",\"evidence_contract_sha256\":\"{}\",\"read_operation\":{}}}",
            route.id, route.evidence_contract_sha256, operation
        )
        .ok()?;
    }
    output.push(']');
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "ffcd0789bc07b7ec522581e4379fa6943190de881560c23069661548289e1b46";

    fn route(id: &str) -> ManifestRoute<'_> {
        ManifestRoute {
            id,
            evidence_contract_sha256: HASH,
            read_operation: None,
        }
    }

    #[test]
    fn encodes_interoperability_vector_and_null() {
        let routes = [ManifestRoute {
            read_operation: Some(ReadOperation::Identity),
            ..route("fixture_route")
        }];
        assert_eq!(
            encode_routes(&routes).as_deref(),
            Some(concat!(
                "[{\"id\":\"fixture_route\",\"evidence_contract_sha256\":",
                "\"ffcd0789bc07b7ec522581e4379fa6943190de881560c23069661548289e1b46\",",
                "\"read_operation\":\"identity\"}]"
            ))
        );
        assert_eq!(
            encode_routes(&[route("a")]).as_deref(),
            Some(concat!(
                "[{\"id\":\"a\",\"evidence_contract_sha256\":",
                "\"ffcd0789bc07b7ec522581e4379fa6943190de881560c23069661548289e1b46\",",
                "\"read_operation\":null}]"
            ))
        );
    }

    #[test]
    fn encodes_all_fixed_operations_in_order() {
        let routes = [
            ManifestRoute {
                read_operation: Some(ReadOperation::BotMembership),
                ..route("a")
            },
            ManifestRoute {
                read_operation: Some(ReadOperation::EventList),
                ..route("b")
            },
            ManifestRoute {
                read_operation: Some(ReadOperation::GuildContext),
                ..route("c")
            },
            ManifestRoute {
                read_operation: Some(ReadOperation::Identity),
                ..route("d")
            },
        ];
        let expected = format!(concat!(
            "[{{\"id\":\"a\",\"evidence_contract_sha256\":\"{0}\",\"read_operation\":\"bot_membership\"}},",
            "{{\"id\":\"b\",\"evidence_contract_sha256\":\"{0}\",\"read_operation\":\"event_list\"}},",
            "{{\"id\":\"c\",\"evidence_contract_sha256\":\"{0}\",\"read_operation\":\"guild_context\"}},",
            "{{\"id\":\"d\",\"evidence_contract_sha256\":\"{0}\",\"read_operation\":\"identity\"}}]"
        ), HASH);
        assert_eq!(encode_routes(&routes), Some(expected));
    }

    #[test]
    fn rejects_invalid_ids_and_hashes() {
        let long_id = "a".repeat(65);
        for id in [
            "",
            "A",
            "0a",
            "_a",
            "a-",
            "a/",
            "a\"",
            "a\\",
            "a ",
            "a\n",
            "aé",
            long_id.as_str(),
        ] {
            assert!(encode_routes(&[route(id)]).is_none(), "{id:?}");
        }
        assert!(encode_routes(&[route(&"a".repeat(64))]).is_some());
        for hash in [
            "",
            &"a".repeat(63),
            &"a".repeat(65),
            &"A".repeat(64),
            &"g".repeat(64),
            &"é".repeat(32),
        ] {
            assert!(encode_routes(&[ManifestRoute {
                evidence_contract_sha256: hash,
                ..route("a")
            }])
            .is_none());
        }
        assert!(encode_routes(&[ManifestRoute {
            evidence_contract_sha256: &"0".repeat(64),
            ..route("a")
        }])
        .is_some());
    }

    #[test]
    fn rejects_empty_extra_duplicate_and_unordered_routes() {
        assert!(encode_routes(&[]).is_none());
        assert!(encode_routes(&[route("a"), route("a")]).is_none());
        assert!(encode_routes(&[route("b"), route("a")]).is_none());
        let ids: Vec<_> = (0..65).map(|n| format!("r{n:02}")).collect();
        let routes: Vec<_> = ids.iter().map(|id| route(id)).collect();
        assert!(encode_routes(&routes[..64]).is_some());
        assert!(encode_routes(&routes).is_none());
    }
}
