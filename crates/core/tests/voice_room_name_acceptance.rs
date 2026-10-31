//! Acceptance cases for the V3 `/name` decision parsers.
//!
//! Scope: pure functions in `crates/core/src/voice_room_name.rs`
//! (`is_literal_name`, `decide_custom_name`, `decide_template_name`) over
//! untrusted member/game text. This file is the acceptance companion to the
//! `voice_room_name` cargo-fuzz target (`fuzz/fuzz_targets/voice_room_name.rs`)
//! and its fixed regression mirror (`crates/core/tests/voice_room_name.rs`).
//! It never edits those files (owner 2026-10-12 pairing rule): all cases live
//! here.
//!
//! What this pins beyond the builder suite: the builder's hostile test drives
//! fixed render facts (`Ava` / `General`). The fuzzer's discovery surface is
//! hostile *render facts* — member-controlled owner/game text pulled into the
//! render by tokens such as `@@owner@@` — plus the full template-syntax
//! matrix. Every seed-corpus input appears below as an inline regression case
//! (no filesystem reads), each driven under plain and hostile facts with the
//! fuzz oracle asserted on every outcome.
//!
//! No database, Discord client, network or secret is touched here.

use two_bot_core::automod::{AutomodFilter, AutomodPolicy};
use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_name_filter::{NameFilterContext, MAX_CHANNEL_NAME_CHARS};
use two_bot_core::voice_naming::RoomContext;
use two_bot_core::voice_room_name::{
    decide_custom_name, decide_template_name, is_literal_name, NameChecks, NameRefusal,
    RenderFacts, MAX_CUSTOM_NAME_CHARS,
};

fn base_context() -> RoomContext {
    RoomContext {
        room_number: 3,
        owner_name: "Ava".to_owned(),
        original_creator_name: "Ava".to_owned(),
        member_count: 2,
        owner_present: true,
        user_limit: 8,
        game_name: "General".to_owned(),
        seed: 7,
        ..RoomContext::default()
    }
}

fn hostile_context(owner: &str, game: &str) -> RoomContext {
    RoomContext {
        owner_name: owner.to_owned(),
        original_creator_name: owner.to_owned(),
        game_name: game.to_owned(),
        ..base_context()
    }
}

fn filter() -> NameFilterContext {
    NameFilterContext {
        guild_id: "100".to_owned(),
        channel_id: "500".to_owned(),
        user_id: "300".to_owned(),
    }
}

struct Fixture {
    context: RoomContext,
    conditions: ConditionFacts,
    policy: AutomodPolicy,
    filter: NameFilterContext,
    others: Vec<String>,
    unique: bool,
}

impl Fixture {
    fn plain() -> Self {
        Self {
            context: base_context(),
            conditions: ConditionFacts::default(),
            policy: AutomodPolicy::default(),
            filter: filter(),
            others: Vec::new(),
            unique: false,
        }
    }

    fn with_context(context: RoomContext) -> Self {
        Self {
            context,
            ..Self::plain()
        }
    }

    fn strict() -> Self {
        Self {
            policy: words(&["blorp"]),
            unique: true,
            others: vec![
                "Lounge".to_owned(),
                "Ava".to_owned(),
                "Ava's room".to_owned(),
            ],
            ..Self::plain()
        }
    }

    fn custom(&self, raw: &str) -> Result<(String, String), NameRefusal> {
        let render = RenderFacts {
            context: &self.context,
            conditions: &self.conditions,
            fallback_name: "Ava's room",
        };
        let checks = NameChecks {
            policy: &self.policy,
            filter: &self.filter,
            unique_names: self.unique,
            other_voice_names: &self.others,
        };
        decide_custom_name(raw, &render, &checks).map(|name| (name.stored, name.channel_name))
    }

    fn template(&self, template: &str) -> Result<String, NameRefusal> {
        let render = RenderFacts {
            context: &self.context,
            conditions: &self.conditions,
            fallback_name: "Ava's room",
        };
        let checks = NameChecks {
            policy: &self.policy,
            filter: &self.filter,
            unique_names: self.unique,
            other_voice_names: &self.others,
        };
        decide_template_name(template, &render, &checks)
    }
}

fn words(list: &[&str]) -> AutomodPolicy {
    AutomodPolicy {
        bad_words: list.iter().map(ToString::to_string).collect(),
        ..AutomodPolicy::default()
    }
}

