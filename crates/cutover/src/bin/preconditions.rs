//! Read-only cutover preconditions checker (TOG-11807; supports B4 TOG-9699).
//!
//! `docs/cutover.md` preconditions are manual prose; this binary verifies the
//! static evidence they point at before the lead's GO, so a missing receipt is
//! found mechanically instead of by reading. Filesystem reads only: no DB, no
//! network, no gateway, no writes. Exit 0 with a JSON report when every input
//! is present, exit 1 naming the missing item otherwise, exit 2 on usage.
//!
//! Usage: preconditions [--repo-root <path>]
//!   The report is JSON on stdout; missing-item lines go to stderr. URLs and
//!   secrets never appear: only relative paths and section names are named.

use std::path::{Path, PathBuf};

const USAGE: &str = "Usage: preconditions [--repo-root <path>]\n\
    Static cutover precondition check: parity marker, runbook docs/sections,\n\
    timer units, registry fixture. Reports JSON to stdout; reads nothing\n\
    outside <repo-root> and touches no DB or network.\n\
    Exit codes: 0 all present, 1 missing item, 2 usage.";

const PARITY_DOC: &str = "docs/parity.md";
const PARITY_MARKER: &str = "Unmapped: **0**";

const CUTOVER_DOC: &str = "docs/cutover.md";

const REQUIRED_DOCS: &[&str] = &[
    "docs/cutover.md",
    "docs/parity.md",
    "docs/staging-soak.md",
    "docs/backup.md",
    "docs/preflight.md",
    "docs/commands.md",
    "docs/runbook.md",
];

const REQUIRED_SECTIONS: &[&str] = &[
    "## Roles, safety and evidence",
    "## Preconditions: all must pass",
    "## T-minus checklist",
    "## Freeze and drain (T_f)",
    "## Tool availability and command sheet",
    "## Data copy and verification",
    "## Registry swap, first boot and go/no-go",
    "## 48-hour watch",
    "## Rollback: preserve Next-window writes before reopening legacy",
    "## Communication template",
];

const REQUIRED_TIMERS: &[&str] = &[
    "deploy/two-bot-next-backup.timer",
    "deploy/two-bot-next-guild-config-backup.timer",
    "deploy/two-bot-next-restore-drill.timer",
];

const REGISTRY_FIXTURE: &str = "crates/core/tests/fixtures/legacy_registry.json";

