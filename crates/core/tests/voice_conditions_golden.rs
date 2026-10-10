//! The V6b independent golden corpus (`fixtures/voice_conditions_golden.json`,
//! [TOG-12468](/TOG/issues/TOG-12468)) and its evaluation through the merged
//! `voice_conditions` evaluator ([TOG-12528](/TOG/issues/TOG-12528)).
//!
//! The structural gate pins the fixture contract independently of the
//! evaluator:
//! - every condition head has at least one row (`MIN_PER_HEAD`);
//! - unknown-head rows refuse (`expected.output == "no"`);
//! - `spec` rows cite §V6 alone and `choice` rows cite the TOG-12189
//!   `docs/voice-conditions-core.md` line they pin;
//! - every row's first V5 conditional node is a verbatim slice of its input,
//!   and every expected output is the text around that node plus a slice of
//!   the node source, so no row invents text; `rule:verbatim` rows select
//!   non-empty branch text (verbatim preservation is parser-level);
//! - `shared_case` rows agree byte-for-byte with `tests/voice_templates/corpus.json`;
//! - every context equals the shared corpus context of the same name, except
//!   the `DERIVED_CONTEXTS` allowlist, which the shared corpus must not define;
//! - the spec pin matches the current `docs/voice-rooms.md` SHA-256.
//!
//! The eval test renders every row's `input` through [`Conditions`] over the
//! row's inlined context and asserts `expected.output` byte-for-byte. Expected
//! outputs are never edited here: a disagreement is a finding for the
//! evaluator owner, reported as row ID, basis, expected and actual.

#[path = "support/voice_corpus_context.rs"]
mod voice_corpus_context;

use std::collections::BTreeMap;

use serde::Deserialize;
use two_bot_core::voice_conditions::Conditions;
use two_bot_core::voice_naming::{parse, render, Extension, Segment};
use voice_corpus_context::{condition_facts, room_context, CorpusContext};

const GOLDEN: &str = include_str!("fixtures/voice_conditions_golden.json");
const SHARED: &str = include_str!("../../../tests/voice_templates/corpus.json");

/// Minimum rows per condition head. The acceptance bar is one row per head;
/// the count only guards against silent fixture shrinkage.
const MIN_PER_HEAD: usize = 1;

/// Contexts synthesised by the generator (`derived_contexts`) because the
/// shared corpus has no equivalent; every other context is copied verbatim.
const DERIVED_CONTEXTS: &[&str] = &["v6b-party-capped"];

/// Every condition head the fixture must cover.
const HEADS: &[&str] = &[
    "condition:GAME",
    "condition:ROLE:id",
    "condition:ANY_ROLE:id",
    "condition:MEMBER:id",
    "condition:OWNER:id",
    "condition:OWNER",
    "condition:PLAYING",
    "condition:LIVE",
    "condition:LIVE_DISCORD",
    "condition:LIVE_EXTERNAL",
    "condition:ANY_LIVE",
    "condition:PLAYERS",
    "condition:MAX",
    "condition:RICH",
    "condition:FULL",
    "condition:PRIVATE",
    "condition:WEEKEND",
    "condition:WEEKDAY",
    "condition:MONTH",
    "compare:<",
    "compare:>",
    "compare:<=",
    "compare:>=",
    "compare:=",
    "compare:!=",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    #[allow(dead_code)]
    version: u32,
    spec: Spec,
    #[allow(dead_code)]
    description: String,
    contexts: BTreeMap<String, serde_json::Value>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    input: String,
    context: String,
    expected: Expected,
    covers: Vec<String>,
    basis: String,
    source: Source,
    #[serde(default)]
    shared_case: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    kind: String,
    output: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    spec: String,
    legacy: String,
}

