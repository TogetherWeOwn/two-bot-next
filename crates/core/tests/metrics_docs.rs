//! Conformance: every counter row in `docs/metrics.md` renders in the
//! `/metrics` exposition with its allowlisted labels, and every exposition
//! counter has a doc row. Counters added by hand (e.g. the dispatch-drops
//! counter) drift silently without this pin.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use two_bot_core::metrics::{
    Metrics, CHECKPOINT_FAILURE_STAGES, COMMUNITY_FACTS_DRAIN_REASONS, DB_ERROR_OPS,
    DISPATCH_LANES, EVENTS, INTERNAL_ACTION_FAMILIES, INTERNAL_ACTION_OUTCOMES, JOBS, JOB_OUTCOMES,
    PREFIX_TRIGGER_REFUSED_REASONS, REST_ROUTES, RESULTS, SEND_ADMISSION_OUTCOMES, VOICE_DEAD_ACTIONS,
    VOICE_OPERATIONS, VOICE_OUTCOMES, VOICE_RECONCILE_ACTIONS, VOICE_VOTE_KICK_OUTCOMES,
};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One `| Metric | Meaning |` row whose name ends in `_total`.
#[derive(Debug, PartialEq, Eq)]
struct DocCounter {
    name: String,
    labels: Vec<String>,
}

fn parse_doc_counters(doc: &str) -> Vec<DocCounter> {
    let mut rows = Vec::new();
    for line in doc.lines() {
        let line = line.trim();
        if !line.starts_with('|') || !line.contains("`two_bot") || !line.contains("_total") {
            continue;
        }
        let cells: Vec<&str> = line.split('|').collect();
        if cells.len() < 2 {
            continue;
        }
        let raw = cells[1].trim().trim_matches('`').trim().to_owned();
        if raw.is_empty() || raw == "Metric" {
            continue;
        }
        let (name, labels) = match raw.find('{') {
            Some(open) => {
                let close = raw.find('}').unwrap_or(raw.len());
                let labels = raw[open + 1..close]
                    .split(',')
                    .map(|label| label.trim().to_owned())
                    .filter(|label| !label.is_empty())
                    .collect();
                (raw[..open].to_owned(), labels)
            }
            None => (raw, Vec::new()),
        };
        if name.ends_with("_total") {
            rows.push(DocCounter { name, labels });
        }
    }
    rows
}

#[derive(Debug, Default)]
struct ExpoFamily {
    kind: String,
    label_keys: Vec<String>,
    values: BTreeMap<String, BTreeSet<String>>,
}

fn parse_exposition(exposition: &str) -> BTreeMap<String, ExpoFamily> {
    let mut families: BTreeMap<String, ExpoFamily> = BTreeMap::new();
    for line in exposition.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut parts = rest.split_whitespace();
            if let (Some(name), Some(kind)) = (parts.next(), parts.next()) {
                families.entry(name.to_owned()).or_default().kind = kind.to_owned();
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, labels) = match line.find('{') {
            Some(open) => {
                let close = line.find('}').unwrap_or(line.len());
                (&line[..open], &line[open + 1..close])
            }
            None => (line.split_whitespace().next().unwrap_or(""), ""),
        };
        // Histogram rendering adds `_bucket`/`_sum`/`_count` siblings; only the
        // exact family name belongs to the family.
        let Some(family) = families.get_mut(name) else {
            continue;
        };
        if labels.is_empty() {
            continue;
        }
        let mut keys = Vec::new();
        for pair in labels.split(',') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            keys.push(key.to_owned());
            family
                .values
                .entry(key.to_owned())
                .or_default()
                .insert(value.trim_matches('"').to_owned());
        }
        if family.label_keys.is_empty() {
            family.label_keys = keys;
        }
    }
    families
}

/// Expected label keys per counter, from the compile-time allowlists.
fn allowlisted_labels(name: &str) -> Vec<&'static str> {
    match name {
        "two_bot_gateway_events_total" => vec!["event"],
        "two_bot_rest_requests_total" => vec!["route", "result"],
        "two_bot_job_runs_total" => vec!["job", "outcome"],
        "two_bot_voice_operations_total" => vec!["op", "outcome"],
        "two_bot_voice_reconcile_actions_total" => vec!["action"],
        "two_bot_voice_dead_letters_total" => vec!["action"],
        "two_bot_db_errors_total" => vec!["op"],
        "two_bot_send_admissions_total" => vec!["outcome"],
        "two_bot_gateway_prefix_trigger_refused_total" => vec!["reason"],
        "two_bot_dispatch_drops_total" => vec!["lane"],
        "two_bot_voice_vote_kick_total" => vec!["outcome"],
        "two_bot_internal_actions_total" => vec!["family", "outcome"],
        "two_bot_community_facts_drain_failures_total" => vec!["reason"],
        "two_bot_gateway_checkpoint_failures_total" => vec!["stage"],
        _ => Vec::new(),
    }
}