struct Outcome {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let root = parse_args(&argv);
    let (outcomes, missing) = check(&root);
    let ok = missing.is_empty();
    let report = serde_json::json!({
        "version": 1,
        "repo_root": root.to_string_lossy(),
        "ok": ok,
        "checks": outcomes
            .iter()
            .map(|o| serde_json::json!({"name": o.name, "ok": o.ok, "detail": o.detail}))
            .collect::<Vec<_>>(),
        "missing": missing,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{\"ok\":false}".to_owned())
    );
    if !ok {
        for item in &missing {
            eprintln!("missing: {item}");
        }
        std::process::exit(1);
    }
}

fn parse_args(argv: &[String]) -> PathBuf {
    if argv.len() == 1 && argv[0] == "--help" {
        println!("{USAGE}");
        std::process::exit(0);
    }
    // Strict `--repo-root <path>`: exactly one occurrence, nothing else.
    if argv.len() == 2 && argv[0] == "--repo-root" {
        return PathBuf::from(&argv[1]);
    }
    if argv.is_empty() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    usage_error();
}

fn usage_error() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn check(root: &Path) -> (Vec<Outcome>, Vec<String>) {
    let mut outcomes = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    // --- 1. parity matrix zero-unmapped marker --------------------------------
    match std::fs::read_to_string(root.join(PARITY_DOC)) {
        Ok(text) if text.contains(PARITY_MARKER) => outcomes.push(Outcome {
            name: "parity-zero-unmapped",
            ok: true,
            detail: format!("{PARITY_DOC} carries the zero-unmapped marker"),
        }),
        Ok(_) => {
            outcomes.push(Outcome {
                name: "parity-zero-unmapped",
                ok: false,
                detail: format!("{PARITY_DOC} lacks the `{PARITY_MARKER}` marker"),
            });
            missing.push(format!("{PARITY_DOC} section `{PARITY_MARKER}`"));
        }
        Err(_) => {
            outcomes.push(Outcome {
                name: "parity-zero-unmapped",
                ok: false,
                detail: format!("{PARITY_DOC} is absent"),
            });
            missing.push(PARITY_DOC.to_owned());
        }
    }

    // --- 2. required runbook docs -------------------------------------------
    let mut absent_docs = Vec::new();
    for &doc in REQUIRED_DOCS {
        if !root.join(doc).is_file() {
            absent_docs.push(doc.to_owned());
        }
    }
    if absent_docs.is_empty() {
        outcomes.push(Outcome {
            name: "runbook-docs",
            ok: true,
            detail: format!(
                "{}/{} required docs present",
                REQUIRED_DOCS.len(),
                REQUIRED_DOCS.len()
            ),
        });
    } else {
        outcomes.push(Outcome {
            name: "runbook-docs",
            ok: false,
            detail: format!("absent: {}", absent_docs.join(", ")),
        });
        missing.extend(absent_docs);
    }

    // --- 3. required docs/cutover.md sections --------------------------------
    match std::fs::read_to_string(root.join(CUTOVER_DOC)) {
        Ok(text) => {
            let absent: Vec<&str> = REQUIRED_SECTIONS
                .iter()
                .filter(|s| !text.contains(*s))
                .copied()
                .collect();
            if absent.is_empty() {
                outcomes.push(Outcome {
                    name: "runbook-sections",
                    ok: true,
                    detail: format!(
                        "{}/{} required {CUTOVER_DOC} sections present",
                        REQUIRED_SECTIONS.len(),
                        REQUIRED_SECTIONS.len()
                    ),
                });
            } else {
                outcomes.push(Outcome {
                    name: "runbook-sections",
                    ok: false,
                    detail: format!("absent sections: {}", absent.join(" | ")),
                });
                missing.extend(
                    absent
                        .iter()
                        .map(|s| format!("{CUTOVER_DOC} section `{s}`")),
                );
            }
        }
        Err(_) => {
            outcomes.push(Outcome {
                name: "runbook-sections",
                ok: false,
                detail: format!("{CUTOVER_DOC} is absent"),
            });
            if !missing.iter().any(|m| m == CUTOVER_DOC) {
                missing.push(CUTOVER_DOC.to_owned());
            }
        }
    }

    // --- 4. timer units -------------------------------------------------------
    let mut bad_timers = Vec::new();
    for &unit in REQUIRED_TIMERS {
        match std::fs::read_to_string(root.join(unit)) {
            Ok(text) if text.contains("OnCalendar=") => {}
            Ok(_) => bad_timers.push(format!("{unit} (no OnCalendar= schedule)")),
            Err(_) => bad_timers.push(unit.to_owned()),
        }
    }
    if bad_timers.is_empty() {
        outcomes.push(Outcome {
            name: "timer-units",
            ok: true,
            detail: format!(
                "{}/{} timer units present with OnCalendar=",
                REQUIRED_TIMERS.len(),
                REQUIRED_TIMERS.len()
            ),
        });
    } else {
        outcomes.push(Outcome {
            name: "timer-units",
            ok: false,
            detail: format!("bad units: {}", bad_timers.join(", ")),
        });
        missing.extend(bad_timers);
    }

    // --- 5. registry fixture ---------------------------------------------------
    match std::fs::read_to_string(root.join(REGISTRY_FIXTURE)) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value)
                if value
                    .get("commands")
                    .and_then(|c| c.as_array())
                    .is_some_and(|c| !c.is_empty()) =>
            {
                let count = value
                    .get("commands")
                    .and_then(|c| c.as_array())
                    .map_or(0, |c| c.len());
                outcomes.push(Outcome {
                    name: "registry-fixture",
                    ok: true,
                    detail: format!("{REGISTRY_FIXTURE} parses with {count} registry commands"),
                });
            }
            Ok(_) => {
                outcomes.push(Outcome {
                    name: "registry-fixture",
                    ok: false,
                    detail: format!("{REGISTRY_FIXTURE} has no non-empty commands array"),
                });
                missing.push(format!("{REGISTRY_FIXTURE} (no non-empty commands array)"));
            }
            Err(e) => {
                outcomes.push(Outcome {
                    name: "registry-fixture",
                    ok: false,
                    detail: format!("{REGISTRY_FIXTURE} is not valid JSON"),
                });
                missing.push(format!("{REGISTRY_FIXTURE} (invalid JSON: {e})"));
            }
        },
        Err(_) => {
            outcomes.push(Outcome {
                name: "registry-fixture",
                ok: false,
                detail: format!("{REGISTRY_FIXTURE} is absent"),
            });
            missing.push(REGISTRY_FIXTURE.to_owned());
        }
    }

    (outcomes, missing)
}
