//! Guard: both LFG send sites degrade through `payload_components`.
//!
//! Builder-owned wiring pin. The protected `lfg_components_acceptance` file
//! pins the helper itself; this file pins that the post and refresh send
//! paths actually call it, so reverting either site to
//! `expect("rendered array")` fails the suite. Source-level by design: the
//! only producer always emits a components array, so no runtime payload can
//! distinguish the degrading call from the old expect at the call sites.
//!
//! Offline only: reads one source file, no database, no network.

#[test]
fn lfg_send_sites_route_through_payload_components() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lfg_interactions.rs"),
    )
    .expect("read lfg_interactions.rs");
    assert!(
        !source.contains(r#"expect("rendered array")"#),
        "LFG send sites must degrade through payload_components, not expect"
    );
    for site in [
        "post_message_with_components",
        "edit_message_with_components",
    ] {
        assert!(
            source.contains(site),
            "expected LFG send site {site} in lfg_interactions.rs"
        );
    }
    let routed = source.matches("Self::payload_components(&payload)").count();
    assert!(
        routed >= 2,
        "post and refresh send sites must route through payload_components, found {routed}"
    );
}
