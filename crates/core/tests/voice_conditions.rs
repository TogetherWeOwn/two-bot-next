//! Hermetic V6b acceptance cases against the public condition core API,
//! including every exact conditional case in the shared template corpus.

use std::collections::HashMap;

use proptest::prelude::*;
use serde_json::Value;
use two_bot_core::voice_conditions::{
    parse_condition, Comparison, Condition, ConditionFacts, Conditions, Counter, GameOp, Keyword,
    Operand, PersonScope, MAX_CONDITION_BYTES, MAX_CONDITION_NESTING,
};
use two_bot_core::voice_naming::{
    parse, render, render_str, resolve_majority_game, ChannelKind, Evaluation, ExtensionPolicy,
    GameOptions, PartyInfo, PassthroughExtensions, RoomContext, Template,
};

const CORPUS: &str = include_str!("../../../tests/voice_templates/corpus.json");

/// A parent-style V6 policy: this slice's conditions plus a stand-in for the
/// V6a styling pass that upper-cases `upper` bodies.
struct V6<'f> {
    conditions: Conditions<'f>,
}

impl ExtensionPolicy for V6<'_> {
    fn conditional(&self, source: &str, evaluation: &mut Evaluation<'_, Self>) -> String {
        self.conditions.evaluate(source, evaluation)
    }

    fn styled(
        &self,
        modes: &str,
        body: &Template,
        _source: &str,
        evaluation: &mut Evaluation<'_, Self>,
    ) -> String {
        let text = evaluation.evaluate(body);
        if modes == "upper" {
            text.to_uppercase()
        } else {
            text
        }
    }
}

/// Unix timestamp at `hour` UTC on the first day matching the labels.
fn clock(weekday: &str, month: &str, hour: i64) -> i64 {
    let wanted = format!("{weekday}|{month}");
    (0..400)
        .map(|day| day * 86_400 + hour * 3_600)
        .find(|&timestamp| {
            let room = RoomContext {
                timestamp,
                ..RoomContext::default()
            };
            render_str("@@weekday@@|@@month@@", &room) == wanted
        })
        .expect("every weekday occurs in every month of a year")
}

/// One owner alone in temporary room 3, Wednesday noon in September.
fn room() -> RoomContext {
    RoomContext {
        room_number: 3,
        owner_name: "Alex".to_string(),
        member_count: 1,
        owner_present: true,
        timestamp: clock("Wednesday", "September", 12),
        ..RoomContext::default()
    }
}

fn facts() -> ConditionFacts {
    ConditionFacts {
        owner_id: Some("100".to_string()),
        member_ids: vec!["100".to_string()],
        ..ConditionFacts::default()
    }
}

/// Unfinalized output: no trim, truncation or fallback.
fn eval(template: &str, room: &RoomContext, facts: &ConditionFacts) -> String {
    let policy = V6 {
        conditions: Conditions::new(facts),
    };
    Evaluation::new(room, &policy).evaluate(&parse(template))
}

fn holds(condition: &str, room: &RoomContext, facts: &ConditionFacts) -> bool {
    parse_condition(condition).holds(facts, room)
}

#[test]
fn grammar_has_optional_else_and_keeps_branch_text_as_written() {
    let open = room();
    let full = RoomContext {
        user_limit: 1,
        ..room()
    };
    let facts = facts();
    assert_eq!(eval("x{{FULL ??yes}}", &full, &facts), "xyes");
    assert_eq!(eval("x{{FULL ??yes}}", &open, &facts), "x");
    assert_eq!(eval("{{FULL ??yes//no}}", &full, &facts), "yes");
    assert_eq!(eval("{{FULL ??yes//no}}", &open, &facts), "no");
    assert_eq!(eval("[{{ FULL ?? yes // no }}]", &full, &facts), "[ yes ]");
    assert_eq!(eval("[{{ FULL ?? yes // no }}]", &open, &facts), "[ no ]");
    // Only the first `??` and the first `//` after it split the node.
    assert_eq!(eval("{{FULL ??a??b//c//d}}", &full, &facts), "a??b");
    assert_eq!(eval("{{FULL ??a??b//c//d}}", &open, &facts), "c//d");
    // Separators inside nested syntax never split the node.
    assert_eq!(eval("{{FULL ??[[a//b]]//<<x/y>>}}", &open, &facts), "x");
    // A node without `??` is not a conditional: it stays literal.
    assert_eq!(eval("{{FULL}}", &full, &facts), "{{FULL}}");
    // A false condition without an else branch renders nothing; the name
    // tail then applies the fallback.
    let fallback = V6 {
        conditions: Conditions::new(&facts),
    };
    assert_eq!(
        render(&parse("{{FULL ??yes}}"), &open, &fallback),
        "Voice Room"
    );
}

