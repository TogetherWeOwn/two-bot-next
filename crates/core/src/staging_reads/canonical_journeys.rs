//! Inert canonical journeys array encoding, not manifest validation or authorization.
//!
//! Callers must separately establish that references exist, every required route
//! is referenced, requirements have reviewed provenance, and the entire manifest
//! matches a trusted expected set. This fragment grants no authority to read.

use std::fmt::Write;

/// An untrusted journey projection and its untrusted route references.
pub struct ManifestJourney<'a> {
    pub id: &'a str,
    pub evidence_contract_sha256: &'a str,
    pub route_ids: &'a [&'a str],
}

fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// Encodes a bounded, strictly ordered journeys array without resolving references.
pub fn encode_journeys(journeys: &[ManifestJourney<'_>]) -> Option<String> {
    if !(1..=64).contains(&journeys.len()) {
        return None;
    }
    for (position, journey) in journeys.iter().enumerate() {
        let hash = journey.evidence_contract_sha256.as_bytes();
        if !valid_id(journey.id)
            || hash.len() != 64
            || !hash
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            || (position > 0 && journeys[position - 1].id >= journey.id)
            || !(1..=64).contains(&journey.route_ids.len())
            || journey.route_ids.iter().any(|id| !valid_id(id))
            || journey.route_ids.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return None;
        }
    }

    let mut output = String::from("[");
    for (position, journey) in journeys.iter().enumerate() {
        if position != 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"id\":\"{}\",\"evidence_contract_sha256\":\"{}\",\"route_ids\":[",
            journey.id, journey.evidence_contract_sha256
        )
        .ok()?;
        for (index, id) in journey.route_ids.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            write!(output, "\"{id}\"").ok()?;
        }
        output.push_str("]}");
    }
    output.push(']');
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "4b66f91acd6b3705c433b04fd47a21b468be440bdb39298d3b0f9421e7845cc8";

    fn journey<'a>(id: &'a str, route_ids: &'a [&'a str]) -> ManifestJourney<'a> {
        ManifestJourney {
            id,
            evidence_contract_sha256: HASH,
            route_ids,
        }
    }

    #[test]
    fn encodes_exact_synthetic_vector_and_multiple_entries() {
        assert_eq!(
            encode_journeys(&[journey("fixture_journey", &["fixture_route"])]).as_deref(),
            Some(concat!(
                "[{\"id\":\"fixture_journey\",\"evidence_contract_sha256\":",
                "\"4b66f91acd6b3705c433b04fd47a21b468be440bdb39298d3b0f9421e7845cc8\",",
                "\"route_ids\":[\"fixture_route\"]}]"
            ))
        );
        let expected = format!("[{{\"id\":\"a\",\"evidence_contract_sha256\":\"{HASH}\",\"route_ids\":[\"a\",\"b\"]}},{{\"id\":\"b\",\"evidence_contract_sha256\":\"{HASH}\",\"route_ids\":[\"c\"]}}]");
        assert_eq!(
            encode_journeys(&[journey("a", &["a", "b"]), journey("b", &["c"])]),
            Some(expected)
        );
    }

    #[test]
    fn rejects_invalid_ids_and_hashes() {
        let long = "a".repeat(65);
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
            long.as_str(),
        ] {
            assert!(encode_journeys(&[journey(id, &["a"])]).is_none(), "{id:?}");
            assert!(encode_journeys(&[journey("a", &[id])]).is_none(), "{id:?}");
        }
        let max = "a".repeat(64);
        assert!(encode_journeys(&[journey(&max, &[&max])]).is_some());
        for hash in [
            "",
            &"a".repeat(63),
            &"a".repeat(65),
            &"A".repeat(64),
            &"g".repeat(64),
            &"é".repeat(32),
        ] {
            assert!(encode_journeys(&[ManifestJourney {
                evidence_contract_sha256: hash,
                ..journey("a", &["a"])
            }])
            .is_none());
        }
        assert!(encode_journeys(&[ManifestJourney {
            evidence_contract_sha256: &"0".repeat(64),
            ..journey("a", &["a"])
        }])
        .is_some());
    }

    #[test]
    fn enforces_bounds_and_order_for_both_lists() {
        assert!(encode_journeys(&[]).is_none());
        assert!(encode_journeys(&[journey("a", &[])]).is_none());
        assert!(encode_journeys(&[journey("a", &["a"]), journey("a", &["b"])]).is_none());
        assert!(encode_journeys(&[journey("b", &["a"]), journey("a", &["b"])]).is_none());
        assert!(encode_journeys(&[journey("a", &["a", "a"])]).is_none());
        assert!(encode_journeys(&[journey("a", &["b", "a"])]).is_none());
        let ids: Vec<_> = (0..65).map(|n| format!("r{n:02}")).collect();
        let refs: Vec<_> = ids.iter().map(String::as_str).collect();
        let journeys: Vec<_> = refs.iter().map(|id| journey(id, &["a"])).collect();
        assert!(encode_journeys(&journeys[..64]).is_some());
        assert!(encode_journeys(&journeys).is_none());
        assert!(encode_journeys(&[journey("a", &refs[..64])]).is_some());
        assert!(encode_journeys(&[journey("a", &refs)]).is_none());
    }
}
