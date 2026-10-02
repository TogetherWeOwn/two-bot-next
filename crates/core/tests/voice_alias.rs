//! Hermetic V7a acceptance cases against the public alias and `/nick` core API.

use proptest::prelude::*;
use two_bot_core::voice_alias::{
    fold_name, owner_display, parse_nick_command, resolve_game, validate_nick, AliasEntry,
    AliasError, AliasField, AliasTable, NickError, NickUpdate, MAX_ALIASES_PER_GUILD,
    MAX_ALIAS_KEY_CHARS, MAX_ALIAS_TARGET_CHARS, MAX_ERROR_ECHO_CHARS, MAX_NICK_CHARS,
};

fn table(pairs: &[(&str, &str)]) -> AliasTable {
    AliasTable::from_entries(pairs.iter().copied()).expect("valid fixture table")
}

fn entry(key: &str, target: &str) -> AliasEntry {
    AliasEntry {
        key: key.to_owned(),
        target: target.to_owned(),
    }
}

/// The invariant the chain rule exists for: every target resolves to itself.
fn assert_single_hop(table: &AliasTable) {
    for entry in table.entries() {
        assert_eq!(
            resolve_game(&entry.target, table),
            entry.target,
            "{table:?}"
        );
    }
}

#[test]
fn add_stores_trimmed_halves_in_order() {
    let mut aliases = AliasTable::new();
    assert!(aliases.is_empty());
    let added = aliases.add("  PUBG  ", "\tBattlegrounds\n").unwrap();
    assert_eq!(added, &entry("PUBG", "Battlegrounds"));
    aliases.add("r5apex", "Apex Legends").unwrap();
    assert_eq!(
        aliases.entries(),
        &[
            entry("PUBG", "Battlegrounds"),
            entry("r5apex", "Apex Legends")
        ]
    );
    assert_eq!(aliases.len(), 2);
}

#[test]
fn blank_halves_are_refused_after_trimming() {
    let mut aliases = AliasTable::new();
    for blank in ["", " ", "\t\n", "\u{3000}", "\u{a0}\u{2003}"] {
        assert_eq!(
            aliases.add(blank, "Game"),
            Err(AliasError::Empty(AliasField::Key)),
            "{blank:?}"
        );
        assert_eq!(
            aliases.add("Game", blank),
            Err(AliasError::Empty(AliasField::Target)),
            "{blank:?}"
        );
        assert_eq!(
            aliases.edit(blank, "Game"),
            Err(AliasError::Empty(AliasField::Key))
        );
        assert_eq!(
            aliases.remove(blank),
            Err(AliasError::Empty(AliasField::Key))
        );
    }
    assert!(aliases.is_empty());
}

#[test]
fn length_limits_count_unicode_scalars_after_trimming() {
    let mut aliases = AliasTable::new();
    let max_key = "é".repeat(MAX_ALIAS_KEY_CHARS);
    let max_target = "界".repeat(MAX_ALIAS_TARGET_CHARS);
    aliases
        .add(&format!("  {max_key}  "), &format!(" {max_target} "))
        .unwrap();

    let long_key = "k".repeat(MAX_ALIAS_KEY_CHARS + 1);
    assert_eq!(
        aliases.add(&long_key, "Game"),
        Err(AliasError::TooLong {
            field: AliasField::Key,
            chars: MAX_ALIAS_KEY_CHARS + 1,
            max: MAX_ALIAS_KEY_CHARS,
        })
    );
    let long_target = "😀".repeat(MAX_ALIAS_TARGET_CHARS + 1);
    assert_eq!(
        aliases.add("Game", &long_target),
        Err(AliasError::TooLong {
            field: AliasField::Target,
            chars: MAX_ALIAS_TARGET_CHARS + 1,
            max: MAX_ALIAS_TARGET_CHARS,
        })
    );
    assert_eq!(
        aliases.edit(&max_key, &long_target),
        Err(AliasError::TooLong {
            field: AliasField::Target,
            chars: MAX_ALIAS_TARGET_CHARS + 1,
            max: MAX_ALIAS_TARGET_CHARS,
        })
    );
    assert!(matches!(
        aliases.remove(&long_key),
        Err(AliasError::TooLong {
            field: AliasField::Key,
            ..
        })
    ));
    assert_eq!(aliases.len(), 1);
}