#[test]
fn comparisons_cover_every_operator_on_numbers_and_counter_tokens() {
    let facts = facts();
    let solo = room();
    let cases: [(&str, bool); 12] = [
        ("@@num@@ < 1", false),
        ("@@num@@ > 1", false),
        ("@@num@@ <= 1", true),
        ("@@num@@ >= 1", true),
        ("@@num@@ = 1", true),
        ("@@num@@ != 1", false),
        ("7 > 2", true),
        ("2>7", false),
        ("@@hour@@ >= 12", true),
        ("@@hour@@<12", false),
        ("$# = 3", true),
        ("$00# = 3", true),
    ];
    for (condition, expected) in cases {
        assert_eq!(holds(condition, &solo, &facts), expected, "{condition}");
    }

    let crowded = RoomContext {
        member_count: 3,
        user_limit: 4,
        live_count: 2,
        members_playing: 2,
        ..room()
    };
    let cases: [(&str, bool); 8] = [
        ("@@num@@ < @@limit@@", true),
        ("@@slots@@ = 1", true),
        ("@@slots@@ != @@limit@@", true),
        ("@@num_others@@ = 2", true),
        ("@@num_live@@ = 2", true),
        ("@@num_playing@@ = 2", true),
        ("@@party_size@@ = @@limit@@", true),
        ("@@NUM@@ = 3", true),
    ];
    for (condition, expected) in cases {
        assert_eq!(holds(condition, &crowded, &facts), expected, "{condition}");
    }
}

#[test]
fn non_numeric_operands_make_comparisons_false() {
    let facts = facts();
    let solo = room();
    for condition in [
        // `##` and `+#` are not numeric; comparisons use `$#`.
        "## = 3",
        "## != 3",
        "+# = 3",
        "+# != 3",
        // Name tokens are never expanded inside a condition.
        "@@owner@@ = Alex",
        "@@owner@@ != Alex",
        "@@game_name@@ = General",
        // Only one comparison per condition.
        "1 < 2 < 3",
        "1 <",
        "= 1",
        "x = x",
    ] {
        assert_eq!(
            parse_condition(condition),
            Condition::Unknown,
            "{condition}"
        );
        assert!(!holds(condition, &solo, &facts), "{condition}");
    }
    assert_eq!(eval("{{## = 3 ??yes//no}}", &solo, &facts), "no");
    assert_eq!(eval("{{+# = 3 ??yes//no}}", &solo, &facts), "no");

    // A blank `@@slots@@` (no limit) parses but has no value, so every
    // operator is false.
    for condition in ["@@slots@@ = 0", "@@slots@@ != 0", "@@slots@@ < @@num@@"] {
        assert_ne!(
            parse_condition(condition),
            Condition::Unknown,
            "{condition}"
        );
        assert!(!holds(condition, &solo, &facts), "{condition}");
    }
}

