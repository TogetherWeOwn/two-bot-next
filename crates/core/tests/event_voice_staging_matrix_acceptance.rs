//! Acceptance: event-voice staging E2E harness + chaos matrix.
//!
//! Card AC (SPEC section 6/8, host decision D15):
//! 1. Matrix defined: T-16 event -> channel appears T-15..T-14 with jump link
//!    on staging web; Discord rename -> staging web updates <=2 min;
//!    complete-while-occupied -> after-party rename + longest-present owner +
//!    controls work; complete-while-empty -> deleted <=2 min; cancel pre-start
//!    -> deleted; restart mid-event -> no duplicate, ownership resolves from PG.
//! 2. Chaos cases defined: restart mid-handoff, injected 429s, category
//!    permission revoked mid-tick, two events same slug same T-15, DST-week
//!    recurrence.
//! 3. Prod fence: hidden test channel only, passive monitors on prod, no
//!    staging-to-prod promotion without review; rollback rehearsed.
//!
//! Builder contract (the builder builds on this branch and never edits this
//! file). Land these artifacts:
//!
//! - `docs/event-voice-staging-matrix.json`: the machine-readable matrix.
//!   Top-level shape:
//!   ```json
//!   {
//!     "version": 1,
//!     "environment": "staging",
//!     "harness": {"entrypoint": "scripts/<runner>", "description": "..."},
//!     "matrix": [
//!       {"id": "create-t15-window", "title": "...", "setup": ["..."],
//!        "assert": ["..."], "timeout_s": 300}
//!     ],
//!     "chaos": [
//!       {"id": "chaos-injected-429s", "title": "...", "fault": ["..."],
//!        "setup": ["..."], "assert": ["..."], "timeout_s": 600}
//!     ],
//!     "prod_fence": {
//!       "guards": ["..."],
//!       "rollback_steps": ["..."],
//!       "rehearsal": "how rollback is rehearsed and where the record lives"
//!     }
//!   }
//!   ```
//!   Row ids must match this file's `CORE_IDS` / `CHAOS_IDS` exactly; extra
//!   rows are allowed. Every row needs a non-empty title, non-empty setup and
//!   assert step lists, and a positive `timeout_s`; chaos rows additionally
//!   need a non-empty `fault` list naming the injected fault. The entrypoint
//!   is a repo-relative path to the harness runner and must exist on disk.
//! - `docs/event-voice-staging-e2e.md`: the human runbook. It must name every
//!   matrix and chaos row id, state the prod fence, and document the rollback
//!   rehearsal.
//!
//!   Term pins (lowercased substring match on each row's setup + assert +
//!   fault text; `[a | b]` means either term passes):
//!   - create-t15-window: `t-15`, `t-14`, `jump link`, `staging`
//!   - rename-propagates: `renam`, `staging`; `timeout_s <= 120`
//!   - complete-while-occupied: `after-party`, `owner`, `control`
//!   - complete-while-empty: `delet`; `timeout_s <= 120`
//!   - cancel-pre-start: `cancel`, `delet`
//!   - restart-mid-event: `duplicat`, [`pg` | `postgres` | `presence`]
//!   - chaos-restart-mid-handoff: `restart`, [`handoff` | `after-party` | `owner`]
//!   - chaos-injected-429s: fault names `429`
//!   - chaos-category-permission-revoked: fault names a revocation
//!     ([`revok` | `deny` | `403`]) on the category ([`categor` | `perm` | `403`])
//!   - chaos-same-slug-same-t15: `slug`, `t-15`
//!   - chaos-dst-week-recurrence: `dst`, [`recur` | `week`]
//!   - prod fence (manifest guards + runbook): hidden test channel, passive
//!     monitors, prod scope (`prod` word or `production`), promotion, human
//!     gate (any `review*` form except `preview`, or `approv*`), takeover,
//!     staging, rollback deleting the test channels, and a rehearsal note.
//!
//! RED: every test fails until those artifacts land. No guild, no network, no
//! database; these pin the harness definition, not a live staging run.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The six SPEC section-6 staging rows, in card order. Extra rows are
/// allowed; dropping or renaming one of these fails the suite.
const CORE_IDS: [&str; 6] = [
    "create-t15-window",
    "rename-propagates",
    "complete-while-occupied",
    "complete-while-empty",
    "cancel-pre-start",
    "restart-mid-event",
];

