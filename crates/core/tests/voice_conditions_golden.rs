//! Structural gate for the V6b independent golden corpus
//! (`fixtures/voice_conditions_golden.json`, [TOG-12468](/TOG/issues/TOG-12468)).
//!
//! This test never touches the `voice_conditions` evaluator (TOG-12189 owns
//! it; PR #246 unmerged at authoring time). It pins the fixture contract the
//! eval-wiring follow-up will consume:
//! - every condition head has at least one row (`MIN_PER_HEAD`);
//! - unknown-head rows refuse (`expected.output == "no"`);
//! - every row parses as a V5 conditional node with its branch text preserved
//!   verbatim (verbatim-output preservation is parser-level, not evaluator);
//! - `shared_case` rows agree byte-for-byte with `tests/voice_templates/corpus.json`;
//! - the spec pin matches the current `docs/voice-rooms.md` SHA-256.
//!
//! The eval follow-up (blocked on TOG-12189) renders `input` through the
//! merged evaluator over the inlined `contexts` and asserts `expected.output`.

use std::collections::BTreeMap;

use serde::Deserialize;
use two_bot_core::voice_naming::{parse, Extension, Segment};

const GOLDEN: &str = include_str!("fixtures/voice_conditions_golden.json");
const SHARED: &str = include_str!("../../../tests/voice_templates/corpus.json");

/// Minimum rows per condition head. The acceptance bar is one row per head;
/// the count only guards against silent fixture shrinkage.
const MIN_PER_HEAD: usize = 1;

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
    #[allow(dead_code)]
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

#[test]
fn heads_have_coverage_unknown_heads_refuse_and_rows_cite_sources() {
    let fixture = fixture();
    assert_eq!(fixture.spec.path, "docs/voice-rooms.md");
    assert_eq!(
        fixture.spec.sha256, "11d8af8076659c221279cbbbadb477d6c0898c4ef284eef9857b93635fdb44fb",
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
        if case.covers.iter().any(|tag| tag == "rule:verbatim") {
            verbatim += 1;
        }
        // Verbatim-output preservation: V5 carries the node source through
        // untouched, so the evaluator receives branch text as written.
        let Some(source) = conditional_source(&case.input) else {
            panic!("{}: input is not a top-level conditional node", case.id);
        };
        assert!(
            source.starts_with("{{") && source.ends_with("}}"),
            "{}: node source keeps its braces",
            case.id
        );
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
