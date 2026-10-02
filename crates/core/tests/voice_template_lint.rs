use proptest::prelude::*;
use two_bot_core::voice_naming::{
    parse, Evaluation, ExtensionPolicy, PassthroughExtensions, Template, DEFAULT_FALLBACK_NAME,
    MAX_TEMPLATE_BYTES,
};
use two_bot_core::voice_template_lint::{
    lint, preview, Construct, Finding, FindingKind, LintReport, ParseError, Position, Scenario,
    Severity, EXCERPT_CHARS, KNOWN_TOKENS, MAX_FINDINGS, MAX_MESSAGE_CHARS, SCENARIO_SEED,
};

const PASSTHROUGH: PassthroughExtensions = PassthroughExtensions;

/// Minimal stand-in for a V6 conditions policy: `{{COND ?? yes // no}}` with
/// a handful of keywords; every other condition is false.
struct TestConditions;

impl ExtensionPolicy for TestConditions {
    fn conditional(&self, source: &str, evaluation: &mut Evaluation<'_, Self>) -> String {
        let inner = &source[2..source.len() - 2];
        let Some((condition, branches)) = inner.split_once("??") else {
            return String::new();
        };
        let (yes, no) = branches.split_once("//").unwrap_or((branches, ""));
        let room = evaluation.context();
        let truth = match condition.trim() {
            "LIVE" => room.live_count > 0,
            "FULL" => room.user_limit > 0 && room.member_count >= room.user_limit,
            "GAME" => room.members_playing > 0,
            // 1970-01-01 was a Thursday; 0 is Sunday and 6 is Saturday.
            "WEEKEND" => matches!((room.timestamp.div_euclid(86_400) + 4) % 7, 0 | 6),
            _ => false,
        };
        evaluation.evaluate(&parse(if truth { yes } else { no }))
    }

    fn styled(
        &self,
        _modes: &str,
        body: &Template,
        _source: &str,
        evaluation: &mut Evaluation<'_, Self>,
    ) -> String {
        evaluation.evaluate(body).to_uppercase()
    }
}

fn unclosed(construct: Construct) -> FindingKind {
    FindingKind::ParseError(ParseError::Unclosed(construct))
}

fn located(report: &LintReport) -> Vec<(FindingKind, Option<usize>)> {
    report
        .findings
        .iter()
        .map(|finding| {
            (
                finding.kind.clone(),
                finding.position.map(|position| position.byte),
            )
        })
        .collect()
}

fn errors<E: ExtensionPolicy>(source: &str, extensions: &E) -> Vec<(FindingKind, Option<usize>)> {
    lint(source, extensions)
        .findings
        .into_iter()
        .filter(|finding| finding.severity() == Severity::Error)
        .map(|finding| (finding.kind, finding.position.map(|position| position.byte)))
        .collect()
}

fn names(source: &str) -> Vec<String> {
    preview(source, &PASSTHROUGH)
        .into_iter()
        .map(|render| render.name)
        .collect()
}

// --- parse errors -----------------------------------------------------------

#[test]
fn parse_error_unclosed_constructs_are_reported_at_their_opener() {
    let cases = [
        ("Room <<one/two", Construct::Plural, 5),
        ("Hi [[a/b", Construct::Choice, 3),
        ("Team __idle/busy", Construct::Resting, 5),
        ("{{LIVE ?? on", Construct::Conditional, 0),
        ("x \"\"bold:hey", Construct::Styled, 2),
        ("x \"\"bold\" y", Construct::Styled, 2),
        ("@@owner's room", Construct::Token, 0),
    ];
    for (source, construct, byte) in cases {
        assert_eq!(
            errors(source, &PASSTHROUGH),
            vec![(unclosed(construct), Some(byte))],
            "{source}"
        );
    }
}

#[test]
fn parse_error_closed_constructs_report_nothing() {
    for source in [
        "Room <<one/two>>",
        "Hi [[a/b]]",
        "Team __idle/busy__",
        "{{LIVE ?? on}}",
        "x \"\"bold:hey\"\"",
        "@@owner@@'s room",
        "[[list:colors]] $00# +# ##",
        "plain text, 50% off: a/b \\ c | d",
    ] {
        assert_eq!(errors(source, &PASSTHROUGH), vec![], "{source}");
    }
}