#[test]
fn control_characters_and_line_breaks_are_refused_in_both_halves() {
    let mut aliases = AliasTable::new();
    for bad in [
        "Apex\nLegends",
        "Apex\r\nLegends",
        "Apex\u{0}",
        "Apex\u{7f}",
        "Apex\u{85}Legends",
        "Apex\u{2028}Legends",
        "Apex\u{2029}Legends",
        "Apex\u{202e}Legends",
        "Apex\u{2067}Legends",
        "Apex\tLegends",
    ] {
        assert_eq!(
            aliases.add(bad, "Game"),
            Err(AliasError::ControlCharacter(AliasField::Key)),
            "{bad:?}"
        );
        assert_eq!(
            aliases.add("Game", bad),
            Err(AliasError::ControlCharacter(AliasField::Target)),
            "{bad:?}"
        );
    }
    // Emoji joiners and variation selectors are ordinary text.
    aliases
        .add("Family", "👨\u{200d}👩\u{200d}👧 night")
        .unwrap();
    aliases.add("Heart", "❤\u{fe0f} club").unwrap();
}

#[test]
fn keys_are_unique_regardless_of_case() {
    let mut aliases = table(&[("Apex Legends", "Apex")]);
    for duplicate in ["apex legends", "APEX LEGENDS", "  aPeX lEgEnDs "] {
        assert_eq!(
            aliases.add(duplicate, "Other"),
            Err(AliasError::DuplicateKey {
                existing: "Apex Legends".to_owned()
            })
        );
    }
    // Canonically equivalent spellings collide too.
    let mut accented = table(&[("Pok\u{e9}mon", "Pokemon")]);
    assert_eq!(
        accented.add("POKE\u{301}MON", "Other"),
        Err(AliasError::DuplicateKey {
            existing: "Pok\u{e9}mon".to_owned()
        })
    );
    assert_eq!(
        AliasTable::from_entries([("Doom", "A"), ("DOOM", "B")]),
        Err(AliasError::DuplicateKey {
            existing: "Doom".to_owned()
        })
    );
}

#[test]
fn table_is_capped_per_guild() {
    let mut aliases =
        AliasTable::from_entries((0..MAX_ALIASES_PER_GUILD).map(|i| (format!("game {i}"), "G")))
            .unwrap();
    assert_eq!(aliases.len(), MAX_ALIASES_PER_GUILD);
    assert_eq!(
        aliases.add("one more", "G"),
        Err(AliasError::TableFull {
            max: MAX_ALIASES_PER_GUILD
        })
    );
    // A duplicate in a full table reports the duplicate, not the cap.
    assert!(matches!(
        aliases.add("GAME 0", "G"),
        Err(AliasError::DuplicateKey { .. })
    ));
    // Editing works at the cap, and removing frees a slot.
    aliases.edit("game 5", "H").unwrap();
    aliases.remove("game 0").unwrap();
    aliases.add("one more", "G").unwrap();
    assert_eq!(aliases.len(), MAX_ALIASES_PER_GUILD);
    assert!(
        AliasTable::from_entries((0..=MAX_ALIASES_PER_GUILD).map(|i| (format!("g{i}"), "G")))
            .is_err()
    );
}

#[test]
fn edit_replaces_target_and_keeps_key_spelling_and_order() {
    let mut aliases = table(&[("PUBG", "Battlegrounds"), ("r5apex", "Apex")]);
    let previous = aliases.edit(" pubg ", "  PUBG: Battlegrounds ").unwrap();
    assert_eq!(previous, entry("PUBG", "Battlegrounds"));
    assert_eq!(
        aliases.entries(),
        &[
            entry("PUBG", "PUBG: Battlegrounds"),
            entry("r5apex", "Apex")
        ]
    );
    assert_eq!(
        aliases.edit("Fortnite", "Battle Royale"),
        Err(AliasError::UnknownKey {
            key: "Fortnite".to_owned()
        })
    );
}

