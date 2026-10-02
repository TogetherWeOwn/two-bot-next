//! Hermetic V12 acceptance cases for the pure assistant-output validator:
//! one assistant template is checked against the six fixed preview scenarios.
//! Each of the three §V12 failure classes (empty names, never-matching
//! conditions, unknown tokens) gets a failing-output refusal test and a
//! passing-output accept test. No network, database, Discord or staging
//! identity is used.

use two_bot_core::voice_assistant_validate::{validate_template, TemplateRefusal};
use two_bot_core::voice_naming::{parse, Evaluation, ExtensionPolicy, Template};
use two_bot_core::voice_template_lint::Scenario;

/// Conditions policy with known truth values over the six scenarios, mirroring
/// the lint suite's stand-in: `LIVE` fires only when someone streams,
/// `FULL` only when the room hit its limit, `GAME` when anyone plays,
/// `WEEKEND` on Saturday/Sunday, and `PRIVATE` nowhere.
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
        evaluation.evaluate(body)
    }
}

const CONDITIONS: TestConditions = TestConditions;

fn accept_names(template: &str) -> Vec<String> {
    validate_template(template, &CONDITIONS)
        .expect("template should validate")
        .previews
        .into_iter()
        .map(|render| render.name)
        .collect()
}

// --- empty names ------------------------------------------------------------

#[test]
fn empty_name_refusal_lists_every_fallback_scenario() {
    // Only the owner-streaming scenario has a stream title.
    let Err(TemplateRefusal::EmptyName { scenarios }) =
        validate_template("@@stream_name@@", &CONDITIONS)
    else {
        panic!("expected an empty-name refusal");
    };
    let expected: Vec<Scenario> = Scenario::ALL
        .into_iter()
        .filter(|scenario| *scenario != Scenario::OwnerStreaming)
        .collect();
    assert_eq!(scenarios, expected);

    // A blank source is empty everywhere.
    let Err(TemplateRefusal::EmptyName { scenarios }) = validate_template("", &CONDITIONS) else {
        panic!("expected an empty-name refusal");
    };
    assert_eq!(scenarios, Scenario::ALL.to_vec());
}

#[test]
fn empty_name_accept_renders_a_name_in_all_six_scenarios() {
    let valid = validate_template("@@owner@@'s room ##", &CONDITIONS).expect("valid template");
    let order: Vec<Scenario> = valid
        .previews
        .iter()
        .map(|render| render.scenario)
        .collect();
    assert_eq!(order, Scenario::ALL.to_vec());
    assert!(
        valid.previews.iter().all(|render| !render.used_fallback),
        "a preview fell back"
    );
    assert_eq!(
        valid.previews.map(|render| render.name),
        [
            "Avery's room #1",
            "Blake's room #2",
            "Casey's room #3",
            "Devon's room #4",
            "Emery's room #5",
            "Finley's room #6",
        ]
    );
}

// --- never-matching conditions ----------------------------------------------

#[test]
fn never_matching_condition_refusal_names_nothing_else() {
    // PRIVATE is false in every scenario; the refusal says so without quoting
    // the source, so the message never leaks member data.
    assert_eq!(
        validate_template("Room {{PRIVATE ?? locked // open}}", &CONDITIONS),
        Err(TemplateRefusal::NeverMatchingCondition)
    );
    assert!(
        TemplateRefusal::NeverMatchingCondition
            .to_string()
            .chars()
            .count()
            <= 120,
        "refusal message is unbounded"
    );
}

#[test]
fn never_matching_condition_accept_when_true_somewhere() {
    // FULL fires only in the locked scenario; GAME and WEEKEND fire elsewhere.
    assert_eq!(
        accept_names("@@owner@@{{FULL ?? · full // · open}}"),
        [
            "Avery · open",
            "Blake · open",
            "Casey · open",
            "Devon · open",
            "Emery · open",
            "Finley · full",
        ]
    );
    assert_eq!(
        accept_names("{{GAME ?? @@game_name@@ // chill}}"),
        ["chill", "Apex", "Chess", "Apex", "Chess", "chill"]
    );
    assert_eq!(
        accept_names("{{WEEKEND ?? party // grind}}"),
        ["grind", "party", "grind", "party", "grind", "grind"]
    );
}

// --- unknown tokens ---------------------------------------------------------

