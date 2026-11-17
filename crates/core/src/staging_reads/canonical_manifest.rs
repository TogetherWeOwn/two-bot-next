//! Inert canonical manifest assembly, not an admission or serving boundary.
//!
//! This does not establish a trusted expected set, provenance, request binding,
//! envelope size, authentication, or authority to issue a challenge or watch PASS.

use super::canonical_journeys::{encode_journeys, ManifestJourney};
use super::canonical_routes::{encode_routes, ManifestRoute};
use super::canonical_slots::encode_slots;
use super::manifest_references::manifest_references_are_valid;
use super::ReadSlot;

/// Encodes the fixed-version manifest after representation and structural checks.
/// Callers must independently enforce every trust and admission requirement.
pub fn encode_manifest(
    candidate_sha: &str,
    slots: &[ReadSlot],
    routes: &[ManifestRoute<'_>],
    journeys: &[ManifestJourney<'_>],
) -> Option<String> {
    if candidate_sha.len() != 40
        || !candidate_sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !manifest_references_are_valid(routes, journeys)
    {
        return None;
    }
    let slots = encode_slots(slots)?;
    let routes = encode_routes(routes)?;
    let journeys = encode_journeys(journeys)?;
    Some(format!(
        "{{\"v\":\"1\",\"candidate_sha\":\"{candidate_sha}\",\"slots\":{slots},\"routes\":{routes},\"journeys\":{journeys}}}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging_reads::{fixed_read_slot, ReadOperation};

    const SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ROUTE_HASH: &str = "ffcd0789bc07b7ec522581e4379fa6943190de881560c23069661548289e1b46";
    const JOURNEY_HASH: &str = "4b66f91acd6b3705c433b04fd47a21b468be440bdb39298d3b0f9421e7845cc8";

    fn plan() -> Vec<ReadSlot> {
        (0..40).map(|i| fixed_read_slot(i).unwrap()).collect()
    }

    fn route(id: &str) -> ManifestRoute<'_> {
        ManifestRoute {
            id,
            evidence_contract_sha256: ROUTE_HASH,
            read_operation: Some(ReadOperation::Identity),
        }
    }

    fn journey<'a>(id: &'a str, route_ids: &'a [&'a str]) -> ManifestJourney<'a> {
        ManifestJourney {
            id,
            evidence_contract_sha256: JOURNEY_HASH,
            route_ids,
        }
    }

    #[test]
    fn encodes_synthetic_manifest_bytes() {
        let slots = (0..40)
            .map(|i| {
                let op = ["identity", "guild_context", "bot_membership", "event_list"][i % 4];
                format!(
                    "{{\"index\":\"{i}\",\"operation\":\"{op}\",\"offset_ms\":\"{}\"}}",
                    15000 + 10000 * i
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let expected = format!(concat!(
            "{{\"v\":\"1\",\"candidate_sha\":\"{SHA}\",\"slots\":[{slots}],",
            "\"routes\":[{{\"id\":\"fixture_route\",\"evidence_contract_sha256\":\"{ROUTE_HASH}\",\"read_operation\":\"identity\"}}],",
            "\"journeys\":[{{\"id\":\"fixture_journey\",\"evidence_contract_sha256\":\"{JOURNEY_HASH}\",\"route_ids\":[\"fixture_route\"]}}]}}"
        ), SHA = SHA, slots = slots, ROUTE_HASH = ROUTE_HASH, JOURNEY_HASH = JOURNEY_HASH);
        assert_eq!(expected.len(), 2867);
        assert_eq!(
            encode_manifest(
                SHA,
                &plan(),
                &[route("fixture_route")],
                &[journey("fixture_journey", &["fixture_route"])]
            )
            .as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn rejects_invalid_candidate_plan_and_references() {
        let slots = plan();
        let routes = [route("a"), route("b")];
        let journeys = [journey("a", &["a", "b"])];
        for sha in [
            "",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "gggggggggggggggggggggggggggggggggggggggg",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\\",
        ] {
            assert!(encode_manifest(sha, &slots, &routes, &journeys).is_none());
        }
        assert!(encode_manifest(SHA, &slots[..39], &routes, &journeys).is_none());
        let mut changed = slots.clone();
        changed.swap(0, 1);
        assert!(encode_manifest(SHA, &changed, &routes, &journeys).is_none());
        assert!(encode_manifest(SHA, &slots, &routes, &[journey("a", &["a"])]).is_none());
        assert!(encode_manifest(SHA, &slots, &routes, &[journey("a", &["a", "c"])]).is_none());
        assert!(encode_manifest(SHA, &slots, &[], &journeys).is_none());
        assert!(encode_manifest(SHA, &slots, &routes, &[]).is_none());
        assert!(encode_manifest(SHA, &slots, &[route("b"), route("a")], &journeys).is_none());
    }
}