/// The five D15 chaos cases, in card order.
const CHAOS_IDS: [&str; 5] = [
    "chaos-restart-mid-handoff",
    "chaos-injected-429s",
    "chaos-category-permission-revoked",
    "chaos-same-slug-same-t15",
    "chaos-dst-week-recurrence",
];

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_manifest() -> Value {
    let path = repository_root().join("docs/event-voice-staging-matrix.json");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "builder: create docs/event-voice-staging-matrix.json (see the contract \
             at the top of crates/core/tests/event_voice_staging_matrix_acceptance.rs)"
        )
    });
    serde_json::from_str(&raw).expect("staging matrix manifest must be valid JSON")
}

fn read_runbook() -> String {
    let path = repository_root().join("docs/event-voice-staging-e2e.md");
    std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "builder: create docs/event-voice-staging-e2e.md naming every matrix \
             and chaos row id, the prod fence, and the rollback rehearsal"
        )
    })
}

fn rows<'a>(manifest: &'a Value, key: &str) -> &'a [Value] {
    manifest
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("manifest must carry a {key} array"))
        .as_slice()
}

fn row_by_id<'a>(rows: &'a [Value], id: &str, key: &str) -> &'a Value {
    rows.iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("manifest {key} is missing required row {id}"))
}

/// Lowercased concatenation of the row's setup + assert (+ fault) step text.
fn row_text(row: &Value) -> String {
    let mut text = String::new();
    for key in ["setup", "assert", "fault"] {
        if let Some(steps) = row.get(key).and_then(Value::as_array) {
            for step in steps {
                if let Some(line) = step.as_str() {
                    text.push_str(&line.to_lowercase());
                    text.push('\n');
                }
            }
        }
    }
    text
}

fn fault_text(row: &Value, id: &str) -> String {
    row.get("fault")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("chaos row {id} must declare a fault array"))
        .iter()
        .map(|step| {
            step.as_str()
                .unwrap_or_else(|| panic!("chaos row {id} fault entries are strings"))
                .to_lowercase()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Word match so "review" does not pass on "preview".
fn contains_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|token| token == word)
}

/// Prod scope, whether written "prod" or "production" ("reproduce" passes
/// neither).
fn mentions_prod(haystack: &str) -> bool {
    contains_word(haystack, "prod") || haystack.contains("production")
}

/// Human gate: any `review*` form except `preview`, or any `approv*` form.
fn mentions_review(haystack: &str) -> bool {
    if haystack.contains("approv") {
        return true;
    }
    haystack
        .match_indices("review")
        .any(|(at, _)| !haystack[..at].ends_with('p'))
}

fn require_terms(id: &str, text: &str, groups: &[&[&str]]) {
    for group in groups {
        assert!(
            group.iter().any(|term| text.contains(term)),
            "row {id} must pin {group:?} in its setup/assert text"
        );
    }
}

#[test]
fn staging_matrix_manifest_exists_and_declares_staging() {
    let manifest = read_manifest();
    assert_eq!(
        manifest.get("version").and_then(Value::as_u64),
        Some(1),
        "manifest version must be 1"
    );
    assert_eq!(
        manifest.get("environment").and_then(Value::as_str),
        Some("staging"),
        "manifest environment must be exactly \"staging\": this harness never runs against prod"
    );
}

#[test]
fn staging_matrix_covers_the_six_card_rows() {
    let manifest = read_manifest();
    let matrix = rows(&manifest, "matrix");
    let mut missing = Vec::new();
    for id in CORE_IDS {
        if !matrix
            .iter()
            .any(|row| row.get("id").and_then(Value::as_str) == Some(id))
        {
            missing.push(id);
        }
    }
    assert!(
        missing.is_empty(),
        "manifest matrix drops card rows: {missing:?}"
    );
}