#[test]
fn remove_matches_any_case_and_returns_the_entry() {
    let mut aliases = table(&[("PUBG", "Battlegrounds"), ("r5apex", "Apex")]);
    assert_eq!(aliases.remove("  R5APEX "), Ok(entry("r5apex", "Apex")));
    assert_eq!(aliases.entries(), &[entry("PUBG", "Battlegrounds")]);
    assert_eq!(
        aliases.remove("r5apex"),
        Err(AliasError::UnknownKey {
            key: "r5apex".to_owned()
        })
    );
}

#[test]
fn chain_rule_refuses_a_target_that_is_another_aliased_key() {
    let mut aliases = table(&[("PUBG", "Battlegrounds")]);
    assert_eq!(
        aliases.add("PlayerUnknown", "pubg"),
        Err(AliasError::TargetIsKey {
            key: "PUBG".to_owned()
        })
    );
    // Editing into a chain is refused the same way.
    aliases.add("Fortnite", "Battle Royale").unwrap();
    assert_eq!(
        aliases.edit("Fortnite", "PUBG"),
        Err(AliasError::TargetIsKey {
            key: "PUBG".to_owned()
        })
    );
    assert_eq!(aliases.entries()[1], entry("Fortnite", "Battle Royale"));
    assert_single_hop(&aliases);
}

#[test]
fn chain_rule_refuses_a_key_that_is_another_entrys_target() {
    let mut aliases = table(&[("PUBG", "Battlegrounds")]);
    assert_eq!(
        aliases.add("BATTLEGROUNDS", "Shooter"),
        Err(AliasError::KeyIsTarget {
            key: "PUBG".to_owned()
        })
    );
    // The two-entry cycle A -> B, B -> A cannot be built in either order.
    assert!(AliasTable::from_entries([("A", "B"), ("B", "A")]).is_err());
    assert!(AliasTable::from_entries([("B", "A"), ("A", "B")]).is_err());
    assert_single_hop(&aliases);
}

#[test]
fn chain_rule_allows_case_corrections_and_shared_canonical_names() {
    // A key may alias to itself in another case.
    let mut aliases = table(&[("APEX LEGENDS", "Apex Legends")]);
    // Other spellings may point at the exact same canonical name...
    aliases.add("r5apex", "Apex Legends").unwrap();
    // ...and a new key may name an existing target with the same alias.
    let mut shared = table(&[("r5apex", "Apex Legends")]);
    shared.add("apex legends", "Apex Legends").unwrap();
    // A different spelling of the canonical name would add a second hop.
    assert_eq!(
        aliases.add("Apex Beta", "apex legends"),
        Err(AliasError::TargetIsKey {
            key: "APEX LEGENDS".to_owned()
        })
    );
    // Retargeting the case correction while others share its name is refused.
    assert_eq!(
        aliases.edit("APEX LEGENDS", "Apex"),
        Err(AliasError::KeyIsTarget {
            key: "r5apex".to_owned()
        })
    );
    // An exact no-op alias is harmless.
    aliases.add("Doom", "Doom").unwrap();
    for t in [&aliases, &shared] {
        assert_single_hop(t);
    }
    assert_eq!(resolve_game("R5APEX", &aliases), "Apex Legends");
    assert_eq!(resolve_game("apex legends", &aliases), "Apex Legends");
}

#[test]
fn errors_never_echo_more_than_the_documented_limit() {
    let huge = "x".repeat(10_000);
    let mut aliases = table(&[("PUBG", "Battlegrounds")]);
    let mut errors = vec![
        aliases.add(&huge, "Game").unwrap_err(),
        aliases.add("Game", &huge).unwrap_err(),
        aliases.edit(&huge, "Game").unwrap_err(),
        aliases.remove(&huge).unwrap_err(),
        aliases.add(&format!("{huge}\n"), "Game").unwrap_err(),
    ];
    let max_key = "y".repeat(MAX_ALIAS_KEY_CHARS);
    errors.push(aliases.remove(&max_key).unwrap_err());
    errors.push(aliases.edit(&max_key, "Game").unwrap_err());
    let mut chained = table(&[(max_key.as_str(), "Target")]);
    errors.push(chained.add("Other", &max_key).unwrap_err());
    errors.push(chained.add("TARGET", "Other").unwrap_err());
    errors.push(chained.add(&max_key.to_uppercase(), "Z").unwrap_err());
    for error in errors {
        let echoed = match &error {
            AliasError::DuplicateKey { existing } => existing.chars().count(),
            AliasError::UnknownKey { key }
            | AliasError::TargetIsKey { key }
            | AliasError::KeyIsTarget { key } => key.chars().count(),
            _ => 0,
        };
        assert!(echoed <= MAX_ERROR_ECHO_CHARS, "{error:?}");
        let message = error.to_string();
        assert!(!message.contains(&"x".repeat(MAX_ERROR_ECHO_CHARS + 1)));
        assert!(
            message.chars().count() <= MAX_ERROR_ECHO_CHARS + 80,
            "{message}"
        );
        assert!(!message.chars().any(char::is_control), "{message:?}");
    }
}

