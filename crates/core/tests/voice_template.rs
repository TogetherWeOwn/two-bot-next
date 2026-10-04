//! V6 acceptance for the composed template policy: every styling mode through
//! the naming engine, nested fallback chains, stable random positions and the
//! naming fallback contract.

use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_naming::{
    majority_games, parse, render, resolve_majority_game, GameOptions, RoomContext,
    DEFAULT_FALLBACK_NAME, MAX_NAME_LEN,
};
use two_bot_core::voice_style::{apply_chain, parse_modes};
use two_bot_core::voice_template::{resolve_room_name, TemplateExtensions};

const BODY: &str = "Hello, the big World";

/// Every mode spelling the spec lists, with `caps` as the `upper` alias.
const MODES: &[&str] = &[
    "upper",
    "caps",
    "lower",
    "title",
    "swap",
    "scaps",
    "rand",
    "spaces",
    "acro",
    "remshort",
    "2w",
    "uwu",
    "usd",
    "bold",
    "italic",
    "bolditalic",
    "script",
    "boldscript",
    "fraktur",
    "boldfraktur",
    "double",
    "sans",
    "boldsans",
    "italicsans",
    "bolditalicsans",
    "mono",
];

fn room(seed: u64) -> RoomContext {
    RoomContext {
        owner_name: "Alex".to_string(),
        original_creator_name: "Alex".to_string(),
        member_count: 1,
        owner_present: true,
        room_number: 1,
        seed,
        ..RoomContext::default()
    }
}

fn v6(template: &str, ctx: &RoomContext, facts: &ConditionFacts) -> String {
    render(&parse(template), ctx, &TemplateExtensions::new(facts))
}

fn styled(mode: &str, body: &str) -> String {
    v6(
        &format!("\"\"{mode}:{body}\"\""),
        &room(7),
        &ConditionFacts::default(),
    )
}

#[test]
fn every_mode_styles_its_body_through_the_engine() {
    for mode in MODES {
        let actual = styled(mode, BODY);
        if *mode == "rand" {
            assert_eq!(
                actual.to_lowercase(),
                BODY.to_lowercase(),
                "rand keeps letters"
            );
            continue;
        }
        // Only `rand` reads the seed, so any seed gives the library's output.
        let expected = apply_chain(&parse_modes(mode), BODY, 0);
        assert_eq!(actual, expected, "mode {mode}");
        assert_ne!(actual, BODY, "mode {mode} changes the body");
    }
}

#[test]
fn case_and_word_modes_render_exactly() {
    let cases = [
        ("upper", "HELLO, THE BIG WORLD"),
        ("caps", "HELLO, THE BIG WORLD"),
        ("lower", "hello, the big world"),
        ("title", "Hello, The Big World"),
        ("swap", "hELLO, THE BIG wORLD"),
        (
            "scaps",
            "H\u{1D07}\u{29F}\u{29F}\u{1D0F}, \u{1D1B}\u{29C}\u{1D07} \
             \u{299}\u{26A}\u{262} W\u{1D0F}\u{280}\u{29F}\u{1D05}",
        ),
        ("spaces", "H e l l o ,   t h e   b i g   W o r l d"),
        ("acro", "HtbW"),
        ("remshort", "Hello, big World"),
        ("2w", "Hello, the"),
        ("uwu", "Hewwo, the big Wowwd"),
    ];
    for (mode, expected) in cases {
        assert_eq!(styled(mode, BODY), expected, "mode {mode}");
    }
}

#[test]
fn font_modes_use_the_unicode_tables() {
    assert_eq!(styled("bold", "Ab1"), "\u{1D400}\u{1D41B}\u{1D7CF}");
    assert_eq!(styled("mono", "Ab1"), "\u{1D670}\u{1D68B}\u{1D7F7}");
    assert_eq!(styled("italic", "Ah"), "\u{1D434}\u{210E}");
    // Letterlike capitals and digits where the font has them.
    assert_eq!(styled("script", "Be"), "\u{212C}\u{212F}");
    assert_eq!(styled("double", "C1"), "\u{2102}\u{1D7D9}");
    // Fonts without digits leave them unchanged.
    assert_eq!(styled("fraktur", "7"), "7");
}