/// The fuzz oracle: every decided name fits the channel bound, is never
/// empty, and carries no C0 control, DEL, `@` or backtick out of the
/// sanitizer. Mirrors `assert_decided_name` in the fuzz target.
fn assert_decided_name(name: &str) {
    assert!(!name.is_empty(), "decided names are never empty");
    assert!(
        name.chars().count() <= MAX_CHANNEL_NAME_CHARS,
        "decided names fit the channel bound"
    );
    assert!(
        !name
            .chars()
            .any(|c| matches!(c, '\u{0}'..='\u{1F}' | '\u{7F}')),
        "no C0 control or DEL leaks into decided names"
    );
    assert!(!name.contains('@'), "the sanitizer strips @");
    assert!(!name.contains('`'), "the sanitizer strips backticks");
}

fn assert_refusal_text(refusal: &NameRefusal) {
    let text = refusal.to_string();
    assert!(!text.is_empty(), "refusals explain what to do");
    assert!(text.chars().count() < 300, "{text}");
}

/// The oracle's refusal side: precedence (blank refuses as empty, overlong
/// refuses as too-long before filtering) and the Taken/TemplateBlocked mapping.
fn assert_custom_oracle(raw: &str, result: &Result<(String, String), NameRefusal>) {
    match result {
        Ok((stored, channel_name)) => {
            assert_eq!(stored, raw.trim());
            assert!(stored.chars().count() <= MAX_CUSTOM_NAME_CHARS);
            assert_decided_name(channel_name);
        }
        Err(refusal) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                assert_eq!(*refusal, NameRefusal::Empty, "{raw:?}");
            }
            if trimmed.chars().count() > MAX_CUSTOM_NAME_CHARS {
                assert_eq!(*refusal, NameRefusal::TooLong, "{raw:?}");
            }
            if *refusal == NameRefusal::Taken {
                assert!(is_literal_name(trimmed), "{raw:?}");
            }
            assert_refusal_text(refusal);
        }
    }
}

fn assert_template_oracle(raw: &str, result: &Result<String, NameRefusal>) {
    match result {
        Ok(name) => assert_decided_name(name),
        Err(refusal) => {
            // Restores never compare for uniqueness; every filter refusal maps
            // to `TemplateBlocked` so the reply never leaks the filter kind.
            assert_eq!(refusal, &NameRefusal::TemplateBlocked, "{raw:?}");
            assert_refusal_text(refusal);
        }
    }
}

/// Inline copies of `fuzz/corpus/voice_room_name/*`: the literal, token,
/// conditional-blocked, format-only, overlong and unicode-fold seeds.
fn seed_inputs(overlong: &str) -> Vec<&str> {
    vec![
        "Late night",
        "@@owner@@'s ## (@@num@@/@@limit@@)",
        "{{LIVE ??blorp//fine}}",
        "@@",
        overlong,
        "👾 Ｌｏｕｎｇｅ @@owner@@",
    ]
}

#[test]
fn seed_corpus_inputs_are_regression_cases_under_plain_and_hostile_facts() {
    let overlong = "a".repeat(MAX_CUSTOM_NAME_CHARS + 1);
    let seeds = seed_inputs(overlong.as_str());
    // Hostile render facts: the fuzzer feeds member-controlled owner/game text
    // through tokens. The builder suite pins fixed facts only; these hostile
    // facts are the fuzz discovery surface.
    let hostile_pairs = [
        ("blorp den", "General"),
        ("discord.gg/abc", "General"),
        ("Ava", "blorp arena"),
        ("A\u{0}va@`", "Ga\u{7F}me@`"),
        ("Ｌｏｕｎｇｅ", "General"),
    ];
    let plain = Fixture::plain();
    let strict = Fixture::strict();
    for raw in &seeds {
        for fixture in [&plain, &strict] {
            assert_custom_oracle(raw, &fixture.custom(raw));
            assert_template_oracle(raw, &fixture.template(raw));
        }
        for (owner, game) in &hostile_pairs {
            let hostile = Fixture::with_context(hostile_context(owner, game));
            assert_custom_oracle(raw, &hostile.custom(raw));
            assert_template_oracle(raw, &hostile.template(raw));
        }
    }
    // Pinned outcomes for the seeds under plain facts: literal passes,
    // format-only is empty, overlong is too long, the unicode fold keeps its
    // emoji while the fullwidth fold strips nothing the oracle forbids.
    assert!(plain.custom("Late night").is_ok());
    assert_eq!(plain.custom("@@"), Err(NameRefusal::Empty));
    assert_eq!(plain.custom(overlong.as_str()), Err(NameRefusal::TooLong));
    let (_, folded) = plain.custom("👾 Ｌｏｕｎｇｅ").unwrap();
    assert_decided_name(&folded);
    assert!(folded.contains("👾"));
}