/// Every allowlisted value must render (exposition emits all series at zero).
fn expected_values(name: &str) -> Vec<(&'static str, Vec<&'static str>)> {
    match name {
        "two_bot_gateway_events_total" => vec![("event", EVENTS.to_vec())],
        "two_bot_rest_requests_total" => {
            vec![
                ("route", REST_ROUTES.to_vec()),
                ("result", RESULTS.to_vec()),
            ]
        }
        "two_bot_job_runs_total" => {
            vec![("job", JOBS.to_vec()), ("outcome", JOB_OUTCOMES.to_vec())]
        }
        "two_bot_voice_operations_total" => vec![
            ("op", VOICE_OPERATIONS.to_vec()),
            ("outcome", VOICE_OUTCOMES.to_vec()),
        ],
        "two_bot_voice_reconcile_actions_total" => {
            vec![("action", VOICE_RECONCILE_ACTIONS.to_vec())]
        }
        "two_bot_voice_dead_letters_total" => {
            vec![("action", VOICE_DEAD_ACTIONS.to_vec())]
        }
        "two_bot_db_errors_total" => vec![("op", DB_ERROR_OPS.to_vec())],
        "two_bot_send_admissions_total" => {
            vec![("outcome", SEND_ADMISSION_OUTCOMES.to_vec())]
        }
        "two_bot_gateway_prefix_trigger_refused_total" => {
            vec![("reason", PREFIX_TRIGGER_REFUSED_REASONS.to_vec())]
        }
        "two_bot_dispatch_drops_total" => vec![("lane", DISPATCH_LANES.to_vec())],
        "two_bot_voice_vote_kick_total" => {
            vec![("outcome", VOICE_VOTE_KICK_OUTCOMES.to_vec())]
        }
        "two_bot_internal_actions_total" => vec![
            ("family", INTERNAL_ACTION_FAMILIES.to_vec()),
            ("outcome", INTERNAL_ACTION_OUTCOMES.to_vec()),
        ],
        "two_bot_community_facts_drain_failures_total" => {
            vec![("reason", COMMUNITY_FACTS_DRAIN_REASONS.to_vec())]
        }
        "two_bot_gateway_checkpoint_failures_total" => {
            vec![("stage", CHECKPOINT_FAILURE_STAGES.to_vec())]
        }
        _ => Vec::new(),
    }
}

/// Every counter this test knows. A new exposition counter must join this
/// list (with its labels and values below) as well as the docs, so neither
/// side can grow without the pin noticing.
const KNOWN_COUNTERS: &[&str] = &[
    "two_bot_gateway_events_total",
    "two_bot_gateway_reconnects_total",
    "two_bot_gateway_resumes_total",
    "two_bot_gateway_disconnects_total",
    "two_bot_gateway_missed_events_total",
    "two_bot_rest_requests_total",
    "two_bot_job_runs_total",
    "two_bot_voice_operations_total",
    "two_bot_voice_reconcile_actions_total",
    "two_bot_voice_dead_letters_total",
    "two_bot_voice_vote_kick_total",
    "two_bot_voice_orphans_total",
    "two_bot_db_errors_total",
    "two_bot_send_admissions_total",
    "two_bot_gateway_prefix_trigger_refused_total",
    "two_bot_dispatch_drops_total",
    "two_bot_internal_actions_total",
    "two_bot_community_facts_drain_failures_total",
    "two_bot_gateway_checkpoint_failures_total",
];

