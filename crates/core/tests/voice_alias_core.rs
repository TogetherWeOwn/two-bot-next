//! Hermetic V7d acceptance cases against the pure alias-resolution core.
//!
//! Table tests pin the documented rules (unicode folding, prefix collisions,
//! unknown-game `None`, non-alias tokens refused); one property test proves
//! resolution is a pure function of `(input, table)`: same inputs give the
//! same output with no ordering dependence.
//!
//! No tests in this fixture use a database, Redis, Discord, or a staging
//! identity.

use proptest::prelude::*;
use two_bot_core::voice_alias_core::{applies_to, normalize_game_name, resolve_alias, AliasRow};

fn rows<'a>(pairs: &[(&'a str, &'a str)]) -> Vec<AliasRow<'a>> {
    pairs
        .iter()
        .map(|(key, target)| AliasRow { key, target })
        .collect()
}

#[test]
fn unicode_folding_matches_across_spellings() {
    let table = rows(&[("Apex Legends", "Apex Legends"), ("Pokémon", "Pokemon")]);
    // Full-width input folds onto the stored ASCII key.
    assert_eq!(
        resolve_alias(&normalize_game_name("\u{FF21}pex \u{FF2C}egends"), &table),
        Some("Apex Legends")
    );
    // Precomposed é and e-plus-combining-accent fold together (NFKC subsumes
    // NFC for canonical equivalence).
    assert_eq!(
        resolve_alias(&normalize_game_name("Poke\u{301}mon"), &table),
        Some("Pokemon")
    );
}

#[test]
fn longest_prefix_wins_and_exact_beats_prefix() {
    let table = rows(&[("apex", "Apex (short)"), ("apex legends", "Apex Legends")]);
    // Both keys prefix the input: the longest wins.
    assert_eq!(
        resolve_alias(&normalize_game_name("Apex Legends Custom Lobby"), &table),
        Some("Apex Legends")
    );
    // An exact key beats a shorter prefix even when listed second.
    let table = rows(&[
        ("apex", "Apex (short)"),
        ("apex legends custom lobby", "ALCL"),
    ]);
    assert_eq!(
        resolve_alias(&normalize_game_name("apex legends custom lobby"), &table),
        Some("ALCL")
    );
}

#[test]
fn unknown_game_resolves_to_none() {
    let table = rows(&[("apex", "Apex Legends")]);
    assert_eq!(
        resolve_alias(&normalize_game_name("Valorant"), &table),
        None
    );
    assert_eq!(resolve_alias(&normalize_game_name(""), &table), None);
    assert_eq!(resolve_alias(&normalize_game_name("Valorant"), &[]), None);
}

#[test]
fn stored_keys_match_in_any_case_or_spelling() {
    let table = rows(&[("APEX LEGENDS", "Apex Legends")]);
    assert_eq!(
        resolve_alias(&normalize_game_name("apex legends"), &table),
        Some("Apex Legends")
    );
    // Display strings are untouched: the stored target comes back verbatim.
    let table = rows(&[("apex", "APEX LEGENDS (Custom)")]);
    assert_eq!(
        resolve_alias(&normalize_game_name("Apex Vanilla"), &table),
        Some("APEX LEGENDS (Custom)")
    );
}

#[test]
fn alias_subject_tokens_accepted_others_refused() {
    assert!(applies_to("game_name"));
    assert!(applies_to("GAME_NAME"));
    assert!(applies_to("game"));
    assert!(applies_to("GAME"));
    assert!(applies_to("  game  "));
    assert!(!applies_to("owner"));
    assert!(!applies_to("game-name"));
    assert!(!applies_to("players"));
    assert!(!applies_to("count"));
    assert!(!applies_to(""));
    assert!(!applies_to("game_name_extra"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Resolution is a pure function of (input, table): reversing the row
    /// order never changes the output. Each target derives from its key's
    /// folded form, so rows whose keys fold together are interchangeable by
    /// construction; exact-match and strictly-longest-prefix selection are
    /// then order-independent for any table shape.
    #[test]
    fn property_resolve_order_independent(
        keys in proptest::collection::vec("[a-z0-9 ]{1,12}", 0..=6),
        input in "[a-z0-9 ]{0,16}",
    ) {
        let pairs: Vec<(String, String)> = keys
            .iter()
            .map(|k| (k.clone(), format!("Game {}", normalize_game_name(k))))
            .collect();
        let forward: Vec<AliasRow<'_>> = pairs
            .iter()
            .map(|(k, t)| AliasRow { key: k, target: t })
            .collect();
        let mut reversed = forward.clone();
        reversed.reverse();
        let normalized = normalize_game_name(&input);
        prop_assert_eq!(
            resolve_alias(&normalized, &forward),
            resolve_alias(&normalized, &reversed)
        );
    }
}
