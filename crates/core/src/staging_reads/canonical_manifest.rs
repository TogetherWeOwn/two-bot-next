//! Inert canonical manifest assembly, not an admission or serving boundary.
//!
//! This does not establish a trusted expected set, provenance, request binding,
//! envelope size, authentication, or authority to issue a challenge or watch PASS.

use super::canonical_journeys::{encode_journeys, ManifestJourney};
use super::canonical_routes::{encode_routes, ManifestRoute};
use super::canonical_slots::encode_slots;
use super::manifest_references::manifest_references_are_valid;
use super::ReadSlot;
use sha2::{Digest, Sha256};

/// Hashes an assembled, structurally validated manifest with the v1 domain.
/// This does not establish provenance, expected-set completeness, envelope size,
/// request binding, authentication, authorization, or a watch PASS.
pub fn manifest_digest(
    candidate_sha: &str,
    slots: &[ReadSlot],
    routes: &[ManifestRoute<'_>],
    journeys: &[ManifestJourney<'_>],
) -> Option<String> {
    let manifest = encode_manifest(candidate_sha, slots, routes, journeys)?;
    let mut hash = Sha256::new();
    hash.update(b"two.bot.staging-reads.manifest.v1\n");
    hash.update(manifest.as_bytes());
    Some(hex::encode(hash.finalize()))
}

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
        // Exact synthetic packet bytes, with no final LF.
        let expected = include_str!("../../tests/fixtures/counted_read_manifest.json");
        assert_eq!(expected.len(), 2867);
        assert_eq!(
            encode_manifest(
                SHA,
                &plan(),
                &[route("fixture_route")],
                &[journey("fixture_journey", &["fixture_route"])]
            )
            .as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn hashes_independent_manifest_vectors() {
        let slots = plan();
        let routes = [route("fixture_route")];
        let journeys = [journey("fixture_journey", &["fixture_route"])];
        // The first digest is the protocol fixture; the second was calculated
        // independently using Python's json and hashlib implementations.
        for (sha, expected) in [
            (
                SHA,
                "0f1f9821354531dc6aedd4ee10c3ddc365e0032ac557aace69a1a76d7fbd6e1c",
            ),
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "84840bab05516edce8722bc89234a8f0bef198e6dd7d96071652eda2e5824bd9",
            ),
        ] {
            assert_eq!(
                manifest_digest(sha, &slots, &routes, &journeys).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn digest_rejects_invalid_manifest_inputs() {
        let slots = plan();
        let routes = [route("a"), route("b")];
        let journeys = [journey("a", &["a", "b"])];
        assert!(manifest_digest("BAD", &slots, &routes, &journeys).is_none());
        assert!(manifest_digest(SHA, &slots[..39], &routes, &journeys).is_none());
        let mut changed = slots.clone();
        changed.push(slots[39]);
        assert!(manifest_digest(SHA, &changed, &routes, &journeys).is_none());
        changed.truncate(slots.len());
        changed.swap(0, 1);
        assert!(manifest_digest(SHA, &changed, &routes, &journeys).is_none());
        changed.swap(0, 1);
        for invalid in [
            ReadSlot {
                index: 1,
                ..slots[0]
            },
            ReadSlot {
                operation: ReadOperation::EventList,
                ..slots[0]
            },
            ReadSlot {
                due_after_seconds: 16,
                ..slots[0]
            },
        ] {
            changed[0] = invalid;
            assert!(
                manifest_digest(SHA, &changed, &routes, &journeys).is_none(),
                "{invalid:?}"
            );
        }
        let bad_route = ManifestRoute {
            evidence_contract_sha256: "",
            ..route("a")
        };
        let bad_journey = ManifestJourney {
            evidence_contract_sha256: "not-a-sha256",
            ..journey("a", &["a", "b"])
        };
        assert!(manifest_digest(SHA, &slots, &[bad_route, route("b")], &journeys).is_none());
        assert!(manifest_digest(SHA, &slots, &routes, &[bad_journey]).is_none());
        assert!(manifest_digest(SHA, &slots, &routes, &[journey("a", &["a"])]).is_none());
        assert!(manifest_digest(SHA, &slots, &routes, &[journey("a", &["a", "c"])]).is_none());
        assert!(manifest_digest(SHA, &slots, &[route("b"), route("a")], &journeys).is_none());
        assert!(manifest_digest(SHA, &slots, &[], &journeys).is_none());
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