fn check_docs_against_exposition(doc: &str, exposition: &str) -> Result<(), String> {
    let doc_counters = parse_doc_counters(doc);
    let families = parse_exposition(exposition);
    let expo_counters: BTreeMap<String, &ExpoFamily> = families
        .iter()
        .filter(|(_, family)| family.kind == "counter")
        .map(|(name, family)| (name.clone(), family))
        .collect();

    for row in &doc_counters {
        let name = row.name.as_str();
        if !KNOWN_COUNTERS.contains(&name) {
            return Err(format!(
                "docs/metrics.md counter {name} is unknown to the conformance pin; extend KNOWN_COUNTERS"
            ));
        }
        let Some(family) = expo_counters.get(name) else {
            return Err(format!(
                "docs/metrics.md counter {name} has no counter family in the exposition"
            ));
        };
        let expected: Vec<String> = allowlisted_labels(name)
            .iter()
            .map(|s| s.to_string())
            .collect();
        if row.labels != expected {
            let doc_labels = row.labels.join(",");
            let want = expected.join(",");
            return Err(format!(
                "docs/metrics.md counter {name} documents labels {doc_labels} but the allowlist is {want}"
            ));
        }
        if family.label_keys != expected {
            let rendered = family.label_keys.join(",");
            let want = expected.join(",");
            return Err(format!(
                "exposition counter {name} renders labels {rendered} but the allowlist is {want}"
            ));
        }
    }
    for name in expo_counters.keys() {
        if !doc_counters
            .iter()
            .any(|row| row.name.as_str() == name.as_str())
        {
            return Err(format!(
                "exposition counter {name} has no row in docs/metrics.md"
            ));
        }
        if !KNOWN_COUNTERS.contains(&name.as_str()) {
            return Err(format!(
                "exposition counter {name} is unknown to the conformance pin; extend KNOWN_COUNTERS"
            ));
        }
    }

    for row in &doc_counters {
        let family = expo_counters[row.name.as_str()];
        for (key, allowed) in expected_values(&row.name) {
            let rendered = family.values.get(key).cloned().unwrap_or_default();
            for value in allowed {
                if !rendered.contains(value) {
                    let name = row.name.as_str();
                    return Err(format!(
                        "exposition counter {name} renders no series for allowlisted {key}=\"{value}\""
                    ));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn committed_metrics_docs_match_exposition() {
    let doc = std::fs::read_to_string(repository_root().join("docs/metrics.md")).unwrap();
    let exposition = Metrics::default().render(None);
    check_docs_against_exposition(&doc, &exposition).unwrap();
}

#[test]
fn dispatch_drops_seed_case_renders_all_lanes() {
    let exposition = Metrics::default().render(None);
    for lane in DISPATCH_LANES {
        assert!(
            exposition.contains(&format!(
                "two_bot_dispatch_drops_total{{lane=\"{lane}\"}} 0\n"
            )),
            "missing lane {lane}"
        );
    }
    let doc = std::fs::read_to_string(repository_root().join("docs/metrics.md")).unwrap();
    let rows = parse_doc_counters(&doc);
    let row = rows
        .iter()
        .find(|row| row.name == "two_bot_dispatch_drops_total")
        .expect("dispatch-drops doc row");
    assert_eq!(row.labels, vec!["lane".to_owned()]);
}

#[test]
fn rejects_deliberate_drift_by_name() {
    let doc = std::fs::read_to_string(repository_root().join("docs/metrics.md")).unwrap();
    let exposition = Metrics::default().render(None);

    // Missing row: the #699 seed counter vanishes from the docs.
    let dropped: String = doc
        .lines()
        .filter(|line| !line.contains("two_bot_dispatch_drops_total{lane}"))
        .collect::<Vec<_>>()
        .join("\n");
    let error = check_docs_against_exposition(&dropped, &exposition).unwrap_err();
    assert!(
        error.contains("two_bot_dispatch_drops_total"),
        "unexpected error: {error}"
    );

    // Wrong labels: the doc row claims a label the allowlist never had.
    let relabeled = doc.replace(
        "two_bot_dispatch_drops_total{lane}",
        "two_bot_dispatch_drops_total{queue}",
    );
    let error = check_docs_against_exposition(&relabeled, &exposition).unwrap_err();
    assert!(
        error.contains("two_bot_dispatch_drops_total"),
        "unexpected error: {error}"
    );

    // Extra row: a documented counter the exposition never renders.
    let extended = format!("{doc}\n| `two_bot_fictional_total` | Never rendered |\n");
    let error = check_docs_against_exposition(&extended, &exposition).unwrap_err();
    assert!(
        error.contains("two_bot_fictional_total"),
        "unexpected error: {error}"
    );

    // Extra exposition family: a rendered counter with no doc row.
    let extended_expo = format!(
        "{exposition}# HELP two_bot_fictional_total Process-local health metric.\n# TYPE two_bot_fictional_total counter\ntwo_bot_fictional_total 0\n"
    );
    let error = check_docs_against_exposition(&doc, &extended_expo).unwrap_err();
    assert!(
        error.contains("two_bot_fictional_total"),
        "unexpected error: {error}"
    );

    // Sanity: the unmodified pair passes.
    check_docs_against_exposition(&doc, &exposition).unwrap();
}