#[test]
fn parse_error_position_counts_bytes_and_characters() {
    let report = lint("Héllo @@num", &PASSTHROUGH);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(
        report.findings[0].position,
        Some(Position { byte: 7, char: 6 })
    );
    assert_eq!(report.findings[0].excerpt, "@@num");
}

#[test]
fn parse_error_inside_a_valid_block_points_at_the_inner_opener() {
    assert_eq!(
        errors("[[a/@@own]] room", &PASSTHROUGH),
        vec![(unclosed(Construct::Token), Some(4))]
    );
    assert_eq!(
        errors("[[a/\"\"b\"c]]", &PASSTHROUGH),
        vec![(unclosed(Construct::Styled), Some(4))]
    );
    assert_eq!(
        errors("{{LIVE ?? [[on // off}}", &PASSTHROUGH),
        vec![(unclosed(Construct::Choice), Some(10))]
    );
    assert_eq!(
        errors("\"\"b:x @@y\"\"", &PASSTHROUGH),
        vec![(unclosed(Construct::Token), Some(6))]
    );
}

#[test]
fn parse_error_valid_block_swallowed_by_an_outer_failure_is_not_reported() {
    // Only the root cause is reported: the plural itself closes.
    assert_eq!(
        errors("[[a/<<one/two>>", &PASSTHROUGH),
        vec![(unclosed(Construct::Choice), Some(0))]
    );
}

#[test]
fn parse_error_too_long_is_one_whole_template_finding() {
    let source = "a".repeat(MAX_TEMPLATE_BYTES + 1);
    let report = lint(&source, &PASSTHROUGH);
    assert_eq!(
        located(&report),
        vec![(FindingKind::ParseError(ParseError::TooLong), Some(0))]
    );
    assert_eq!(
        report.findings[0].excerpt,
        format!("{}…", "a".repeat(EXCERPT_CHARS))
    );
    assert!(lint(&"a".repeat(MAX_TEMPLATE_BYTES), &PASSTHROUGH).is_clean());
}

#[test]
fn parse_error_too_deep_is_one_whole_template_finding() {
    let deep = format!("{}x{}", "<<".repeat(70), ">>".repeat(70));
    assert_eq!(
        errors(&deep, &PASSTHROUGH),
        vec![(FindingKind::ParseError(ParseError::TooDeep), Some(0))]
    );
    let shallow = format!("{}x{}", "<<".repeat(8), ">>".repeat(8));
    assert_eq!(errors(&shallow, &PASSTHROUGH), vec![]);
}

// --- unknown tokens -----------------------------------------------------------

#[test]
fn unknown_token_is_reported_wherever_it_appears() {
    assert_eq!(
        errors("@@ownr@@'s room", &PASSTHROUGH),
        vec![(FindingKind::UnknownToken, Some(0))]
    );
    assert_eq!(
        errors("[[a/@@nope@@]] <<x/@@game@@>>", &PASSTHROUGH),
        vec![
            (FindingKind::UnknownToken, Some(4)),
            (FindingKind::UnknownToken, Some(19)),
        ]
    );
    // A condition is parsed for tokens even though it never renders.
    assert_eq!(
        errors("{{@@nmu@@ > 2 ?? x}} room", &PASSTHROUGH),
        vec![(FindingKind::UnknownToken, Some(2))]
    );
}

#[test]
fn unknown_token_known_names_are_accepted_in_any_case() {
    let all: String = KNOWN_TOKENS
        .iter()
        .map(|name| format!("@@{name}@@ "))
        .collect();
    assert_eq!(errors(&all, &PASSTHROUGH), vec![]);
    assert_eq!(errors("@@OWNER@@ @@Game_Name@@", &PASSTHROUGH), vec![]);
}

// --- empty renders ------------------------------------------------------------

