//! Inert structural cross-reference check, not full manifest validation or authority.

use super::canonical_journeys::{encode_journeys, ManifestJourney};
use super::canonical_routes::{encode_routes, ManifestRoute};

/// Checks component validity and that journey references cover exactly the supplied
/// routes. This does not establish a trusted expected set, requirement provenance,
/// full manifest validity, authorization, or a serving boundary.
pub fn manifest_references_are_valid(
    routes: &[ManifestRoute<'_>],
    journeys: &[ManifestJourney<'_>],
) -> bool {
    if encode_routes(routes).is_none() || encode_journeys(journeys).is_none() {
        return false;
    }

    let mut referenced = [false; 64];
    for journey in journeys {
        for id in journey.route_ids {
            let Ok(index) = routes.binary_search_by(|route| route.id.cmp(id)) else {
                return false;
            };
            referenced[index] = true;
        }
    }
    referenced[..routes.len()].iter().all(|used| *used)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "4b66f91acd6b3705c433b04fd47a21b468be440bdb39298d3b0f9421e7845cc8";

    fn route(id: &str) -> ManifestRoute<'_> {
        ManifestRoute {
            id,
            evidence_contract_sha256: HASH,
            read_operation: None,
        }
    }

    fn journey<'a>(id: &'a str, route_ids: &'a [&'a str]) -> ManifestJourney<'a> {
        ManifestJourney {
            id,
            evidence_contract_sha256: HASH,
            route_ids,
        }
    }

    #[test]
    fn accepts_complete_and_shared_route_references() {
        let routes = [route("a"), route("b"), route("c")];
        assert!(manifest_references_are_valid(
            &routes,
            &[journey("a", &["a", "b"]), journey("b", &["b", "c"])],
        ));
        assert!(manifest_references_are_valid(
            &[route("a")],
            &[journey("a", &["a"])]
        ));
    }

    #[test]
    fn rejects_dangling_and_unreferenced_routes() {
        let routes = [route("a"), route("b")];
        assert!(!manifest_references_are_valid(
            &routes,
            &[journey("a", &["a", "c"])]
        ));
        assert!(!manifest_references_are_valid(
            &routes,
            &[journey("a", &["a"])]
        ));
    }

    #[test]
    fn preserves_component_validation() {
        assert!(!manifest_references_are_valid(&[], &[journey("a", &["a"])]));
        assert!(!manifest_references_are_valid(&[route("a")], &[]));
        assert!(!manifest_references_are_valid(
            &[route("a"), route("a")],
            &[journey("a", &["a"])]
        ));
        assert!(!manifest_references_are_valid(
            &[route("b"), route("a")],
            &[journey("a", &["a", "b"])]
        ));
        assert!(!manifest_references_are_valid(
            &[route("a")],
            &[journey("a", &["a", "a"])]
        ));
        assert!(!manifest_references_are_valid(
            &[route("a")],
            &[journey("b", &["a"]), journey("a", &["a"])]
        ));
        assert!(!manifest_references_are_valid(
            &[ManifestRoute {
                evidence_contract_sha256: "bad",
                ..route("a")
            }],
            &[journey("a", &["a"])]
        ));
        assert!(!manifest_references_are_valid(
            &[route("a")],
            &[ManifestJourney {
                evidence_contract_sha256: "bad",
                ..journey("a", &["a"])
            }]
        ));
    }

    #[test]
    fn accepts_maximum_bounded_route_set() {
        let ids: Vec<_> = (0..65).map(|n| format!("r{n:02}")).collect();
        let routes: Vec<_> = ids.iter().map(|id| route(id)).collect();
        let refs: Vec<_> = ids.iter().map(String::as_str).collect();
        assert!(manifest_references_are_valid(
            &routes[..64],
            &[journey("a", &refs[..64])]
        ));
        assert!(!manifest_references_are_valid(
            &routes,
            &[journey("a", &refs[..64])]
        ));
        assert!(!manifest_references_are_valid(
            &routes[..64],
            &[journey("a", &refs)]
        ));
    }
}
