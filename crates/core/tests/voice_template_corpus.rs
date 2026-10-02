//! Runs the independent voice-template corpus (`tests/voice_templates/`)
//! against the V5 naming engine with [`PassthroughExtensions`].
//!
//! The corpus was authored from `docs/voice-rooms.md`, not from this engine.
//! Expected outputs are never edited here: a spec-vs-engine disagreement is a
//! finding for the engine owner, reported as case ID plus expected vs actual.
//!
//! - `exact` cases compare byte-for-byte.
//! - `invariant` cases check nonempty output, the 100-character ceiling,
//!   repeat-render equality, `allowed_outputs` and `casefold_equals`; every
//!   `stability_groups` entry must render the same output across contexts.
//! - `deferred` cases await spec clarification: skipped and counted.
//!
//! Cases that need V6 conditionals or styling are listed in [`PENDING`]. The
//! list may only shrink: a pending case that starts passing fails the test
//! until its ID is removed, and any non-pending case that fails is a
//! regression.

#[path = "support/voice_corpus_context.rs"]
mod voice_corpus_context;

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use two_bot_core::voice_naming::{parse, render, PassthroughExtensions, RoomContext};
use voice_corpus_context::{room_context, CorpusContext};

const CORPUS: &str = include_str!("../../../tests/voice_templates/corpus.json");