#[test]
fn staging_matrix_rows_are_executable_definitions() {
    let manifest = read_manifest();
    for id in CORE_IDS {
        let row = row_by_id(rows(&manifest, "matrix"), id, "matrix");
        let title = row.get("title").and_then(Value::as_str).unwrap_or("");
        assert!(!title.is_empty(), "row {id} needs a non-empty title");
        for key in ["setup", "assert"] {
            let steps = row
                .get(key)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("row {id} must carry a {key} step list"));
            assert!(
                !steps.is_empty(),
                "row {id} {key} must list at least one step"
            );
            for step in steps {
                let line = step
                    .as_str()
                    .unwrap_or_else(|| panic!("row {id} {key} entries are strings"));
                assert!(!line.is_empty(), "row {id} {key} has an empty step");
            }
        }
        let timeout = row.get("timeout_s").and_then(Value::as_u64).unwrap_or(0);
        assert!(
            timeout > 0,
            "row {id} needs a positive timeout_s in seconds"
        );
    }
    // Ids stay unique so run records can cite rows unambiguously.
    let mut seen = std::collections::BTreeSet::new();
    for row in rows(&manifest, "matrix") {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .expect("matrix rows carry string ids");
        assert!(seen.insert(id), "duplicate matrix row id {id}");
    }
}

#[test]
fn staging_matrix_rows_pin_the_card_observables() {
    let manifest = read_manifest();
    let matrix = rows(&manifest, "matrix");
    // Each group is alternatives; at least one term per group must appear.
    // Terms come from the card text verbatim.
    let pins: [(&str, &[&[&str]]); 6] = [
        (
            "create-t15-window",
            &[&["t-15"], &["t-14"], &["jump link"], &["staging"]],
        ),
        ("rename-propagates", &[&["renam"], &["staging"]]),
        (
            "complete-while-occupied",
            &[&["after-party"], &["owner"], &["control"]],
        ),
        ("complete-while-empty", &[&["delet"]]),
        ("cancel-pre-start", &[&["cancel"], &["delet"]]),
        (
            "restart-mid-event",
            &[&["duplicat"], &["pg", "postgres", "presence"]],
        ),
    ];
    for (id, groups) in pins {
        require_terms(id, &row_text(row_by_id(matrix, id, "matrix")), groups);
    }
}

#[test]
fn staging_matrix_timeouts_honor_the_two_minute_bounds() {
    let manifest = read_manifest();
    let matrix = rows(&manifest, "matrix");
    for id in ["rename-propagates", "complete-while-empty"] {
        let timeout = row_by_id(matrix, id, "matrix")
            .get("timeout_s")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        assert!(
            timeout > 0 && timeout <= 120,
            "row {id} must assert within the card's <=2 min bound, got timeout_s={timeout}"
        );
    }
}

#[test]
fn staging_chaos_covers_the_five_card_cases() {
    let manifest = read_manifest();
    let chaos = rows(&manifest, "chaos");
    let mut missing = Vec::new();
    for id in CHAOS_IDS {
        if !chaos
            .iter()
            .any(|row| row.get("id").and_then(Value::as_str) == Some(id))
        {
            missing.push(id);
        }
    }
    assert!(
        missing.is_empty(),
        "manifest chaos drops card cases: {missing:?}"
    );
    let mut seen = std::collections::BTreeSet::new();
    for row in chaos {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .expect("chaos rows carry string ids");
        assert!(seen.insert(id), "duplicate chaos row id {id}");
    }
}

#[test]
fn staging_chaos_rows_declare_fault_and_observable() {
    let manifest = read_manifest();
    let chaos = rows(&manifest, "chaos");
    for id in CHAOS_IDS {
        let row = row_by_id(chaos, id, "chaos");
        for key in ["setup", "assert", "fault"] {
            let steps = row
                .get(key)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("chaos row {id} must carry a {key} step list"));
            assert!(
                !steps.is_empty(),
                "chaos row {id} {key} must list at least one step"
            );
        }
        let timeout = row.get("timeout_s").and_then(Value::as_u64).unwrap_or(0);
        assert!(
            timeout > 0,
            "chaos row {id} needs a positive timeout_s in seconds"
        );
    }
    require_terms(
        "chaos-restart-mid-handoff",
        &row_text(row_by_id(chaos, "chaos-restart-mid-handoff", "chaos")),
        &[&["restart"], &["handoff", "after-party", "owner"]],
    );
    let injected = fault_text(
        row_by_id(chaos, "chaos-injected-429s", "chaos"),
        "chaos-injected-429s",
    );
    assert!(
        injected.contains("429"),
        "chaos-injected-429s fault must name the injected 429s"
    );
    let revoked = fault_text(
        row_by_id(chaos, "chaos-category-permission-revoked", "chaos"),
        "chaos-category-permission-revoked",
    );
    let revokes = ["revok", "deny", "403"]
        .iter()
        .any(|term| revoked.contains(term));
    let category_scope = ["categor", "perm", "403"]
        .iter()
        .any(|term| revoked.contains(term));
    assert!(
        revokes && category_scope,
        "chaos-category-permission-revoked fault must name a revocation on the category permission"
    );
    require_terms(
        "chaos-same-slug-same-t15",
        &row_text(row_by_id(chaos, "chaos-same-slug-same-t15", "chaos")),
        &[&["slug"], &["t-15"]],
    );
    require_terms(
        "chaos-dst-week-recurrence",
        &row_text(row_by_id(chaos, "chaos-dst-week-recurrence", "chaos")),
        &[&["dst"], &["recur", "week"]],
    );
}