#[test]
fn parser_builds_typed_conditions() {
    assert_eq!(parse_condition(" full "), Condition::Keyword(Keyword::Full));
    assert_eq!(
        parse_condition("ROLE: 42 "),
        Condition::Person {
            scope: PersonScope::Role,
            id: "42".to_string(),
        }
    );
    assert_eq!(
        parse_condition("GAME!=Half-Life: Alyx"),
        Condition::Game {
            op: GameOp::NotEquals,
            title: "Half-Life: Alyx".to_string(),
        }
    );
    assert_eq!(
        parse_condition("@@slots@@ <= $0#"),
        Condition::Compare {
            left: Operand::Counter(Counter::Slots),
            op: Comparison::LessOrEqual,
            right: Operand::RoomNumber,
        }
    );
    assert_eq!(
        parse_condition("MONTH:december"),
        Condition::Compare {
            left: Operand::Month,
            op: Comparison::Equal,
            right: Operand::Number(12),
        }
    );
}

#[test]
fn activity_and_streaming_keywords() {
    let solo = room();
    let idle = facts();
    let busy = ConditionFacts {
        owner_playing: true,
        owner_live_external: true,
        live_external_count: 1,
        ..facts()
    };
    let other_live = ConditionFacts {
        live_discord_count: 1,
        ..facts()
    };
    let cases: [(&str, bool, bool, bool); 5] = [
        // condition, idle owner, playing + external owner, someone else live
        ("PLAYING", false, true, false),
        ("LIVE", false, true, false),
        ("LIVE_DISCORD", false, false, false),
        ("LIVE_EXTERNAL", false, true, false),
        ("ANY_LIVE", false, true, true),
    ];
    for (condition, when_idle, when_busy, when_other_live) in cases {
        assert_eq!(holds(condition, &solo, &idle), when_idle, "{condition}");
        assert_eq!(holds(condition, &solo, &busy), when_busy, "{condition}");
        assert_eq!(
            holds(condition, &solo, &other_live),
            when_other_live,
            "{condition}"
        );
    }
    let discord = ConditionFacts {
        owner_live_discord: true,
        live_discord_count: 1,
        ..facts()
    };
    assert!(holds("LIVE", &solo, &discord));
    assert!(holds("LIVE_DISCORD", &solo, &discord));
    assert!(!holds("LIVE_EXTERNAL", &solo, &discord));
}

#[test]
fn roles_and_people_keywords() {
    let solo = room();
    let facts = ConditionFacts {
        owner_id: Some("100".to_string()),
        owner_role_ids: vec!["7".to_string()],
        member_ids: vec!["100".to_string(), "200".to_string()],
        member_role_ids: vec!["7".to_string(), "8".to_string()],
        ..ConditionFacts::default()
    };
    let cases: [(&str, bool); 11] = [
        ("ROLE:7", true),
        ("ROLE:8", false),
        ("ANY_ROLE:7", true),
        ("ANY_ROLE:8", true),
        ("ANY_ROLE:9", false),
        ("MEMBER:200", true),
        ("MEMBER:300", false),
        ("OWNER:100", true),
        ("OWNER:200", false),
        ("OWNER", true),
        ("ROLE:", false),
    ];
    for (condition, expected) in cases {
        assert_eq!(holds(condition, &solo, &facts), expected, "{condition}");
    }
    let owner_away = RoomContext {
        owner_present: false,
        ..room()
    };
    assert!(!holds("OWNER", &owner_away, &facts));
}

