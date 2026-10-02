//! Runs the independent voice-template corpus (`tests/voice_templates/`)
//! against the naming engine with the full V6 policy, [`TemplateExtensions`].
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
//! Every non-deferred case must pass, including the V6 conditional and
//! styling cases.

#[path = "support/voice_corpus_context.rs"]
mod voice_corpus_context;

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_naming::{parse, render, RoomContext};
use two_bot_core::voice_template::TemplateExtensions;
use voice_corpus_context::{condition_facts, room_context, CorpusContext};

const CORPUS: &str = include_str!("../../../tests/voice_templates/corpus.json");

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

/// One corpus context as the engine sees it: name tokens and V6 condition
/// facts.
struct Room {
    context: RoomContext,
    facts: ConditionFacts,
}

/// One corpus context as the engine sees it: name tokens and V6 condition
/// facts, both derived from the shared mapping so the two agrees.
fn room(context: &CorpusContext) -> Room {
    Room {
        context: room_context(context),
        facts: condition_facts(context),
    }
}

fn render_case(case: &Case, rooms: &BTreeMap<String, Room>) -> String {
    let room = &rooms[&case.context];
    render(
        &parse(&case.input),
        &room.context,
        &TemplateExtensions::new(&room.facts),
    )
}

/// Why an invariant case fails, or `None` when every declared constraint holds.
fn invariant_failure(case: &Case, rooms: &BTreeMap<String, Room>) -> Option<String> {
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
    let output = render_case(case, rooms);
    let mut failures = Vec::new();
    if *nonempty && output.is_empty() {
        failures.push("empty output".to_string());
    }
    if output.chars().count() > *max_characters {
        failures.push(format!("over {max_characters} characters"));
    }
    if *stable_for_same_context && render_case(case, rooms) != output {
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
    let rooms: BTreeMap<String, Room> = corpus
        .contexts
        .iter()
        .map(|(name, context)| (name.clone(), room(context)))
        .collect();

    let mut ids = BTreeSet::new();
    let (mut exact_pass, mut invariant_pass) = (0usize, 0usize);
    let mut deferred: BTreeMap<&str, usize> = BTreeMap::new();
    let mut failures = Vec::new();

    for case in &corpus.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate case ID {}",
            case.id
        );
        assert!(
            rooms.contains_key(&case.context),
            "{}: missing context",
            case.id
        );
        let failure = match &case.expected {
            Expected::Deferred { ambiguity_id } => {
                *deferred.entry(ambiguity_id.as_str()).or_default() += 1;
                continue;
            }
            Expected::Exact { output } => {
                let actual = render_case(case, &rooms);
                (actual != *output)
                    .then(|| format!("{}: expected {output:?}, actual {actual:?}", case.id))
            }
            Expected::Invariant { .. } => invariant_failure(case, &rooms),
        };
        match failure {
            Some(message) => failures.push(message),
            None if matches!(case.expected, Expected::Exact { .. }) => exact_pass += 1,
            None => invariant_pass += 1,
        }
    }

    for group in &corpus.stability_groups {
        let outputs: BTreeSet<String> = group
            .case_ids
            .iter()
            .map(|id| {
                let case = corpus.cases.iter().find(|c| &c.id == id);
                render_case(case.expect("stability case exists"), &rooms)
            })
            .collect();
        if outputs.len() != 1 {
            failures.push(format!(
                "{}: outputs differ across renames: {outputs:?}",
                group.id
            ));
        }
    }

    let deferred_total: usize = deferred.values().sum();
    eprintln!(
        "voice template corpus: {} cases | exact {exact_pass} pass | invariant {invariant_pass} \
         pass | {deferred_total} deferred | {} stability groups",
        corpus.cases.len(),
        corpus.stability_groups.len(),
    );
    eprintln!("deferred by ambiguity: {deferred:?}");

    assert!(
        failures.is_empty(),
        "{} corpus case(s) disagree with the engine:\n{}",
        failures.len(),
        failures.join("\n")
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
    assert_eq!(exact_pass, total_of("exact"), "exact counts reconcile");
    assert_eq!(
        invariant_pass,
        total_of("invariant"),
        "invariant counts reconcile"
    );
    assert_eq!(
        deferred_total,
        total_of("deferred"),
        "deferred counts reconcile"
    );
    assert_eq!(
        exact_pass + invariant_pass + deferred_total,
        raw_cases.len(),
        "pass + deferred reconcile with the corpus total"
    );
}