#[test]
fn staging_harness_entrypoint_exists() {
    let manifest = read_manifest();
    let entrypoint = manifest
        .get("harness")
        .and_then(|harness| harness.get("entrypoint"))
        .and_then(Value::as_str)
        .expect("manifest harness.entrypoint names the runner");
    assert!(!entrypoint.is_empty(), "harness.entrypoint must be set");
    let relative = Path::new(entrypoint);
    assert!(
        relative.is_relative(),
        "harness.entrypoint must be a repo-relative path, got {entrypoint}"
    );
    assert!(
        repository_root().join(relative).is_file(),
        "harness.entrypoint {entrypoint} must exist on disk"
    );
}

#[test]
fn staging_manifest_declares_the_prod_fence() {
    let manifest = read_manifest();
    let fence = manifest
        .get("prod_fence")
        .expect("manifest must declare a prod_fence block");
    let guards = fence
        .get("guards")
        .and_then(Value::as_array)
        .expect("prod_fence.guards lists the fence rules");
    assert!(
        !guards.is_empty(),
        "prod_fence.guards must list at least one rule"
    );
    let guard_text = guards
        .iter()
        .map(|guard| {
            guard
                .as_str()
                .expect("prod_fence.guards entries are strings")
                .to_lowercase()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for word in ["hidden", "channel"] {
        assert!(
            contains_word(&guard_text, word),
            "prod_fence.guards must pin {word:?} (hidden test channel only)"
        );
    }
    assert!(
        mentions_prod(&guard_text),
        "prod_fence.guards must scope the passive monitors to prod"
    );
    assert!(
        mentions_review(&guard_text),
        "prod_fence.guards must gate promotion on review/approval"
    );
    for phrase in ["passive monitors", "promotion", "takeover", "staging"] {
        assert!(
            guard_text.contains(phrase),
            "prod_fence.guards must pin {phrase:?}"
        );
    }
    let rollback = fence
        .get("rollback_steps")
        .and_then(Value::as_array)
        .expect("prod_fence.rollback_steps lists the rollback");
    assert!(
        !rollback.is_empty(),
        "prod_fence.rollback_steps must list at least one step"
    );
    let rollback_text = rollback
        .iter()
        .map(|step| {
            step.as_str()
                .expect("prod_fence.rollback_steps entries are strings")
                .to_lowercase()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rollback_text.contains("delete") && rollback_text.contains("channel"),
        "prod_fence.rollback_steps must cover deleting the test channels"
    );
    let rehearsal = fence.get("rehearsal").and_then(Value::as_str).unwrap_or("");
    assert!(
        !rehearsal.is_empty() && rehearsal.to_lowercase().contains("rehears"),
        "prod_fence.rehearsal must say how rollback is rehearsed and recorded"
    );
}

#[test]
fn staging_runbook_covers_matrix_fence_and_rehearsal() {
    let runbook = read_runbook();
    let lowered = runbook.to_lowercase();
    let mut missing = Vec::new();
    for id in CORE_IDS.into_iter().chain(CHAOS_IDS) {
        if !runbook.contains(id) {
            missing.push(id);
        }
    }
    assert!(missing.is_empty(), "runbook drops matrix rows: {missing:?}");
    for phrase in [
        "hidden test channel",
        "passive monitors",
        "promotion",
        "takeover",
        "rollback",
        "rehears",
        "staging",
    ] {
        assert!(
            lowered.contains(phrase),
            "runbook must state the prod fence ({phrase:?})"
        );
    }
    assert!(
        mentions_review(&lowered),
        "runbook must gate promotion on review/approval"
    );
    assert!(
        mentions_prod(&lowered),
        "runbook must scope the passive monitors to prod"
    );
}