#[test]
fn game_and_party_keywords() {
    let solo = room();
    let tie = ConditionFacts {
        games: vec!["Apex Legends".to_string(), "Chess".to_string()],
        ..facts()
    };
    let cases: [(&str, bool); 9] = [
        ("GAME", true),
        ("GAME:legend", true),
        ("GAME:Chess", true),
        ("GAME:Poker", false),
        ("GAME=apex legends", true),
        ("GAME=Apex", false),
        ("GAME!=Chess", false),
        ("GAME!=Poker", true),
        ("GAME<Chess", false),
    ];
    for (condition, expected) in cases {
        assert_eq!(holds(condition, &solo, &tie), expected, "{condition}");
    }
    assert!(!holds("GAME", &solo, &facts()));
    assert!(holds("GAME!=Chess", &solo, &facts()));

    let party = |size, max| PartyInfo {
        size,
        max,
        ..PartyInfo::default()
    };
    let nobody = room();
    let playing_alone = RoomContext {
        members_playing: 1,
        ..room()
    };
    let open_party = RoomContext {
        parties: vec![party(4, Some(12))],
        ..room()
    };
    let full_party = RoomContext {
        parties: vec![party(2, Some(5)), party(4, Some(4))],
        ..room()
    };
    let unbounded = RoomContext {
        parties: vec![party(4, None)],
        ..room()
    };
    let three_way = RoomContext {
        parties: vec![party(2, Some(2)), party(2, Some(2)), party(2, Some(2))],
        ..room()
    };
    let cases: [(&RoomContext, bool, bool, bool); 6] = [
        // room, PLAYERS, MAX, RICH
        (&nobody, false, false, false),
        (&playing_alone, true, false, false),
        (&open_party, true, false, true),
        (&full_party, true, true, true),
        (&unbounded, true, false, true),
        (&three_way, true, false, true),
    ];
    for (room, players, max, rich) in cases {
        assert_eq!(holds("PLAYERS", room, &facts()), players, "{room:?}");
        assert_eq!(holds("MAX", room, &facts()), max, "{room:?}");
        assert_eq!(holds("RICH", room, &facts()), rich, "{room:?}");
    }
}

#[test]
fn room_state_keywords() {
    let private = ConditionFacts {
        private: true,
        ..facts()
    };
    let with = |member_count, user_limit| RoomContext {
        member_count,
        user_limit,
        ..room()
    };
    // FULL requires a limit.
    assert!(!holds("FULL", &with(3, 0), &facts()));
    assert!(!holds("FULL", &with(3, 4), &facts()));
    assert!(holds("FULL", &with(3, 3), &facts()));
    assert!(holds("FULL", &with(4, 3), &facts()));

    let temporary = room();
    let standalone = RoomContext {
        channel_kind: ChannelKind::Standalone,
        ..room()
    };
    assert!(holds("PRIVATE", &temporary, &private));
    assert!(!holds("PRIVATE", &temporary, &facts()));
    // PRIVATE is always false on standalone channels.
    assert!(!holds("PRIVATE", &standalone, &private));
    assert!(!holds("PRIVATE", &standalone, &facts()));
}

#[test]
fn date_keywords_follow_the_room_clock() {
    let days = [
        ("Monday", false),
        ("Tuesday", false),
        ("Wednesday", false),
        ("Thursday", false),
        ("Friday", false),
        ("Saturday", true),
        ("Sunday", true),
    ];
    for (index, (day, weekend)) in days.into_iter().enumerate() {
        let at = RoomContext {
            timestamp: clock(day, "March", 9),
            ..room()
        };
        assert_eq!(holds("WEEKEND", &at, &facts()), weekend, "{day}");
        assert_eq!(holds("WEEKDAY", &at, &facts()), !weekend, "{day}");
        assert!(holds(&format!("WEEKDAY:{day}"), &at, &facts()), "{day}");
        assert!(holds(&format!("WEEKDAY = {}", index + 1), &at, &facts()));
        assert!(!holds(&format!("WEEKDAY != {day}"), &at, &facts()));
    }

    // The guild offset moves the local day: Sunday 23:00 UTC is Monday at +2h.
    let shifted = RoomContext {
        timestamp: clock("Sunday", "March", 23),
        tz_offset_minutes: 120,
        ..room()
    };
    assert!(!holds("WEEKEND", &shifted, &facts()));
    assert!(holds("WEEKDAY:monday", &shifted, &facts()));

    let september = room();
    let cases: [(&str, bool); 8] = [
        ("MONTH", false),
        ("MONTH:September", true),
        ("MONTH = september", true),
        ("MONTH = 9", true),
        ("MONTH >= June", true),
        ("MONTH < 9", false),
        ("June <= MONTH", true),
        ("MONTH = Sept", false),
    ];
    for (condition, expected) in cases {
        assert_eq!(
            holds(condition, &september, &facts()),
            expected,
            "{condition}"
        );
    }
}