#[test]
fn unknown_token_refusal_covers_misspellings_and_other_languages() {
    // A misspelled English token and a non-English token name are refused the
    // same way: tokens are always English, and anything else does not exist.
    assert_eq!(
        validate_template("@@ownr@@'s room", &CONDITIONS),
        Err(TemplateRefusal::UnknownToken)
    );
    assert_eq!(
        validate_template("@@propietario@@'s room", &CONDITIONS),
        Err(TemplateRefusal::UnknownToken)
    );
    // Tokens that never render are still refused, not silently dropped.
    assert_eq!(
        validate_template("[[a/@@nope@@]] <<x/@@game@@>>", &CONDITIONS),
        Err(TemplateRefusal::UnknownToken)
    );
    // A token name with non-ASCII letters never parses as a token: it would
    // show as raw syntax, so it is refused too.
    assert_eq!(
        validate_template("@@propriétaire@@'s room", &CONDITIONS),
        Err(TemplateRefusal::UnbalancedSyntax)
    );
}

#[test]
fn unknown_token_accept_for_every_engine_token() {
    // Every token the V5 engine substitutes is accepted; casing is folded by
    // the parser, and `creator` is the alias of `owner`.
    for token in [
        "owner",
        "creator",
        "original_creator",
        "num",
        "num_others",
        "num_live",
        "limit",
        "slots",
        "game_name",
        "stream_name",
        "num_playing",
        "party_size",
        "party_state",
        "party_details",
        "weekday",
        "month",
        "hour",
        "random_emoji",
        "nato",
        "OWNER",
    ] {
        let source = format!("Room @@{token}@@");
        assert!(
            validate_template(&source, &CONDITIONS).is_ok(),
            "engine token {token} was refused"
        );
    }
}

// --- ordering and syntax ----------------------------------------------------

#[test]
fn refusal_priority_is_syntax_then_tokens_then_empty_then_conditions() {
    // An unknown token that also renders empty reports the token, not the
    // empty renders it causes.
    assert_eq!(
        validate_template("@@nope@@", &CONDITIONS),
        Err(TemplateRefusal::UnknownToken)
    );
    // An unclosed delimiter is refused even though the literal text would
    // otherwise render in every scenario.
    assert_eq!(
        validate_template("Room <<one/two", &CONDITIONS),
        Err(TemplateRefusal::UnbalancedSyntax)
    );
    // A never-matching condition whose branches would also fall back reports
    // the empty name first, matching the §V12 validation order.
    let Err(TemplateRefusal::EmptyName { scenarios }) =
        validate_template("{{PRIVATE ?? @@stream_name@@}}", &CONDITIONS)
    else {
        panic!("expected an empty-name refusal");
    };
    assert_eq!(scenarios.len(), 6);
}

#[test]
fn template_past_the_lint_scan_bound_is_refused_not_accepted_unchecked() {
    // The lint inspects at most 32 conditional blocks. A never-matching
    // condition past that bound goes unchecked, so the whole template is
    // refused instead of accepted.
    let checked = "{{GAME ?? a // b}}".repeat(32);
    assert!(validate_template(&checked, &CONDITIONS).is_ok());
    let hidden = format!("{checked}{{{{PRIVATE ?? c // d}}}}");
    assert_eq!(
        validate_template(&hidden, &CONDITIONS),
        Err(TemplateRefusal::TooComplex)
    );
}

#[test]
fn validated_previews_are_exactly_the_admin_preview() {
    let valid =
        validate_template("## @@owner@@ · @@game_name@@", &CONDITIONS).expect("valid template");
    assert_eq!(valid.template, "## @@owner@@ · @@game_name@@");
    assert_eq!(
        valid.previews.clone().map(|render| render.name),
        [
            "#1 Avery · General",
            "#2 Blake · Apex",
            "#3 Casey · Chess",
            "#4 Devon · Apex",
            "#5 Emery · Chess",
            "#6 Finley · General",
        ]
    );
    // Deterministic: the same source validates identically twice.
    assert_eq!(
        validate_template("## @@owner@@ · @@game_name@@", &CONDITIONS),
        Ok(valid)
    );
}

#[test]
fn validation_is_pure_and_member_free() {
    // No API takes a name, presence snapshot or ID: the only input is the
    // template source, and refusals repeat no source text.
    for refusal in [
        TemplateRefusal::EmptyName {
            scenarios: Scenario::ALL.to_vec(),
        },
        TemplateRefusal::NeverMatchingCondition,
        TemplateRefusal::UnknownToken,
        TemplateRefusal::UnbalancedSyntax,
        TemplateRefusal::TooComplex,
    ] {
        let message = refusal.to_string();
        assert!(
            !message.contains("Avery"),
            "{refusal:?} echoes a scenario name"
        );
        assert!(
            message.chars().count() <= 120,
            "{refusal:?} message is unbounded"
        );
    }
}
