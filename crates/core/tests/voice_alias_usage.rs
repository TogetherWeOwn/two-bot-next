//! V7 usage acceptance: the alias-table lifecycle as the template engine uses
//! it (`@@game_name@@` / `GAME` conditions) and `/nick` display/reset as
//! `@@owner@@` uses it.
//!
//! Tests-only slice against the existing pure API in
//! `crates/core/src/voice_alias.rs`; no `src` changes. Complements
//! `crates/core/tests/voice_alias.rs`, which pins validation edges: this file
//! pins the alias-to-game-to-condition chain and the nick display/reset round
//! trip from `docs/voice-rooms.md` §V7.

use two_bot_core::voice_alias::{
    fold_name, owner_display, parse_nick_command, resolve_game, AliasTable, NickUpdate,
};

/// Add maps `resolve_game`, edit remaps, remove unmaps, and a canonical name
/// always resolves to itself (the single-hop invariant the chain rule exists
/// for).
#[test]
fn alias_lifecycle_drives_resolve_game() {
    let mut aliases = AliasTable::new();
    // No entry yet: the raw activity name passes through to `@@game_name@@`.
    assert_eq!(resolve_game("Apex Legends", &aliases), "Apex Legends");
    // Add maps every casing of the key onto the canonical target.
    aliases.add("r5apex", "Apex Legends").unwrap();
    assert_eq!(resolve_game("r5apex", &aliases), "Apex Legends");
    assert_eq!(resolve_game("R5APEX", &aliases), "Apex Legends");
    // Edit remaps the same key without touching its spelling or order.
    aliases.edit("R5APEX", "Apex").unwrap();
    assert_eq!(resolve_game("r5apex", &aliases), "Apex");
    assert_eq!(aliases.entries().len(), 1);
    // Remove unmaps: the raw name passes through again, exactly as given.
    aliases.remove("r5apex").unwrap();
    assert_eq!(resolve_game("r5apex", &aliases), "r5apex");
    assert!(aliases.is_empty());
}

/// `GAME` conditions compare folded names, so the folded resolved game must
/// equal the folded canonical target however the player typed the activity.
#[test]
fn game_condition_folding_uses_folded_resolved_names() {
    let mut aliases = AliasTable::new();
    aliases.add("PUBG", "Battlegrounds").unwrap();
    // `GAME = battlegrounds` (exact on folded forms) matches via the alias.
    let resolved = resolve_game("pubg", &aliases);
    assert_eq!(fold_name(resolved), fold_name("BATTLEGROUNDS"));
    // NFC-equivalent raw spellings fold onto the same game too.
    aliases.add("Pokémon Violet", "Pokémon").unwrap();
    assert_eq!(
        fold_name(resolve_game("POKÉMON VIOLET", &aliases)),
        fold_name("pokémon")
    );
    // An unaliased activity folds to itself: no false `GAME` match.
    assert_eq!(
        fold_name(resolve_game("Minecraft", &aliases)),
        fold_name("Minecraft")
    );
    // A canonical target resolves to itself, so `GAME` sees one stable name.
    assert_eq!(resolve_game("Battlegrounds", &aliases), "Battlegrounds");
}

/// `@@owner@@` shows the `/nick` name when one is stored and the display name
/// otherwise; `reset` restores the display name.
#[test]
fn nick_set_and_reset_drive_owner_display() {
    // Set: the stored nick wins over the display name.
    let set = parse_nick_command(" Captain ").unwrap();
    assert!(matches!(set, NickUpdate::Set(_)));
    let stored = match parse_nick_command("Captain").unwrap() {
        NickUpdate::Set(nick) => nick.into_inner(),
        NickUpdate::Reset => unreachable!("Captain is not the reset keyword"),
    };
    assert_eq!(owner_display(Some(&stored), "Rick"), "Captain");
    // No nick stored: the display name shows.
    assert_eq!(owner_display(None, "Rick"), "Rick");
    // Reset (any ASCII case, with whitespace) forgets the name.
    for raw in ["reset", " RESET ", "Reset"] {
        assert_eq!(parse_nick_command(raw), Ok(NickUpdate::Reset));
    }
    assert_eq!(owner_display(None, "Rick"), "Rick");
    // A stale stored value that no longer validates falls back to display.
    assert_eq!(owner_display(Some("@everyone"), "Rick"), "Rick");
}