#[derive(Deserialize)]
struct Shared {
    spec: Spec,
    contexts: BTreeMap<String, serde_json::Value>,
    cases: Vec<SharedCase>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedCase {
    id: String,
    input: String,
    context: String,
    expected: SharedExpected,
    #[allow(dead_code)]
    covers: Vec<String>,
}

#[derive(Deserialize)]
struct SharedExpected {
    kind: String,
    #[serde(default)]
    output: Option<String>,
}

fn fixture() -> Fixture {
    serde_json::from_str(GOLDEN).expect("golden fixture is JSON")
}

/// The first top-level conditional node source as V5 carries it: `{{...}}`
/// renders through unchanged (PassthroughExtensions), so branch text reaches
/// the evaluator verbatim.
fn conditional_source(template: &str) -> Option<String> {
    parse(template)
        .0
        .into_iter()
        .find_map(|segment| match segment {
            Segment::Extension(Extension::Conditional { source, .. }) => Some(source),
            _ => None,
        })
}

/// Whether a citation names a `docs/voice-conditions-core.md` line (`L21`,
/// `L28-34`) after the document path.
fn cites_core_line(spec: &str) -> bool {
    spec.split_once("docs/voice-conditions-core.md")
        .is_some_and(|(_, cite)| {
            cite.split(" L")
                .skip(1)
                .any(|line| line.starts_with(|c: char| c.is_ascii_digit()))
        })
}

#[test]
fn heads_have_coverage_unknown_heads_refuse_and_rows_cite_sources() {
    let fixture = fixture();
    assert_eq!(fixture.spec.path, "docs/voice-rooms.md");
    assert_eq!(
        fixture.spec.sha256, "79907c4c74fe43f220dc9d19e3b3f592d553767a7af2be4eadf567ee7aaa9be9",
        "spec moved: update rows consciously, not by regeneration"
    );
    let mut unknown = 0;
    let mut nested = 0;
    let mut ci = 0;
    let mut verbatim = 0;
    for head in HEADS {
        let rows = fixture
            .cases
            .iter()
            .filter(|case| case.covers.iter().any(|tag| tag == head))
            .count();
        assert!(
            rows >= MIN_PER_HEAD,
            "head {head} has {rows} rows, minimum {MIN_PER_HEAD}"
        );
    }
    for case in &fixture.cases {
        assert!(
            fixture.contexts.contains_key(&case.context),
            "{} names a missing context",
            case.id
        );
        assert!(!case.covers.is_empty(), "{} covers nothing", case.id);
        assert!(
            case.basis == "spec" || case.basis == "choice",
            "{} has basis {:?}",
            case.id,
            case.basis
        );
        assert!(!case.source.spec.is_empty(), "{} cites no spec", case.id);
        // `spec` rows follow from §V6 alone; `choice` rows pin a TOG-12189
        // decision and cite the `docs/voice-conditions-core.md` line for it.
        if case.basis == "spec" {
            assert_eq!(
                case.source.spec, "docs/voice-rooms.md §V6",
                "{}: a spec row cites only §V6",
                case.id
            );
        } else {
            assert!(
                cites_core_line(&case.source.spec),
                "{}: a choice row cites a docs/voice-conditions-core.md line",
                case.id
            );
        }
        assert!(
            !case.source.legacy.is_empty(),
            "{} cites no legacy template source",
            case.id
        );
        if case
            .covers
            .iter()
            .any(|tag| tag == "rule:unknown-condition")
        {
            assert_eq!(
                case.expected.output, "no",
                "{}: unknown heads refuse",
                case.id
            );
            unknown += 1;
        }
        if case.covers.iter().any(|tag| tag == "rule:nested") {
            nested += 1;
        }
        if case.covers.iter().any(|tag| tag == "rule:case") {
            ci += 1;
        }
        // Verbatim-output preservation: V5 carries the node source through
        // untouched, so the evaluator receives branch text as written and the
        // oracle output can only be the surrounding text plus node text.
        let Some(source) = conditional_source(&case.input) else {
            panic!("{}: input has no top-level conditional node", case.id);
        };
        let Some((before, after)) = case.input.split_once(source.as_str()) else {
            panic!("{}: node source is not a slice of the input", case.id);
        };
        let Some(branch) = case
            .expected
            .output
            .strip_prefix(before)
            .and_then(|rest| rest.strip_suffix(after))
        else {
            panic!("{}: output drops the text around the node", case.id);
        };
        assert!(
            source.contains(branch),
            "{}: output {branch:?} is not text of {source:?}",
            case.id
        );
        if case.covers.iter().any(|tag| tag == "rule:verbatim") {
            assert!(
                !branch.is_empty(),
                "{}: verbatim row selects no branch text",
                case.id
            );
            verbatim += 1;
        }
    }
    assert!(unknown >= 1, "unknown-head refusal needs rows");
    assert!(nested >= 1, "nesting needs rows");
    assert!(ci >= 1, "case-insensitivity needs rows");
    assert!(verbatim >= 1, "verbatim-output preservation needs rows");
    assert_eq!(fixture.cases.len(), 90, "do not silently shrink the corpus");
}

#[test]
fn shared_rows_match_the_voice_template_corpus() {
    let fixture = fixture();
    let shared: Shared = serde_json::from_str(SHARED).expect("shared corpus is JSON");
    assert_eq!(
        shared.spec.sha256, fixture.spec.sha256,
        "shared corpus moved to another spec revision"
    );
    let by_id: BTreeMap<&str, &SharedCase> = shared
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect();
    let mut checked = 0;
    for case in &fixture.cases {
        let Some(shared_id) = case.shared_case.as_deref() else {
            continue;
        };
        let shared = by_id
            .get(shared_id)
            .unwrap_or_else(|| panic!("{}: shared case {shared_id} missing", case.id));
        assert_eq!(shared.input, case.input, "{shared_id}: input drifted");
        assert_eq!(shared.context, case.context, "{shared_id}: context drifted");
        assert_eq!(
            shared.expected.kind, "exact",
            "{shared_id}: no longer exact"
        );
        assert_eq!(
            shared.expected.output.as_deref(),
            Some(case.expected.output.as_str()),
            "{shared_id}: expected output drifted"
        );
        checked += 1;
    }
    assert_eq!(checked, 39, "every shared_case link is asserted");
}

#[test]
fn contexts_are_shared_verbatim_except_the_derived_allowlist() {
    let fixture = fixture();
    let shared: Shared = serde_json::from_str(SHARED).expect("shared corpus is JSON");
    for name in DERIVED_CONTEXTS {
        assert!(
            fixture.contexts.contains_key(*name),
            "derived context {name} is unused: drop it from the allowlist"
        );
    }
    let mut copied = 0;
    for (name, body) in &fixture.contexts {
        if DERIVED_CONTEXTS.contains(&name.as_str()) {
            assert!(
                !shared.contexts.contains_key(name),
                "derived context {name} shadows a shared corpus context"
            );
            continue;
        }
        let Some(shared_body) = shared.contexts.get(name) else {
            panic!("context {name} is neither shared nor in DERIVED_CONTEXTS");
        };
        assert_eq!(
            body, shared_body,
            "context {name} drifted from the shared corpus"
        );
        copied += 1;
    }
    assert_eq!(copied, 26, "26 contexts are copied verbatim");
}

#[test]
fn every_row_renders_its_expected_output_through_the_evaluator() {
    let fixture = fixture();
    let mut rooms = BTreeMap::new();
    for (name, body) in &fixture.contexts {
        let context: CorpusContext = serde_json::from_value(body.clone())
            .unwrap_or_else(|error| panic!("context {name}: {error}"));
        let room = room_context(&context);
        let facts = condition_facts(&context);
        // `GAME` must agree with the `@@game@@` the same name renders.
        let shown = match facts.games.as_slice() {
            [] => context.settings.no_game.clone(),
            titles => titles.join(" & "),
        };
        assert_eq!(room.game_name, shown, "context {name}: GAME facts drift");
        rooms.insert(name.as_str(), (room, facts));
    }
    let mut failures = Vec::new();
    for case in &fixture.cases {
        assert_eq!(case.expected.kind, "exact", "{}: not exact", case.id);
        let (room, facts) = &rooms[case.context.as_str()];
        let actual = render(&parse(&case.input), room, &Conditions::new(facts));
        if actual != case.expected.output {
            failures.push(format!(
                "{} ({}): expected {:?}, actual {actual:?}",
                case.id, case.basis, case.expected.output
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} row(s) disagree with the evaluator:\n{}",
        failures.len(),
        fixture.cases.len(),
        failures.join("\n")
    );
    assert!(
        rooms.contains_key("v6b-party-capped"),
        "the derived context is evaluated"
    );
}