#[test]
fn empty_render_lists_every_scenario_that_falls_back() {
    let report = lint("", &PASSTHROUGH);
    assert_eq!(
        located(&report),
        vec![(
            FindingKind::EmptyRender {
                scenarios: Scenario::ALL.to_vec()
            },
            None
        )]
    );
    assert_eq!(report.findings[0].excerpt, "");

    let report = lint("  @@stream_name@@ ", &PASSTHROUGH);
    let expected: Vec<Scenario> = Scenario::ALL
        .into_iter()
        .filter(|scenario| *scenario != Scenario::OwnerStreaming)
        .collect();
    assert_eq!(
        located(&report),
        vec![(
            FindingKind::EmptyRender {
                scenarios: expected
            },
            None
        )]
    );
    assert_eq!(report.findings[0].severity(), Severity::Warning);
    assert!(!report.has_errors());
}

#[test]
fn empty_render_follows_the_extension_policy() {
    let source = "{{LIVE ?? @@stream_name@@}}";
    // Passthrough keeps the block literal, so the name is never empty.
    assert!(lint(source, &PASSTHROUGH).is_clean());
    let report = lint(source, &TestConditions);
    assert!(matches!(
        &report.findings[..],
        [Finding { kind: FindingKind::EmptyRender { scenarios }, .. }] if scenarios.len() == 5
    ));
}

#[test]
fn empty_render_is_not_reported_for_a_name_in_every_scenario() {
    assert!(lint("Room ##", &PASSTHROUGH).is_clean());
    assert!(lint("@@game_name@@", &PASSTHROUGH).is_clean());
}

// --- conditions that never match --------------------------------------------

#[test]
fn condition_never_matching_in_any_scenario_is_reported() {
    let report = lint("Room {{PRIVATE ?? 🔒 // 🔊}}", &TestConditions);
    assert_eq!(
        located(&report),
        vec![(FindingKind::ConditionNeverMatches, Some(5))]
    );
    assert_eq!(report.findings[0].severity(), Severity::Warning);
    assert_eq!(report.findings[0].excerpt, "{{PRIVATE ?? 🔒 // 🔊}}");

    // Nested inside a choice option.
    assert_eq!(
        located(&lint("[[a/{{PRIVATE ?? x}}b]]", &TestConditions)),
        vec![(FindingKind::ConditionNeverMatches, Some(4))]
    );
}

#[test]
fn condition_matching_in_some_scenario_is_not_reported() {
    for source in [
        "Room {{FULL ?? 🔒 }}",
        "{{LIVE ?? 🔴 // ⚪}} @@owner@@",
        "{{WEEKEND ?? party // grind}}",
        "{{GAME ?? @@game_name@@ // chill}}",
    ] {
        assert!(lint(source, &TestConditions).is_clean(), "{source}");
    }
}

#[test]
fn condition_truth_is_unknown_under_passthrough() {
    // V5 keeps blocks literal, so no condition can be judged.
    assert!(lint("Room {{PRIVATE ?? x}}", &PASSTHROUGH).is_clean());
}

// --- bounds -----------------------------------------------------------------

#[test]
fn bounds_cap_the_number_of_findings() {
    let report = lint(&"@@ ".repeat(100), &PASSTHROUGH);
    assert_eq!(report.findings.len(), MAX_FINDINGS);
    assert!(report.truncated);
    assert!(!report.is_clean());
    assert!(report
        .findings
        .iter()
        .all(|finding| finding.kind == unclosed(Construct::Token)));

    let report = lint(&"\"\"a ".repeat(100), &PASSTHROUGH);
    assert!(report.findings.len() <= MAX_FINDINGS);
    assert!(report.truncated);

    let report = lint(&"{{PRIVATE ?? x}}".repeat(40), &TestConditions);
    assert!(report.findings.len() <= MAX_FINDINGS);
    assert!(report.truncated);
}

#[test]
fn bounds_cap_the_echoed_input() {
    let secretish = format!("@@{}", "s".repeat(500));
    let report = lint(&secretish, &PASSTHROUGH);
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.excerpt.chars().count(), EXCERPT_CHARS + 1);
    assert!(finding.excerpt.ends_with('…'));
    assert!(!finding.message().contains("ss"));
    assert!(finding.to_string().chars().count() <= MAX_MESSAGE_CHARS + EXCERPT_CHARS + 32);
}