#[test]
fn member_controlled_token_expansion_is_filtered_after_render() {
    // A blocked word inside the owner's name is pulled into the render by
    // `@@owner@@` and refused after expansion, even though the typed text is
    // clean template syntax.
    let mut bad_owner = Fixture::with_context(hostile_context("blorp den", "General"));
    bad_owner.policy = words(&["blorp"]);
    assert!(matches!(
        bad_owner.custom("@@owner@@"),
        Err(NameRefusal::Blocked { .. })
    ));
    assert_eq!(
        bad_owner.template("@@owner@@'s hub"),
        Err(NameRefusal::TemplateBlocked)
    );
    // Same for the game title through `@@game_name@@`.
    let mut bad_game = Fixture::with_context(hostile_context("Ava", "blorp arena"));
    bad_game.policy = words(&["blorp"]);
    assert!(matches!(
        bad_game.custom("@@game_name@@ arena"),
        Err(NameRefusal::Blocked { .. })
    ));
    // An invite link inside the owner's name is refused after expansion under
    // the default policy.
    let linked = Fixture::with_context(hostile_context("discord.gg/abc", "General"));
    assert!(matches!(
        linked.custom("@@owner@@"),
        Err(NameRefusal::Blocked {
            filter: AutomodFilter::InviteLink
        })
    ));
    assert_eq!(
        linked.template("@@owner@@'s hub"),
        Err(NameRefusal::TemplateBlocked)
    );
    // Controls, `@` and backticks inside the owner's name never leak into a
    // decided name: they are sanitized, and the oracle holds either way.
    let dirty = Fixture::with_context(hostile_context("A\u{0}va@`", "General"));
    match dirty.custom("@@owner@@") {
        Ok((_, channel_name)) => assert_decided_name(&channel_name),
        Err(refusal) => assert_refusal_text(&refusal),
    }
    match dirty.template("@@owner@@'s hub") {
        Ok(name) => assert_decided_name(&name),
        Err(refusal) => assert_eq!(refusal, NameRefusal::TemplateBlocked),
    }
    // An overlong owner name cannot blow the decided bound: the stored text is
    // the short typed template, and the render truncates to the channel bound.
    let huge = Fixture::with_context(hostile_context(
        &"o".repeat(MAX_CUSTOM_NAME_CHARS + 50),
        "General",
    ));
    match huge.custom("@@owner@@") {
        Ok((stored, channel_name)) => {
            assert_eq!(stored, "@@owner@@");
            assert_decided_name(&channel_name);
        }
        Err(refusal) => assert_refusal_text(&refusal),
    }
}

#[test]
fn is_literal_name_pins_the_full_documented_syntax_matrix() {
    // Literals: plain text, even with characters that look templated but parse
    // as text.
    for literal in ["Plain name", "Room #5", "100%", "(a/b)", "under_score"] {
        assert!(is_literal_name(literal), "{literal:?}");
    }
    // Every documented non-literal syntax family: tokens, numbering (`##`,
    // `$#`, `+#`), plurals (`<<a/b>>`), choices (`[[a/b]]`, named lists),
    // resting blocks (`__a/b__`), conditionals (`{{..}}`) and styling
    // (`""mode:text""`).
    for templated in [
        "@@owner@@",
        "##",
        "$#",
        "+#",
        "<<member/members>>",
        "[[a/b]]",
        "[[list:name]]",
        "__resting/in use__",
        "{{LIVE ??a//b}}",
        r#"""upper:@@owner@@"""#,
        "Room ## (@@num@@/@@limit@@)",
    ] {
        assert!(!is_literal_name(templated), "{templated:?}");
    }
}