#[test]
fn unknown_conditions_are_false_and_never_panic() {
    let solo = room();
    let everything = ConditionFacts {
        owner_playing: true,
        owner_live_discord: true,
        private: true,
        ..facts()
    };
    for condition in [
        "",
        "   ",
        "NOT_DEFINED",
        "!PRIVATE",
        "PRIVATE FULL",
        "OWNER=100",
        "ROLE=7",
        "FULL:1",
        "@@unknown@@ = 1",
        "\u{1F3AE} > 1",
        "99999999999999999999 > 1",
    ] {
        assert_eq!(
            parse_condition(condition),
            Condition::Unknown,
            "{condition}"
        );
        assert!(!holds(condition, &solo, &everything), "{condition}");
    }
    assert_eq!(eval("{{NOT_DEFINED ??yes//no}}", &solo, &everything), "no");

    // Over-long conditions are unknown even when they would otherwise match.
    let id = "9".repeat(MAX_CONDITION_BYTES);
    let member = ConditionFacts {
        member_ids: vec![id.clone()],
        ..facts()
    };
    assert!(!holds(&format!("MEMBER:{id}"), &solo, &member));
    let short = &id[..MAX_CONDITION_BYTES - "MEMBER:".len()];
    let member = ConditionFacts {
        member_ids: vec![short.to_string()],
        ..facts()
    };
    assert!(holds(&format!("MEMBER:{short}"), &solo, &member));
}

#[test]
fn nested_role_live_default_fallback_chain() {
    let template = "{{ROLE:raid ??Raid @@owner@@//{{LIVE ??Live: @@stream_name@@//{{PRIVATE ??Locked//@@owner@@'s room}}}}}}";
    let stream = RoomContext {
        stream_title: "Speedruns".to_string(),
        ..room()
    };
    let raider = ConditionFacts {
        owner_role_ids: vec!["raid".to_string()],
        owner_live_discord: true,
        ..facts()
    };
    let live = ConditionFacts {
        owner_live_external: true,
        ..facts()
    };
    let locked = ConditionFacts {
        private: true,
        ..facts()
    };
    assert_eq!(eval(template, &stream, &raider), "Raid Alex");
    assert_eq!(eval(template, &stream, &live), "Live: Speedruns");
    assert_eq!(eval(template, &stream, &locked), "Locked");
    assert_eq!(eval(template, &stream, &facts()), "Alex's room");
}

#[test]
fn nested_conditions_resolve_innermost_first_without_expanding_names() {
    let solo = room();
    let full = RoomContext {
        user_limit: 1,
        ..room()
    };
    let private = ConditionFacts {
        private: true,
        ..facts()
    };
    // The inner node picks which condition the outer node tests.
    let template = "{{{{PRIVATE ??FULL//OWNER}} ??yes//no}}";
    // Inner branch: OWNER, FULL with no limit, FULL at the limit.
    assert_eq!(eval(template, &solo, &facts()), "yes");
    assert_eq!(eval(template, &solo, &private), "no");
    assert_eq!(eval(template, &full, &private), "yes");

    // A false inner node without an else branch leaves only the outer text.
    assert_eq!(
        eval("{{{{PRIVATE ??!}}FULL ??yes//no}}", &full, &facts()),
        "yes"
    );
    // Selected inner text keeps tokens as source, so names never match.
    assert_eq!(
        eval(
            "{{{{FULL ??x//@@owner@@}} = Alex ??yes//no}}",
            &solo,
            &facts()
        ),
        "no"
    );
    assert_eq!(
        eval("{{{{FULL ??x//@@num@@}} = 1 ??yes//no}}", &solo, &facts()),
        "yes"
    );
}