#[test]
fn resolve_game_matches_keys_exactly_ignoring_case() {
    let aliases = table(&[
        ("PLAYERUNKNOWN'S BATTLEGROUNDS", "PUBG"),
        ("Pok\u{e9}mon Violet", "Pokémon"),
        ("Apex Legends", "Apex"),
    ]);
    assert_eq!(
        resolve_game("playerunknown's battlegrounds", &aliases),
        "PUBG"
    );
    assert_eq!(resolve_game("Apex Legends", &aliases), "Apex");
    assert_eq!(resolve_game("APEX LEGENDS", &aliases), "Apex");
    // Surrounding whitespace is trimmed before matching.
    assert_eq!(resolve_game("  Apex Legends\t", &aliases), "Apex");
    // Canonically equivalent (NFC) spellings match.
    assert_eq!(resolve_game("POKE\u{301}MON VIOLET", &aliases), "Pokémon");
    // Not a substring, prefix or fuzzy match.
    for raw in ["Apex", "Apex Legends Beta", "Apex  Legends", "Legends"] {
        assert_eq!(resolve_game(raw, &aliases), raw);
    }
    // Compatibility forms are not folded (no NFKC): full-width stays distinct.
    let fullwidth = "Ａｐｅｘ Ｌｅｇｅｎｄｓ";
    assert_eq!(resolve_game(fullwidth, &aliases), fullwidth);
}

#[test]
fn resolve_game_passes_unknown_names_through_unchanged() {
    let aliases = table(&[("PUBG", "Battlegrounds")]);
    for raw in ["Minecraft", "  Minecraft  ", "", "   ", "pubg\n"] {
        let resolved = resolve_game(raw, &aliases);
        if raw.trim().eq_ignore_ascii_case("pubg") {
            assert_eq!(resolved, "Battlegrounds");
        } else {
            assert_eq!(resolved, raw, "{raw:?}");
        }
    }
    assert_eq!(resolve_game("Anything", &AliasTable::new()), "Anything");
}

#[test]
fn fold_name_is_trim_lowercase_nfc_only() {
    assert_eq!(fold_name("  Apex Legends "), "apex legends");
    assert_eq!(fold_name("E\u{301}clair"), "\u{e9}clair");
    assert_eq!(fold_name("\u{c9}CLAIR"), "\u{e9}clair");
    assert_eq!(fold_name("a  b"), "a  b");
    assert_eq!(fold_name("ﬁfa"), "ﬁfa");
    assert_eq!(fold_name("Ｇ"), "ｇ");
}

#[test]
fn table_refusals_leave_the_table_unchanged() {
    let original = table(&[("PUBG", "Battlegrounds"), ("APEX", "Apex")]);
    let mut aliases = original.clone();
    let _ = aliases.add("pubg", "x");
    let _ = aliases.add("Fortnite", "pubg");
    let _ = aliases.add("battlegrounds", "x");
    let _ = aliases.edit("missing", "x");
    let _ = aliases.edit("APEX", "PUBG");
    let _ = aliases.remove("missing");
    assert_eq!(aliases, original);
}