#[test]
fn refusal_precedence_and_char_bound_hold_on_hostile_shapes() {
    let fixture = Fixture::plain();
    // Trim first: padded names store trimmed, and blank shapes refuse as
    // empty even when the raw text is long.
    assert_eq!(
        fixture.custom("  Late night  ").unwrap().0,
        "Late night".to_owned()
    );
    assert_eq!(fixture.custom("   "), Err(NameRefusal::Empty));
    assert_eq!(
        fixture.custom(&format!("   {}   ", " ".repeat(MAX_CUSTOM_NAME_CHARS + 10))),
        Err(NameRefusal::Empty)
    );
    // The bound counts characters, not bytes: multibyte text at exactly the
    // bound passes, one more character refuses.
    assert!(fixture.custom(&"é".repeat(MAX_CUSTOM_NAME_CHARS)).is_ok());
    assert_eq!(
        fixture.custom(&"é".repeat(MAX_CUSTOM_NAME_CHARS + 1)),
        Err(NameRefusal::TooLong)
    );
    assert!(fixture.custom(&"👾".repeat(MAX_CUSTOM_NAME_CHARS)).is_ok());
    assert_eq!(
        fixture.custom(&"👾".repeat(MAX_CUSTOM_NAME_CHARS + 1)),
        Err(NameRefusal::TooLong)
    );
    // Padding does not inflate the bound: the trimmed text is what counts.
    assert!(fixture
        .custom(&format!("  {}  ", "a".repeat(MAX_CUSTOM_NAME_CHARS)))
        .is_ok());
    assert_eq!(
        fixture.custom(&format!("  {}  ", "a".repeat(MAX_CUSTOM_NAME_CHARS + 1))),
        Err(NameRefusal::TooLong)
    );
    // Every refusal variant explains itself briefly without echoing the name.
    for refusal in [
        NameRefusal::Empty,
        NameRefusal::TooLong,
        NameRefusal::Blocked {
            filter: AutomodFilter::BadWords,
        },
        NameRefusal::Blocked {
            filter: AutomodFilter::InviteLink,
        },
        NameRefusal::Taken,
        NameRefusal::TemplateBlocked,
    ] {
        assert_refusal_text(&refusal);
    }
}

#[test]
fn restore_path_holds_under_hostile_facts() {
    let hostile = Fixture::with_context(hostile_context("blorp den", "General"));
    // Blank templates restore the fallback name.
    assert_eq!(hostile.template(""), Ok("Ava's room".to_owned()));
    assert_eq!(hostile.template("   "), Ok("Ava's room".to_owned()));
    // A restore never compares for uniqueness, even on a clash with the
    // setting on.
    let mut unique = Fixture::with_context(hostile_context("Ava", "General"));
    unique.unique = true;
    unique.others = vec!["Ava's hub".to_owned()];
    assert_eq!(
        unique.template("@@owner@@'s hub"),
        Ok("Ava's hub".to_owned())
    );
    // A template blocked by the filter maps to `TemplateBlocked`, never the
    // filter kind, so the reply cannot fingerprint the policy.
    let mut blocked = Fixture::with_context(hostile_context("Ava", "General"));
    blocked.policy = words(&["blorp"]);
    assert_eq!(
        blocked.template("blorp hub"),
        Err(NameRefusal::TemplateBlocked)
    );
    assert_eq!(
        blocked.template("@@owner@@'s hub"),
        Ok("Ava's hub".to_owned())
    );
    let mut blocked_render = Fixture::with_context(hostile_context("blorp den", "General"));
    blocked_render.policy = words(&["blorp"]);
    assert_eq!(
        blocked_render.template("@@owner@@'s hub"),
        Err(NameRefusal::TemplateBlocked)
    );
}

#[test]
fn hostile_unicode_keeps_the_oracle_without_panicking() {
    // The fuzzer discards invalid UTF-8 at the `&str` boundary; the nearest
    // `cargo test` pin is the hostile-but-valid fringe: replacement chars,
    // zero-width and bidi controls, NFKC folds, emoji and C0/DEL. None may
    // panic, and every outcome keeps the oracle.
    let hostile_texts = [
        "�",
        "a�b",
        "zero\u{200B}width",
        "\u{202E}reversed",
        "＠ｅｖｅｒｙｏｎｅ",
        "｀code｀",
        "é👾Ｌｏｕｎｇｅ",
        "\u{0}\u{1}\u{7F}@`",
        "@@@```",
        "{{LIVE ??\u{0}//fine}}",
        "[[�/b]]",
        "__�/in use__",
        "<<�/�>>",
        "@@owner@@\u{200B}",
    ];
    let plain = Fixture::plain();
    let strict = Fixture::strict();
    let hostile = Fixture::with_context(hostile_context("�\u{200B}＠", "�game\u{0}"));
    for raw in hostile_texts {
        for fixture in [&plain, &strict, &hostile] {
            assert_custom_oracle(raw, &fixture.custom(raw));
            assert_template_oracle(raw, &fixture.template(raw));
        }
    }
}
