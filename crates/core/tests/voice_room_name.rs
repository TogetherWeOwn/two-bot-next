//! Acceptance cases for the pure V3 `/name` decisions.

use two_bot_core::automod::{AutomodFilter, AutomodPolicy};
use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_name_filter::NameFilterContext;
use two_bot_core::voice_naming::RoomContext;
use two_bot_core::voice_room_name::{
    decide_custom_name, decide_template_name, is_literal_name, NameChecks, NameRefusal,
    RenderFacts, MAX_CUSTOM_NAME_CHARS,
};

fn context() -> RoomContext {
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
    fn new() -> Self {
        Self {
            context: context(),
            conditions: ConditionFacts::default(),
            policy: AutomodPolicy::default(),
            filter: filter(),
            others: Vec::new(),
            unique: false,
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

#[test]
fn literal_names_pass_through_trimmed_and_sanitized() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.custom("  Late   night  "),
        Ok(("Late   night".to_owned(), "Late night".to_owned()))
    );
}

#[test]
fn tokens_expand_through_the_template_engine() {
    let fixture = Fixture::new();
    let (stored, name) = fixture
        .custom("@@owner@@'s ## (@@num@@/@@limit@@)")
        .unwrap();
    assert_eq!(stored, "@@owner@@'s ## (@@num@@/@@limit@@)");
    assert_eq!(name, "Ava's #3 (2/8)");
    // V6 conditionals and styling work too, against the supplied facts.
    let mut live = Fixture::new();
    live.conditions.owner_live_discord = true;
    let (_, name) = live
        .custom(r#"{{LIVE ??""upper:@@owner@@"" is live//@@owner@@'s room}}"#)
        .unwrap();
    assert_eq!(name, "AVA is live");
}

#[test]
fn empty_overlong_and_formatting_only_names_are_refused() {
    let fixture = Fixture::new();
    assert_eq!(fixture.custom(""), Err(NameRefusal::Empty));
    assert_eq!(fixture.custom(" \t "), Err(NameRefusal::Empty));
    // Only characters the sanitizer strips.
    assert_eq!(fixture.custom("@@"), Err(NameRefusal::Empty));
    assert_eq!(fixture.custom("`"), Err(NameRefusal::Empty));
    assert!(fixture.custom(&"a".repeat(MAX_CUSTOM_NAME_CHARS)).is_ok());
    assert_eq!(
        fixture.custom(&"a".repeat(MAX_CUSTOM_NAME_CHARS + 1)),
        Err(NameRefusal::TooLong)
    );
    // The bound counts characters, not bytes.
    assert!(fixture.custom(&"é".repeat(MAX_CUSTOM_NAME_CHARS)).is_ok());
}

#[test]
fn an_empty_render_falls_back_to_the_supplied_name() {
    let fixture = Fixture::new();
    // An unknown token renders to nothing; the engine falls back.
    let (stored, name) = fixture.custom("@@nothing_here@@").unwrap();
    assert_eq!(stored, "@@nothing_here@@");
    assert_eq!(name, "Ava's room");
}

#[test]
fn the_automod_name_filter_runs_on_the_text_and_on_the_render() {
    let mut fixture = Fixture::new();
    fixture.policy = words(&["blorp"]);
    for text in ["blorp den", "BLORP", "ｂｌｏｒｐ", "den blorp den"] {
        assert_eq!(
            fixture.custom(text),
            Err(NameRefusal::Blocked {
                filter: AutomodFilter::BadWords
            }),
            "{text}"
        );
    }
    assert!(fixture.custom("blurp den").is_ok());
    // Default policy still blocks links.
    let fixture = Fixture::new();
    assert!(matches!(
        fixture.custom("discord.gg/abc"),
        Err(NameRefusal::Blocked {
            filter: AutomodFilter::InviteLink
        })
    ));
    // A blocked word in an inactive conditional branch is refused now.
    let mut fixture = Fixture::new();
    fixture.policy = words(&["blorp"]);
    assert!(matches!(
        fixture.custom("{{LIVE ??blorp//fine}}"),
        Err(NameRefusal::Blocked { .. })
    ));
    // A member-controlled value pulled in by a token is filtered after it
    // expands.
    let mut fixture = Fixture::new();
    fixture.policy = words(&["ava"]);
    assert!(matches!(
        fixture.custom("@@owner@@'s den"),
        Err(NameRefusal::Blocked { .. })
    ));
}

#[test]
fn unique_names_reject_literal_clashes_only_when_enabled() {
    let mut fixture = Fixture::new();
    fixture.others = vec!["Lounge".to_owned(), "Voice Room".to_owned()];
    // Off: a clash is fine.
    assert!(fixture.custom("Lounge").is_ok());
    fixture.unique = true;
    for text in [
        "Lounge",
        "lounge",
        "LOUNGE",
        "  lounge  ",
        "Ｌｏｕｎｇｅ",
        "ｌｏｕｎｇｅ",
    ] {
        assert_eq!(fixture.custom(text), Err(NameRefusal::Taken), "{text:?}");
    }
    // Different names pass, as do partial matches.
    assert!(fixture.custom("Loung").is_ok());
    assert!(fixture.custom("Lounge 2").is_ok());
    // No other channel, no clash.
    fixture.others.clear();
    assert!(fixture.custom("Lounge").is_ok());
}

#[test]
fn unique_names_skip_names_built_from_tokens() {
    let mut fixture = Fixture::new();
    fixture.unique = true;
    fixture.others = vec!["Ava".to_owned(), "Ava's room".to_owned()];
    // `@@owner@@` renders to "Ava", which another channel uses, but the name
    // follows the room, so a one-time comparison would prove nothing.
    assert!(fixture.custom("@@owner@@").is_ok());
    assert!(fixture.custom("##").is_ok());
    assert!(!is_literal_name("@@owner@@"));
    assert!(!is_literal_name("[[a/b]]"));
    assert!(!is_literal_name("{{LIVE ??a//b}}"));
    assert!(is_literal_name("Plain name"));
    assert!(is_literal_name("Room #5"));
}

#[test]
fn restore_renders_the_creator_template_or_the_fallback() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.template("@@owner@@'s hub"),
        Ok("Ava's hub".to_owned())
    );
    // A blank template (the creator default) restores the fallback name.
    assert_eq!(fixture.template(""), Ok("Ava's room".to_owned()));
    assert_eq!(fixture.template("   "), Ok("Ava's room".to_owned()));
    // A restore is never compared for uniqueness, even with the setting on.
    let mut unique = Fixture::new();
    unique.unique = true;
    unique.others = vec!["Ava's hub".to_owned()];
    assert_eq!(
        unique.template("@@owner@@'s hub"),
        Ok("Ava's hub".to_owned())
    );
}

#[test]
fn restore_refuses_a_template_the_filter_blocks() {
    let mut fixture = Fixture::new();
    fixture.policy = words(&["blorp"]);
    assert_eq!(
        fixture.template("blorp hub"),
        Err(NameRefusal::TemplateBlocked)
    );
}

#[test]
fn refusal_text_is_actionable_and_never_echoes_the_name() {
    for refusal in [
        NameRefusal::Empty,
        NameRefusal::TooLong,
        NameRefusal::Blocked {
            filter: AutomodFilter::BadWords,
        },
        NameRefusal::Taken,
        NameRefusal::TemplateBlocked,
    ] {
        let text = refusal.to_string();
        assert!(!text.is_empty());
        assert!(text.chars().count() < 300, "{text}");
    }
    assert!(NameRefusal::Blocked {
        filter: AutomodFilter::BadWords
    }
    .to_string()
    .contains("bad words"));
}