#[test]
fn bounds_every_message_fits() {
    let constructs = [
        Construct::Token,
        Construct::Plural,
        Construct::Choice,
        Construct::Resting,
        Construct::Conditional,
        Construct::Styled,
    ];
    let mut kinds: Vec<FindingKind> = constructs.into_iter().map(unclosed).collect();
    kinds.extend([
        FindingKind::ParseError(ParseError::TooLong),
        FindingKind::ParseError(ParseError::TooDeep),
        FindingKind::UnknownToken,
        FindingKind::EmptyRender {
            scenarios: Scenario::ALL.to_vec(),
        },
        FindingKind::ConditionNeverMatches,
    ]);
    for kind in kinds {
        let finding = Finding {
            kind,
            position: None,
            excerpt: String::new(),
        };
        assert!(
            finding.message().chars().count() <= MAX_MESSAGE_CHARS,
            "{}",
            finding.message()
        );
    }
}

// --- preview ----------------------------------------------------------------

#[test]
fn preview_renders_the_six_fixed_scenarios_in_order() {
    let renders = preview("## @@owner@@ · @@game_name@@", &PASSTHROUGH);
    let order: Vec<Scenario> = renders.iter().map(|render| render.scenario).collect();
    assert_eq!(order, Scenario::ALL.to_vec());
    assert_eq!(
        names("## @@owner@@ · @@game_name@@"),
        GOLDEN_OWNER_GAME.map(str::to_string).to_vec()
    );
    assert_eq!(
        names("@@num@@ <<member/members>> · @@slots@@ · @@party_state@@"),
        GOLDEN_COUNTS.map(str::to_string).to_vec()
    );
    assert_eq!(
        names("@@weekday@@ @@month@@ @@hour@@ @@nato@@ [[a/b/c]] @@random_emoji@@"),
        GOLDEN_TIME_RANDOM.map(str::to_string).to_vec()
    );
}

const GOLDEN_OWNER_GAME: [&str; 6] = ["", "", "", "", "", ""];
const GOLDEN_COUNTS: [&str; 6] = ["", "", "", "", "", ""];
const GOLDEN_TIME_RANDOM: [&str; 6] = ["", "", "", "", "", ""];

#[test]
fn preview_marks_fallback_names() {
    for render in preview("   ", &PASSTHROUGH) {
        assert!(render.used_fallback);
        assert_eq!(render.name, DEFAULT_FALLBACK_NAME);
    }
    let renders = preview("@@stream_name@@", &PASSTHROUGH);
    let live: Vec<bool> = renders.iter().map(|render| !render.used_fallback).collect();
    assert_eq!(live, vec![false, false, true, false, false, false]);
    assert_eq!(renders[2].name, "Ranked grind");
}

#[test]
fn preview_scenario_fixtures_are_fixed() {
    let members: Vec<u32> = Scenario::ALL
        .iter()
        .map(|scenario| scenario.context().member_count)
        .collect();
    assert_eq!(members, vec![1, 3, 2, 4, 4, 2]);
    let limits: Vec<u32> = Scenario::ALL
        .iter()
        .map(|scenario| scenario.context().user_limit)
        .collect();
    assert_eq!(limits, vec![0, 0, 0, 0, 5, 2]);
    for scenario in Scenario::ALL {
        let room = scenario.context();
        assert_eq!(room.seed, SCENARIO_SEED);
        assert_eq!(room, scenario.context(), "{scenario}");
    }
    assert_eq!(
        names("@@game_name@@"),
        vec!["General", "Apex", "Chess", "Apex", "Chess", "General"]
    );
    assert_eq!(
        names("@@weekday@@"),
        vec![
            "Monday",
            "Saturday",
            "Wednesday",
            "Sunday",
            "Friday",
            "Tuesday"
        ]
    );
    assert_eq!(
        Scenario::ALL.map(Scenario::label),
        [
            "solo, no game",
            "three in a game",
            "owner streaming",
            "game with party info",
            "nearly full",
            "locked"
        ]
    );
}