#[test]
fn validate_nick_trims_and_bounds_length() {
    assert_eq!(validate_nick("  Rick  ").unwrap().as_str(), "Rick");
    assert_eq!(validate_nick("x").unwrap().into_inner(), "x");
    let max = "ß".repeat(MAX_NICK_CHARS);
    assert_eq!(validate_nick(&format!(" {max} ")).unwrap().as_str(), max);
    assert_eq!(
        validate_nick(&"a".repeat(MAX_NICK_CHARS + 1)),
        Err(NickError::TooLong {
            chars: MAX_NICK_CHARS + 1,
            max: MAX_NICK_CHARS
        })
    );
    for blank in ["", "   ", "\u{3000}", "\t"] {
        assert_eq!(validate_nick(blank), Err(NickError::Empty), "{blank:?}");
    }
    // Interior text is not normalised.
    assert_eq!(validate_nick("A  B").unwrap().as_ref(), "A  B");
}

#[test]
fn validate_nick_refuses_control_characters_and_line_breaks() {
    for bad in [
        "a\nb",
        "a\rb",
        "a\tb",
        "a\u{0}b",
        "a\u{1b}[31mb",
        "a\u{7f}",
        "a\u{85}b",
        "a\u{9b}b",
        "a\u{2028}b",
        "a\u{2029}b",
        "a\u{202e}b",
        "a\u{2066}b",
    ] {
        assert_eq!(
            validate_nick(bad),
            Err(NickError::ControlCharacter),
            "{bad:?}"
        );
    }
    // Trailing newlines are just trimmed whitespace.
    assert_eq!(validate_nick("Rick\n").unwrap().as_str(), "Rick");
    assert!(validate_nick("👩\u{200d}🚀 pilot").is_ok());
}

#[test]
fn validate_nick_refuses_mass_mentions_and_mention_syntax() {
    for bad in [
        "@everyone",
        "@here",
        "hi @everyone",
        "@EVERYONE",
        "@Here!",
        "<@123>",
        "<@!123>",
        "<@&456>",
        "x<@1>y",
        "<@<@99>",
        // Mass mentions are refused anywhere in the name, even mid-word.
        "mail@here.example",
    ] {
        assert_eq!(validate_nick(bad), Err(NickError::Mention), "{bad:?}");
    }
    for ok in [
        "@rick",
        "every@one",
        "<@>",
        "<@abc>",
        "<@ 12>",
        "<@12",
        "<#123>",
        "here@",
    ] {
        assert!(validate_nick(ok).is_ok(), "{ok:?}");
    }
}

#[test]
fn nick_errors_never_echo_input() {
    let secret = "SECRETVALUE";
    for raw in [
        format!("{secret}{}", "a".repeat(MAX_NICK_CHARS)),
        format!("{secret}\n{secret}"),
        format!("{secret} @everyone"),
    ] {
        let message = validate_nick(&raw).unwrap_err().to_string();
        assert!(!message.contains(secret), "{message}");
    }
}

#[test]
fn nick_command_supports_reset() {
    for reset in ["reset", " RESET ", "Reset"] {
        assert_eq!(
            parse_nick_command(reset),
            Ok(NickUpdate::Reset),
            "{reset:?}"
        );
    }
    assert_eq!(
        parse_nick_command(" Captain "),
        Ok(NickUpdate::Set(validate_nick("Captain").unwrap()))
    );
    assert!(matches!(
        parse_nick_command("resetting"),
        Ok(NickUpdate::Set(_))
    ));
    assert_eq!(parse_nick_command(""), Err(NickError::Empty));
    assert_eq!(parse_nick_command("@here"), Err(NickError::Mention));
}

#[test]
fn owner_display_prefers_a_valid_nick() {
    assert_eq!(owner_display(Some("Captain"), "Rick"), "Captain");
    assert_eq!(owner_display(Some("  Captain "), "Rick"), "Captain");
    assert_eq!(owner_display(None, "Rick"), "Rick");
    // A stored value that no longer validates falls back to the display name.
    let too_long = "n".repeat(MAX_NICK_CHARS + 1);
    for stale in ["", "   ", "@everyone", "a\nb", too_long.as_str()] {
        assert_eq!(owner_display(Some(stale), "Rick"), "Rick", "{stale:?}");
    }
}

#[derive(Debug, Clone)]
enum Op {
    Add(usize, usize),
    Edit(usize, usize),
    Remove(usize),
}