#[test]
fn chains_apply_left_to_right_and_unknown_modes_are_inert() {
    assert_eq!(styled("upper+bold", "ab"), "\u{1D400}\u{1D401}");
    // Font letters have no case mapping, so order matters.
    assert_eq!(styled("bold+upper", "ab"), "\u{1D41A}\u{1D41B}");
    assert_eq!(styled("sparkle", "hi"), "hi");
    assert_eq!(styled("upper+sparkle", "hi"), "HI");
    assert_eq!(styled("CAPS", "hi"), "HI");
}

#[test]
fn rand_case_is_seeded_by_the_room_only() {
    let template = "\"\"rand:room name\"\"";
    let solo = room(11);
    let busy = RoomContext {
        member_count: 4,
        owner_name: "Sam".to_string(),
        room_number: 9,
        ..room(11)
    };
    let live = ConditionFacts {
        owner_live_discord: true,
        ..ConditionFacts::default()
    };
    let first = v6(template, &solo, &ConditionFacts::default());
    assert_eq!(first.to_lowercase(), "room name");
    assert_eq!(first, v6(template, &solo, &ConditionFacts::default()));
    assert_eq!(
        first,
        v6(template, &busy, &live),
        "membership never re-rolls"
    );

    let distinct: std::collections::BTreeSet<String> = (0..32)
        .map(|seed| v6(template, &room(seed), &ConditionFacts::default()))
        .collect();
    assert!(distinct.len() > 1, "the room seed drives the case pattern");
}

#[test]
fn nested_fallback_chain_resolves_role_then_live_then_default() {
    let template =
        "{{ROLE:raid ??Raid crew//{{LIVE ??\"\"upper:@@owner@@\"\" live//@@owner@@'s room}}}}";
    let ctx = room(3);
    let role = vec!["raid".to_string()];
    let facts = |owner_role_ids: Vec<String>, owner_live_discord: bool| ConditionFacts {
        owner_role_ids,
        owner_live_discord,
        ..ConditionFacts::default()
    };
    assert_eq!(v6(template, &ctx, &facts(role.clone(), true)), "Raid crew");
    assert_eq!(v6(template, &ctx, &facts(role, false)), "Raid crew");
    assert_eq!(v6(template, &ctx, &facts(Vec::new(), true)), "ALEX live");
    assert_eq!(v6(template, &ctx, &facts(Vec::new(), false)), "Alex's room");
}

#[test]
fn styling_applies_after_conditionals_and_tokens() {
    let template = "\"\"upper:{{ROLE:raid ??@@owner@@'s crew//solo}}\"\"";
    let ctx = room(5);
    let crew = ConditionFacts {
        owner_role_ids: vec!["raid".to_string()],
        ..ConditionFacts::default()
    };
    assert_eq!(v6(template, &ctx, &crew), "ALEX'S CREW");
    assert_eq!(v6(template, &ctx, &ConditionFacts::default()), "SOLO");
}

#[test]
fn styled_random_picks_keep_their_positions() {
    let styled = "[[a/b/c]] \"\"upper:[[x/y/z]]\"\" [[p/q]]";
    let plain = "[[a/b/c]] [[x/y/z]] [[p/q]]";
    let facts = ConditionFacts::default();
    for seed in 0..64 {
        let ctx = room(seed);
        let styled = v6(styled, &ctx, &facts);
        let plain = v6(plain, &ctx, &facts);
        let styled: Vec<&str> = styled.split(' ').collect();
        let plain: Vec<&str> = plain.split(' ').collect();
        assert_eq!(styled[0], plain[0], "seed {seed}");
        assert_eq!(styled[1], plain[1].to_uppercase(), "seed {seed}");
        assert_eq!(styled[2], plain[2], "seed {seed}");
    }
}