#[test]
fn preview_every_known_token_renders_in_some_scenario() {
    for name in KNOWN_TOKENS {
        let renders = preview(&format!("@@{name}@@"), &PASSTHROUGH);
        assert!(
            renders.iter().any(|render| !render.used_fallback),
            "{name} never renders"
        );
    }
}

#[test]
fn preview_and_lint_are_byte_identical_across_runs() {
    let source = "{{LIVE ?? 🔴 }}@@owner@@ [[a/b/c]] @@random_emoji@@ @@nope@@ <<x";
    assert_eq!(
        preview(source, &TestConditions),
        preview(source, &TestConditions)
    );
    assert_eq!(lint(source, &TestConditions), lint(source, &TestConditions));
    assert_eq!(names(source), names(source));
}

// --- properties -------------------------------------------------------------

const PIECES: [&str; 26] = [
    "@@", "<<", ">>", "[[", "]]", "__", "{{", "}}", "??", "//", "\"\"", ":", "/", "|", "\\", "##",
    "$0#", "list:", "owner", "nope", "LIVE", "PRIVATE", " ", "é", "x", "\n",
];

fn template_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(PIECES.to_vec()), 0..48)
        .prop_map(|pieces| pieces.concat())
}

fn opener(construct: Construct) -> &'static str {
    match construct {
        Construct::Token => "@@",
        Construct::Plural => "<<",
        Construct::Choice => "[[",
        Construct::Resting => "__",
        Construct::Conditional => "{{",
        Construct::Styled => "\"\"",
    }
}

fn assert_well_formed(source: &str, report: &LintReport) {
    assert!(report.findings.len() <= MAX_FINDINGS);
    let mut last_error = Some(0);
    let mut seen_warning = false;
    for finding in &report.findings {
        assert!(finding.message().chars().count() <= MAX_MESSAGE_CHARS);
        assert!(finding.excerpt.chars().count() <= EXCERPT_CHARS + 1);
        match finding.severity() {
            Severity::Error => {
                assert!(!seen_warning, "errors come first");
                let byte = finding.position.map(|position| position.byte);
                assert!(byte >= last_error, "errors are in source order");
                last_error = byte;
            }
            Severity::Warning => seen_warning = true,
        }
        let Some(position) = finding.position else {
            assert_eq!(finding.excerpt, "");
            continue;
        };
        assert!(source.is_char_boundary(position.byte));
        assert_eq!(position.char, source[..position.byte].chars().count());
        let shown = finding
            .excerpt
            .strip_suffix('…')
            .unwrap_or(&finding.excerpt);
        assert!(source[position.byte..].starts_with(shown));
        let expected = match &finding.kind {
            FindingKind::ParseError(ParseError::Unclosed(construct)) => opener(*construct),
            FindingKind::UnknownToken => "@@",
            FindingKind::ConditionNeverMatches => "{{",
            _ => continue,
        };
        // Findings inside an unsplittable conditional anchor at its `{{`.
        assert!(
            shown.starts_with(expected) || shown.starts_with("{{"),
            "{finding:?} in {source:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_lint_findings_are_bounded_and_located(source in template_strategy()) {
        assert_well_formed(&source, &lint(&source, &PASSTHROUGH));
        assert_well_formed(&source, &lint(&source, &TestConditions));
    }

    #[test]
    fn property_lint_and_preview_are_deterministic(source in template_strategy()) {
        prop_assert_eq!(lint(&source, &TestConditions), lint(&source, &TestConditions));
        prop_assert_eq!(preview(&source, &TestConditions), preview(&source, &TestConditions));
    }

    #[test]
    fn property_preview_names_are_never_empty(source in template_strategy()) {
        for render in preview(&source, &PASSTHROUGH) {
            prop_assert!(!render.name.trim().is_empty());
            prop_assert_eq!(render.used_fallback, render.name == DEFAULT_FALLBACK_NAME);
        }
    }
}