#[test]
fn random_picks_stay_stable_when_conditions_flip() {
    let template =
        "[[a/b/c/d/e/f/g/h]] {{FULL ??[[1/2/3/4/5/6/7/8]]//[[p/q/r/s/t/u/v/w]] [[i/j/k/l/m/n/o/x]]}} [[A/B/C/D/E/F/G/H]]";
    let open = room();
    let full = RoomContext {
        user_limit: 1,
        ..room()
    };
    let facts = facts();
    let words = |text: String| text.split(' ').map(str::to_string).collect::<Vec<_>>();
    for seed in 0..64 {
        let open = RoomContext {
            seed,
            ..open.clone()
        };
        let full = RoomContext {
            seed,
            ..full.clone()
        };
        let yes = words(eval(template, &full, &facts));
        let no = words(eval(template, &open, &facts));
        let literal =
            words(Evaluation::new(&open, &PassthroughExtensions).evaluate(&parse(template)));
        assert_eq!(yes.len(), 3, "{yes:?}");
        assert_eq!(no.len(), 4, "{no:?}");
        // Picks before and after the node never re-roll, and agree with the
        // renderer's own reservation for the literal node.
        assert_eq!(yes[0], no[0]);
        assert_eq!(yes[2], no[3]);
        assert_eq!(literal.first(), Some(&yes[0]));
        assert_eq!(literal.last(), Some(&yes[2]));
    }
}

#[test]
fn composes_with_passthrough_and_styling_policies() {
    let role = ConditionFacts {
        owner_role_ids: vec!["raid".to_string()],
        ..facts()
    };
    let solo = room();
    let template = "{{ROLE:raid ??\"\"upper:@@owner@@\"\"//fallback}}";
    // V5 passthrough leaves the node literal.
    assert_eq!(
        Evaluation::new(&solo, &PassthroughExtensions).evaluate(&parse(template)),
        template
    );
    // Conditions alone evaluate the node and leave styling literal.
    let conditions = Conditions::new(&role);
    assert_eq!(
        Evaluation::new(&solo, &conditions).evaluate(&parse(template)),
        "\"\"upper:@@owner@@\"\""
    );
    // A parent policy delegating to `Conditions::evaluate` styles the branch.
    assert_eq!(eval(template, &solo, &role), "ALEX");
    assert_eq!(eval(template, &solo, &facts()), "fallback");
}

#[test]
fn nesting_beyond_the_bound_renders_literally() {
    let open = room();
    let facts = facts();
    let chain = |depth: usize| format!("{}z{}", "{{FULL ??a//".repeat(depth), "}}".repeat(depth));
    assert_eq!(eval(&chain(MAX_CONDITION_NESTING), &open, &facts), "z");
    let deep = eval(&chain(MAX_CONDITION_NESTING + 1), &open, &facts);
    assert_eq!(deep, "{{FULL ??a//z}}");

    // Conditions nested inside conditions share the same budget.
    let condition = |depth: usize| {
        let mut text = "OWNER".to_string();
        for _ in 0..depth {
            text = format!("{{{{{text} ??OWNER//FULL}}}}");
        }
        format!("{{{{{text} ??yes//no}}}}")
    };
    assert_eq!(
        eval(&condition(MAX_CONDITION_NESTING - 1), &open, &facts),
        "yes"
    );
    assert_eq!(eval(&condition(40), &open, &facts), "no");
}