#[test]
fn a_condition_flip_never_rerolls_later_picks() {
    let template = "{{LIVE ??[[a/b/c]]//\"\"upper:[[d/e]]\"\"}} [[p/q/r/s]]";
    let live = ConditionFacts {
        owner_live_external: true,
        ..ConditionFacts::default()
    };
    for seed in 0..64 {
        let ctx = room(seed);
        let on = v6(template, &ctx, &live);
        let off = v6(template, &ctx, &ConditionFacts::default());
        assert_eq!(on.split(' ').nth(1), off.split(' ').nth(1), "seed {seed}");
        assert!(off.split(' ').next().is_some_and(|w| w == "D" || w == "E"));
    }
}

#[test]
fn truncation_counts_styled_characters() {
    let name = styled("bold", &"a".repeat(MAX_NAME_LEN + 20));
    assert_eq!(name.chars().count(), MAX_NAME_LEN);
    assert!(name.chars().all(|c| c == '\u{1D41A}'));
}

#[test]
fn resolve_room_name_keeps_the_fallback_contract() {
    let ctx = room(1);
    let facts = ConditionFacts::default();
    assert_eq!(
        resolve_room_name("\"\"remshort:the of a\"\"", &ctx, &facts, "Hangout"),
        "Hangout"
    );
    assert_eq!(
        resolve_room_name("\"\"0w:anything\"\"", &ctx, &facts, "Hangout"),
        "Hangout"
    );
    assert_eq!(
        resolve_room_name("{{LIVE ??live}}", &ctx, &facts, "Hangout"),
        "Hangout"
    );
    assert_eq!(resolve_room_name("  ", &ctx, &facts, "Hangout"), "Hangout");
    assert_eq!(
        resolve_room_name(&"x".repeat(4097), &ctx, &facts, "Hangout"),
        "Hangout"
    );
    assert_eq!(
        resolve_room_name("{{LIVE ??live}}", &ctx, &facts, "  "),
        DEFAULT_FALLBACK_NAME
    );
    assert_eq!(
        resolve_room_name("\"\"upper:@@owner@@\"\" ##", &ctx, &facts, "Hangout"),
        "ALEX #1"
    );
}

#[test]
fn majority_games_match_the_shown_title() {
    fn games(
        titles: &[Option<&str>],
        owner: Option<&str>,
        options: &GameOptions,
    ) -> (Vec<String>, String) {
        let activities: Vec<Option<String>> =
            titles.iter().map(|t| t.map(str::to_string)).collect();
        (
            majority_games(&activities, owner, options),
            resolve_majority_game(&activities, owner, options),
        )
    }
    let options = GameOptions::default();
    assert_eq!(
        games(&[None], None, &options),
        (Vec::<String>::new(), "General".to_string())
    );
    assert_eq!(
        games(&[Some("Apex"), Some("Apex"), Some("Chess")], None, &options),
        (vec!["Apex".to_string()], "Apex".to_string())
    );
    assert_eq!(
        games(&[Some("Chess"), Some("Apex")], None, &options),
        (
            vec!["Apex".to_string(), "Chess".to_string()],
            "Apex & Chess".to_string()
        )
    );
    assert_eq!(
        games(&[Some("Apex"), Some("Chess"), Some("Go")], None, &options),
        (Vec::<String>::new(), "General".to_string())
    );
    let single = GameOptions {
        force_single: true,
        ..GameOptions::default()
    };
    assert_eq!(
        games(&[Some("Apex"), Some("Chess")], Some("Chess"), &single),
        (vec!["Chess".to_string()], "Chess".to_string())
    );

    // The GAME condition and @@game_name@@ read the same resolution.
    let activities = vec![Some("Long Title".to_string()), None];
    let aliased = GameOptions {
        aliases: [("Long Title".to_string(), "Apex".to_string())].into(),
        ..GameOptions::default()
    };
    let ctx = RoomContext {
        game_name: resolve_majority_game(&activities, None, &aliased),
        ..room(2)
    };
    let facts = ConditionFacts {
        games: majority_games(&activities, None, &aliased),
        ..ConditionFacts::default()
    };
    assert_eq!(
        v6("{{GAME=apex ??@@game_name@@//none}}", &ctx, &facts),
        "Apex"
    );
}