/// A small pool with case, NFC, whitespace and full-width collisions so random
/// sequences exercise every chain and duplicate path.
const POOL: &[&str] = &[
    "Apex",
    "apex",
    "APEX",
    " Apex ",
    "Doom",
    "doom",
    "Zelda",
    "r5apex",
    "Ｄoom",
    "Pok\u{e9}mon",
    "POKE\u{301}MON",
    "Battlegrounds",
    "pubg",
];

fn op() -> impl Strategy<Value = Op> {
    let index = 0..POOL.len();
    prop_oneof![
        (index.clone(), index.clone()).prop_map(|(k, t)| Op::Add(k, t)),
        (index.clone(), index.clone()).prop_map(|(k, t)| Op::Edit(k, t)),
        index.prop_map(Op::Remove),
    ]
}

fn assert_table_invariants(aliases: &AliasTable) {
    assert!(aliases.len() <= MAX_ALIASES_PER_GUILD);
    let keys: Vec<String> = aliases
        .entries()
        .iter()
        .map(|e| fold_name(&e.key))
        .collect();
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        keys.len(),
        "duplicate folded keys: {aliases:?}"
    );
    assert_single_hop(aliases);
    // No chain of any length: resolving twice equals resolving once.
    for raw in POOL
        .iter()
        .copied()
        .chain(aliases.entries().iter().map(|e| e.key.as_str()))
    {
        let once = resolve_game(raw, aliases);
        assert_eq!(resolve_game(once, aliases), once, "{raw:?} in {aliases:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// No sequence of add, edit and remove creates an alias chain or cycle, and
    /// a refused operation leaves the table unchanged.
    #[test]
    fn property_no_operation_sequence_creates_a_cycle(ops in prop::collection::vec(op(), 0..40)) {
        let mut aliases = AliasTable::new();
        for op in ops {
            let before = aliases.clone();
            let refused = match op {
                Op::Add(k, t) => aliases.add(POOL[k], POOL[t]).is_err(),
                Op::Edit(k, t) => aliases.edit(POOL[k], POOL[t]).is_err(),
                Op::Remove(k) => aliases.remove(POOL[k]).is_err(),
            };
            if refused {
                prop_assert_eq!(&aliases, &before);
            }
            assert_table_invariants(&aliases);
        }
    }

    /// Resolving any canonical name in any reachable table returns it exactly.
    #[test]
    fn property_resolving_a_canonical_name_returns_it(
        pairs in prop::collection::vec(("[a-cA-C ]{1,4}", "[a-cA-C ]{1,4}"), 0..16),
    ) {
        let mut aliases = AliasTable::new();
        for (key, target) in &pairs {
            let _ = aliases.add(key, target);
        }
        for entry in aliases.entries() {
            prop_assert_eq!(resolve_game(&entry.target, &aliases), entry.target.as_str());
        }
        for (_, target) in &pairs {
            let resolved = resolve_game(target, &aliases);
            prop_assert_eq!(resolve_game(resolved, &aliases), resolved);
        }
    }

    /// A validated nick never contains a control character, a line break, a
    /// mass mention or surrounding whitespace, and stays within the limit.
    #[test]
    fn property_validated_nick_has_no_control_character(
        raw in prop_oneof![
            any::<String>(),
            "[ a@<>!&0-9\\n\\r\\t\\x00\\x1b\\u{85}\\u{2028}\\u{202e}eEvryonhH]{0,40}",
        ],
    ) {
        if let Ok(nick) = validate_nick(&raw) {
            let text = nick.as_str();
            prop_assert!(!text.chars().any(char::is_control));
            // Hoisted: `prop_assert!` formats `stringify!(cond)` into the
            // failure message, so inline `\u{...}` escapes would parse as
            // format placeholders and fail to compile.
            let separators: &[char] = &['\u{2028}', '\u{2029}', '\u{202e}'];
            prop_assert!(!text.contains(separators));
            prop_assert!((1..=MAX_NICK_CHARS).contains(&text.chars().count()));
            prop_assert_eq!(text, text.trim());
            let lower = text.to_lowercase();
            prop_assert!(!lower.contains("@everyone") && !lower.contains("@here"));
            prop_assert_eq!(owner_display(Some(raw.as_str()), "fallback"), text);
        } else {
            prop_assert_eq!(owner_display(Some(raw.as_str()), "fallback"), "fallback");
        }
    }
}