// ---------------------------------------------------------------------------
// Shared template corpus
// ---------------------------------------------------------------------------

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn number(value: &Value) -> u32 {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

fn flag(value: &Value) -> bool {
    value.as_bool().unwrap_or(false)
}

/// The corpus context as the parent runtime would hand it to the core.
fn corpus_room(context: &Value) -> (RoomContext, ConditionFacts) {
    let members = context["members"].as_array().cloned().unwrap_or_default();
    let owner_id = text(&context["owner_id"]);
    let owner = members
        .iter()
        .find(|member| text(&member["id"]) == owner_id);
    let settings = &context["settings"];
    let aliases: HashMap<String, String> = settings["aliases"]
        .as_object()
        .map(|map| map.iter().map(|(k, v)| (k.clone(), text(v))).collect())
        .unwrap_or_default();
    let canonical = |title: String| aliases.get(&title).cloned().unwrap_or(title);
    let activities: Vec<Option<String>> = members
        .iter()
        .map(|member| member["game"].as_str().map(str::to_string))
        .collect();
    let options = GameOptions {
        aliases: aliases.clone(),
        force_single: flag(&settings["force_single_game"]),
        count_idle_toward_majority: flag(&settings["include_inactive"]),
        no_game_label: text(&settings["no_game"]),
    };
    let owner_game = owner.and_then(|member| member["game"].as_str());
    let mut parties: Vec<(String, PartyInfo)> = Vec::new();
    for party in members.iter().map(|member| &member["party"]) {
        if party.is_object() && !parties.iter().any(|(id, _)| *id == text(&party["id"])) {
            let max = party["maximum"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok());
            let info = PartyInfo {
                size: number(&party["size"]),
                max,
                state: text(&party["state"]),
                details: text(&party["details"]),
            };
            parties.push((text(&party["id"]), info));
        }
    }
    let live = |member: &&Value| flag(&member["live_discord"]) || flag(&member["live_external"]);
    let clock_labels = &context["clock"];
    let room = RoomContext {
        channel_kind: if context["channel_kind"] == "standalone" {
            ChannelKind::Standalone
        } else {
            ChannelKind::Temporary
        },
        room_number: number(&context["number"]),
        owner_name: owner
            .map(|member| {
                member["nick"]
                    .as_str()
                    .map_or_else(|| text(&member["display_name"]), str::to_string)
            })
            .unwrap_or_default(),
        original_creator_name: text(&context["original_creator_name"]),
        member_count: u32::try_from(members.len()).unwrap_or(u32::MAX),
        owner_present: owner.is_some(),
        live_count: u32::try_from(members.iter().filter(live).count()).unwrap_or(0),
        user_limit: number(&context["limit"]),
        game_name: resolve_majority_game(&activities, owner_game, &options),
        stream_title: owner
            .filter(live)
            .map(|member| text(&member["stream_title"]))
            .unwrap_or_default(),
        members_playing: u32::try_from(activities.iter().flatten().count()).unwrap_or(0),
        parties: parties.into_iter().map(|(_, info)| info).collect(),
        timestamp: clock(
            &text(&clock_labels["weekday"]),
            &text(&clock_labels["month"]),
            clock_labels["hour"].as_i64().unwrap_or(0),
        ),
        seed: text(&context["seed"])
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            }),
        named_lists: settings["named_lists"]
            .as_object()
            .map(|lists| {
                lists
                    .iter()
                    .map(|(name, items)| {
                        let items = items
                            .as_array()
                            .map(|items| items.iter().map(text).collect());
                        (name.clone(), items.unwrap_or_default())
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ..RoomContext::default()
    };

    // Shown titles: the majority title, both on a two-way tie, none otherwise.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for title in activities.iter().flatten() {
        *counts.entry(canonical(title.clone())).or_insert(0) += 1;
    }
    let top = counts.values().copied().max().unwrap_or(0);
    let mut games: Vec<String> = counts
        .into_iter()
        .filter(|(_, count)| *count == top)
        .map(|(title, _)| title)
        .collect();
    games.sort();
    if games.len() > 2 {
        games.clear();
    }
    let roles = |member: &Value| -> Vec<String> {
        member["roles"]
            .as_array()
            .map(|roles| roles.iter().map(text).collect())
            .unwrap_or_default()
    };
    let facts = ConditionFacts {
        owner_id: Some(owner_id),
        owner_role_ids: owner.map(roles).unwrap_or_default(),
        member_ids: members.iter().map(|member| text(&member["id"])).collect(),
        member_role_ids: members.iter().flat_map(roles).collect(),
        owner_playing: owner_game.is_some(),
        owner_live_discord: owner.is_some_and(|member| flag(&member["live_discord"])),
        owner_live_external: owner.is_some_and(|member| flag(&member["live_external"])),
        live_discord_count: u32::try_from(
            members
                .iter()
                .filter(|member| flag(&member["live_discord"]))
                .count(),
        )
        .unwrap_or(0),
        live_external_count: u32::try_from(
            members
                .iter()
                .filter(|member| flag(&member["live_external"]))
                .count(),
        )
        .unwrap_or(0),
        games,
        private: flag(&context["private"]),
    };
    (room, facts)
}

fn corpus_render(case: &Value, contexts: &Value) -> String {
    let (room, facts) = corpus_room(&contexts[text(&case["context"])]);
    let policy = V6 {
        conditions: Conditions::new(&facts),
    };
    render(&parse(&text(&case["input"])), &room, &policy)
}

#[test]
fn corpus_exact_conditional_cases_pass() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("corpus is JSON");
    let contexts = &corpus["contexts"];
    let mut checked = 0;
    for case in corpus["cases"].as_array().expect("cases") {
        let input = text(&case["input"]);
        if !input.contains("{{") || case["expected"]["kind"] != "exact" {
            continue;
        }
        let expected = text(&case["expected"]["output"]);
        assert_eq!(
            corpus_render(case, contexts),
            expected,
            "corpus case {}",
            case["id"]
        );
        checked += 1;
    }
    assert_eq!(checked, 48, "every exact conditional case is asserted");
}

/// Deferred probes record this slice's chosen semantics (see
/// docs/voice-conditions-core.md); the corpus leaves them open.
#[test]
fn corpus_deferred_condition_probes_follow_chosen_semantics() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("corpus is JSON");
    let contexts = &corpus["contexts"];
    let chosen: HashMap<&str, &str> = HashMap::from([
        ("condition-unresolved-PLAYING", "no"),
        ("condition-unresolved-LIVE", "yes"),
        ("condition-unresolved-LIVE_DISCORD", "yes"),
        ("condition-unresolved-LIVE_EXTERNAL", "yes"),
        ("condition-unresolved-ANY_LIVE", "yes"),
        ("condition-unresolved-OWNER", "yes"),
        ("condition-unresolved-PLAYERS", "yes"),
        ("condition-unresolved-MAX", "no"),
        ("condition-unresolved-RICH", "yes"),
        ("condition-unresolved-WEEKDAY", "yes"),
        ("condition-unresolved-MONTH", "no"),
        ("numeric-excluded-##", "no"),
        ("numeric-excluded-+#", "no"),
        ("malformed-template-delimiter", "{{PRIVATE ??yes"),
    ]);
    let mut checked = 0;
    for case in corpus["cases"].as_array().expect("cases") {
        let input = text(&case["input"]);
        if !input.contains("{{") || case["expected"]["kind"] == "exact" {
            continue;
        }
        let id = text(&case["id"]);
        let expected = chosen.get(id.as_str()).unwrap_or_else(|| panic!("{id}"));
        assert_eq!(corpus_render(case, contexts), *expected, "{id}");
        checked += 1;
    }
    assert_eq!(checked, chosen.len());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_condition_nodes_never_panic(
        pieces in proptest::collection::vec(
            proptest::sample::select(vec![
                "{{", "}}", "??", "//", "FULL", "OWNER", "ROLE:", "7", " ", "=", "!=", "<",
                "@@num@@", "$#", "##", "[[a/b]]", "\"\"upper:", "\"\"", "<<x/y>>", "GAME:",
            ]),
            0..48,
        )
    ) {
        let template = pieces.concat();
        let facts = facts();
        let out = render(&parse(&template), &room(), &V6 { conditions: Conditions::new(&facts) });
        prop_assert!(!out.is_empty());
        prop_assert!(out.chars().count() <= 100);
        // Same input, same name.
        let again = render(&parse(&template), &room(), &V6 { conditions: Conditions::new(&facts) });
        prop_assert_eq!(out, again);
    }
}