/// Non-deferred cases that need the V6 conditional or styling policy. Remove
/// an ID once the engine renders that case as the corpus expects.
const PENDING: &[&str] = &[
    // V6 conditionals: `{{cond ?? yes // no}}`.
    "weekend-Monday",
    "weekend-Tuesday",
    "weekend-Wednesday",
    "weekend-Thursday",
    "weekend-Friday",
    "weekend-Saturday",
    "weekend-Sunday",
    "compare-equal-lt",
    "compare-equal-gt",
    "compare-equal-le",
    "compare-equal-ge",
    "compare-equal-eq",
    "compare-equal-ne",
    "compare-num-limit",
    "compare-slots-limit",
    "compare-hour",
    "compare-room-number",
    "compare-literal",
    "full-unlimited",
    "full-full",
    "full-space",
    "private-temporary-False",
    "private-temporary-True",
    "private-standalone-False",
    "private-standalone-True",
    "role-solo",
    "role-role-owner",
    "role-role-other",
    "any-role-solo",
    "any-role-role-owner",
    "any-role-role-other",
    "person-condition-MEMBER:owner",
    "person-condition-MEMBER:absent",
    "person-condition-OWNER:owner",
    "person-condition-OWNER:absent",
    "game-condition-GAME:Ape",
    "game-condition-GAME=Apex",
    "game-condition-GAME!=Apex",
    "game-condition-GAME=Chess",
    "condition-unknown",
    "condition-name-not-expanded",
    "optional-else-false",
    "optional-else-true",
    "nested-role-owner",
    "nested-private-temporary-True",
    "nested-solo",
    "order-condition-token-style",
    "discarded-branch",
    // V6 styling: `""mode:text""`, including the seeded `rand` invariants.
    "style-upper",
    "style-caps",
    "style-lower",
    "style-title",
    "style-swap",
    "style-remshort",
    "style-1w",
    "style-2w",
    "style-chain",
    "style-unknown",
    "style-unknown-chain",
    "random-case-stable",
    "random-case-stable-rename",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    version: u32,
    #[allow(dead_code)]
    spec: serde_json::Value,
    contexts: BTreeMap<String, CorpusContext>,
    cases: Vec<Case>,
    stability_groups: Vec<StabilityGroup>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    input: String,
    context: String,
    expected: Expected,
    #[allow(dead_code)]
    covers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Expected {
    Exact {
        output: String,
    },
    Invariant {
        nonempty: bool,
        max_characters: usize,
        stable_for_same_context: bool,
        #[serde(default)]
        allowed_outputs: Option<Vec<String>>,
        #[serde(default)]
        casefold_equals: Option<String>,
    },
    Deferred {
        ambiguity_id: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StabilityGroup {
    id: String,
    case_ids: Vec<String>,
}

fn render_case(case: &Case, contexts: &BTreeMap<String, RoomContext>) -> String {
    let context = &contexts[&case.context];
    render(&parse(&case.input), context, &PassthroughExtensions)
}

/// Why an invariant case fails, or `None` when every declared constraint holds.
fn invariant_failure(case: &Case, contexts: &BTreeMap<String, RoomContext>) -> Option<String> {
    let Expected::Invariant {
        nonempty,
        max_characters,
        stable_for_same_context,
        allowed_outputs,
        casefold_equals,
    } = &case.expected
    else {
        unreachable!("invariant case");
    };
    let output = render_case(case, contexts);
    let mut failures = Vec::new();
    if *nonempty && output.is_empty() {
        failures.push("empty output".to_string());
    }
    if output.chars().count() > *max_characters {
        failures.push(format!("over {max_characters} characters"));
    }
    if *stable_for_same_context && render_case(case, contexts) != output {
        failures.push("repeat render differs".to_string());
    }
    if let Some(allowed) = allowed_outputs
        .as_ref()
        .filter(|allowed| !allowed.contains(&output))
    {
        failures.push(format!("not one of {allowed:?}"));
    }
    if let Some(target) = casefold_equals {
        // Lowercasing equals Unicode case folding for ASCII targets only.
        assert!(target.is_ascii(), "{}: non-ASCII case-fold target", case.id);
        if output.to_lowercase() != *target {
            failures.push(format!(
                "case-folds to {:?}, not {target:?}",
                output.to_lowercase()
            ));
        }
    }
    (!failures.is_empty())
        .then(|| format!("{}: {} (actual {output:?})", case.id, failures.join("; ")))
}

#[test]
fn voice_template_corpus_matches_engine() {
    let corpus: Corpus = serde_json::from_str(CORPUS).expect("corpus parses");
    assert_eq!(corpus.version, 1, "corpus version");
    let contexts: BTreeMap<String, RoomContext> = corpus
        .contexts
        .iter()
        .map(|(name, context)| (name.clone(), room_context(context)))
        .collect();
    let pending: BTreeSet<&str> = PENDING.iter().copied().collect();
    assert_eq!(pending.len(), PENDING.len(), "duplicate PENDING entry");

    let mut ids = BTreeSet::new();
    let (mut exact_pass, mut exact_pending, mut invariant_pass, mut invariant_pending) =
        (0usize, 0usize, 0usize, 0usize);
    let mut deferred: BTreeMap<&str, usize> = BTreeMap::new();
    let mut regressions = Vec::new();
    let mut now_passing = Vec::new();

    for case in &corpus.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate case ID {}",
            case.id
        );
        assert!(
            contexts.contains_key(&case.context),
            "{}: missing context",
            case.id
        );
        let is_pending = pending.contains(case.id.as_str());
        let failure = match &case.expected {
            Expected::Deferred { ambiguity_id } => {
                assert!(!is_pending, "{}: deferred cases cannot be PENDING", case.id);
                *deferred.entry(ambiguity_id.as_str()).or_default() += 1;
                continue;
            }
            Expected::Exact { output } => {
                let actual = render_case(case, &contexts);
                (actual != *output)
                    .then(|| format!("{}: expected {output:?}, actual {actual:?}", case.id))
            }
            Expected::Invariant { .. } => invariant_failure(case, &contexts),
        };
        let is_exact = matches!(case.expected, Expected::Exact { .. });
        match (failure, is_pending) {
            (None, false) if is_exact => exact_pass += 1,
            (None, false) => invariant_pass += 1,
            (Some(_), true) if is_exact => exact_pending += 1,
            (Some(_), true) => invariant_pending += 1,
            (Some(message), false) => regressions.push(message),
            (None, true) => now_passing.push(case.id.clone()),
        }
    }

    let stale: Vec<&&str> = pending.iter().filter(|id| !ids.contains(**id)).collect();
    assert!(
        stale.is_empty(),
        "PENDING names unknown case IDs: {stale:?}"
    );

    for group in &corpus.stability_groups {
        let outputs: BTreeSet<String> = group
            .case_ids
            .iter()
            .map(|id| {
                let case = corpus.cases.iter().find(|c| &c.id == id);
                render_case(case.expect("stability case exists"), &contexts)
            })
            .collect();
        if outputs.len() != 1 {
            regressions.push(format!(
                "{}: outputs differ across renames: {outputs:?}",
                group.id
            ));
        }
    }

    let deferred_total: usize = deferred.values().sum();
    eprintln!(
        "voice template corpus: {} cases | exact {exact_pass} pass / {exact_pending} pending | \
         invariant {invariant_pass} pass / {invariant_pending} pending | {deferred_total} deferred \
         | {} stability groups",
        corpus.cases.len(),
        corpus.stability_groups.len(),
    );
    eprintln!("deferred by ambiguity: {deferred:?}");

    assert!(
        now_passing.is_empty(),
        "PENDING cases now pass; remove them from PENDING: {now_passing:?}"
    );
    assert!(
        regressions.is_empty(),
        "{} corpus case(s) disagree with the engine:\n{}",
        regressions.len(),
        regressions.join("\n")
    );
    // Corpus totals come from an untyped read, independent of the loop above.
    let raw: serde_json::Value = serde_json::from_str(CORPUS).expect("corpus parses");
    let raw_cases = raw["cases"].as_array().expect("cases array");
    let total_of = |kind: &str| {
        raw_cases
            .iter()
            .filter(|case| case["expected"]["kind"] == kind)
            .count()
    };
    assert_eq!(
        exact_pass + exact_pending,
        total_of("exact"),
        "exact counts reconcile"
    );
    assert_eq!(
        invariant_pass + invariant_pending,
        total_of("invariant"),
        "invariant counts reconcile"
    );
    assert_eq!(
        deferred_total,
        total_of("deferred"),
        "deferred counts reconcile"
    );
    assert_eq!(
        exact_pass + exact_pending + invariant_pass + invariant_pending + deferred_total,
        raw_cases.len(),
        "pass + pending + deferred reconcile with the corpus total"
    );
    assert_eq!(
        exact_pending + invariant_pending,
        PENDING.len(),
        "every PENDING ID counted"
    );
}
