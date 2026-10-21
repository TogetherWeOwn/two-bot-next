//! Read-only cutover preconditions checker (TOG-11807; supports B4 TOG-9699).
//!
//! `docs/cutover.md` preconditions are manual prose; this binary verifies the
//! static evidence they point at before the lead's GO, so a missing receipt is
//! found mechanically instead of by reading. Filesystem reads only: no DB, no
//! network, no gateway, no writes. Exit 0 with a JSON report when every input
//! is present, exit 1 naming the missing item otherwise, exit 2 on usage.
//!
//! Usage: preconditions [--repo-root `<path>`]
//!   The report is JSON on stdout; missing-item lines go to stderr. URLs and
//!   secrets never appear: only relative paths and section names are named.

// Operator CLI reports intentionally use stdout; runtime/library modules do not.
#![allow(clippy::print_stdout)]

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

/// Cap for any single input file buffered into memory: 8 MiB, matching the
/// capped cutover siblings (`ghost_cleanup`, `raid-remove`). Reads go through
/// `cli::read_text_capped`, which detects an over-cap file without ever
/// buffering past the cap; the boundary is strict `>`.
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
enum InputError {
    OverCap,
    Unreadable,
}

/// Bounded read of one checker input. Over-cap files report `OverCap` so the
/// caller can refuse them as a failed check naming the role and the cap;
/// open/read failures stay opaque (`Unreadable`) like the old
/// `read_to_string` path, including directories, which open but never read.
fn read_input(path: &Path) -> Result<String, InputError> {
    let file = std::fs::File::open(path).map_err(|_| InputError::Unreadable)?;
    two_bot_cutover::cli::read_text_capped(file, MAX_INPUT_BYTES)
        .map_err(|_| InputError::Unreadable)?
        .ok_or(InputError::OverCap)
}

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
    match read_input(&root.join(PARITY_DOC)) {
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
        Err(InputError::OverCap) => {
            outcomes.push(Outcome {
                name: "parity-zero-unmapped",
                ok: false,
                detail: format!("{PARITY_DOC} exceeds 8 MiB"),
            });
            missing.push(format!("{PARITY_DOC} exceeds 8 MiB"));
        }
        Err(InputError::Unreadable) => {
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
    match read_input(&root.join(CUTOVER_DOC)) {
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
        Err(InputError::OverCap) => {
            outcomes.push(Outcome {
                name: "runbook-sections",
                ok: false,
                detail: format!("{CUTOVER_DOC} exceeds 8 MiB"),
            });
            missing.push(format!("{CUTOVER_DOC} exceeds 8 MiB"));
        }
        Err(InputError::Unreadable) => {
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
        match read_input(&root.join(unit)) {
            Ok(text) if text.contains("OnCalendar=") => {}
            Ok(_) => bad_timers.push(format!("{unit} (no OnCalendar= schedule)")),
            Err(InputError::OverCap) => bad_timers.push(format!("{unit} exceeds 8 MiB")),
            Err(InputError::Unreadable) => bad_timers.push(unit.to_owned()),
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
    match read_input(&root.join(REGISTRY_FIXTURE)) {
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
        Err(InputError::OverCap) => {
            outcomes.push(Outcome {
                name: "registry-fixture",
                ok: false,
                detail: format!("{REGISTRY_FIXTURE} exceeds 8 MiB"),
            });
            missing.push(format!("{REGISTRY_FIXTURE} exceeds 8 MiB"));
        }
        Err(InputError::Unreadable) => {
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

#[cfg(test)]
mod tests {
    //! In-process coverage for the 8 MiB read cap. The protected
    //! `preconditions_read_cap_acceptance` suite pins the same contract by
    //! spawning this binary with a cleared environment, which drops the
    //! coverage profile; these unit tests execute the same paths in-process
    //! so the changed-lines gate can measure them.
    use super::*;
    use std::fs;

    /// Pinned cap, kept as a literal so the contract does not move with the
    /// implementation.
    const CAP: usize = 8 * 1024 * 1024;

    fn scratch_dir(name: &str) -> PathBuf {
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!("preconditions-unit-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Pad `head` with filler to exactly `total` bytes.
    fn sized(head: &str, total: usize) -> String {
        let mut out = String::with_capacity(total);
        out.push_str(head);
        out.push_str(&"x".repeat(total - head.len()));
        out
    }

    /// A repo-root tree every check accepts.
    fn valid_tree(name: &str) -> PathBuf {
        let dir = scratch_dir(name);
        for &doc in REQUIRED_DOCS {
            write(&dir, doc, "present\n");
        }
        write(&dir, PARITY_DOC, &format!("{PARITY_MARKER}\n"));
        write(&dir, CUTOVER_DOC, &REQUIRED_SECTIONS.join("\n"));
        for &unit in REQUIRED_TIMERS {
            write(&dir, unit, "[Timer]\nOnCalendar=daily\n");
        }
        write(&dir, REGISTRY_FIXTURE, r#"{"commands": [{"id": "cmd"}]}"#);
        dir
    }

    fn assert_refused(missing: &[String], role: &str) {
        let entry = format!("{role} exceeds 8 MiB");
        assert!(
            missing.contains(&entry),
            "missing must refuse {role} naming the cap, got: {missing:?}"
        );
    }

    #[test]
    fn read_input_accepts_small_and_at_cap_files() {
        let dir = scratch_dir("read-ok");
        let small = dir.join("small.txt");
        fs::write(&small, "hello\n").unwrap();
        assert_eq!(read_input(&small).unwrap(), "hello\n");
        let at_cap = dir.join("at-cap.txt");
        fs::write(&at_cap, sized("hello\n", CAP)).unwrap();
        assert_eq!(read_input(&at_cap).unwrap().len(), CAP);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_input_refuses_over_cap() {
        let dir = scratch_dir("read-over");
        let path = dir.join("over.txt");
        fs::write(&path, sized("hello\n", CAP + 1)).unwrap();
        assert!(matches!(read_input(&path), Err(InputError::OverCap)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_input_maps_a_missing_file_to_unreadable() {
        let dir = scratch_dir("read-missing");
        assert!(matches!(
            read_input(&dir.join("absent.txt")),
            Err(InputError::Unreadable)
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_input_maps_a_directory_to_unreadable() {
        // Directories open but never read: the open succeeds and the bounded
        // read fails, so the input stays opaque like the old path.
        let dir = scratch_dir("read-dir");
        assert!(matches!(read_input(&dir), Err(InputError::Unreadable)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_accepts_a_valid_tree() {
        let dir = valid_tree("check-valid");
        let (outcomes, missing) = check(&dir);
        assert!(missing.is_empty(), "unexpected missing: {missing:?}");
        assert!(outcomes.iter().all(|o| o.ok));
        assert_eq!(outcomes.len(), 5);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_refuses_an_over_cap_parity_doc() {
        let dir = valid_tree("check-parity-over");
        write(
            &dir,
            PARITY_DOC,
            &sized(&format!("{PARITY_MARKER}\n"), CAP + 1),
        );
        let (outcomes, missing) = check(&dir);
        assert_refused(&missing, PARITY_DOC);
        assert!(outcomes
            .iter()
            .any(|o| o.name == "parity-zero-unmapped" && !o.ok));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_refuses_an_over_cap_cutover_doc() {
        let dir = valid_tree("check-cutover-over");
        write(
            &dir,
            CUTOVER_DOC,
            &sized(&REQUIRED_SECTIONS.join("\n"), CAP + 1),
        );
        let (outcomes, missing) = check(&dir);
        assert_refused(&missing, CUTOVER_DOC);
        assert!(outcomes
            .iter()
            .any(|o| o.name == "runbook-sections" && !o.ok));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_refuses_an_over_cap_timer_unit() {
        let dir = valid_tree("check-timer-over");
        write(
            &dir,
            REQUIRED_TIMERS[0],
            &sized("[Timer]\nOnCalendar=daily\n", CAP + 1),
        );
        let (_, missing) = check(&dir);
        assert_refused(&missing, REQUIRED_TIMERS[0]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_refuses_an_over_cap_registry_fixture() {
        let dir = valid_tree("check-registry-over");
        // Valid JSON, so only the cap can refuse it (and the refusal must
        // name the cap, not JSON).
        let prefix = r#"{"commands": [{"id": "cmd", "pad": ""#;
        let suffix = r#""}]}"#;
        let content = format!(
            "{prefix}{}{suffix}",
            "x".repeat(CAP + 1 - prefix.len() - suffix.len())
        );
        assert_eq!(content.len(), CAP + 1);
        serde_json::from_str::<serde_json::Value>(&content).unwrap();
        write(&dir, REGISTRY_FIXTURE, &content);
        let (outcomes, missing) = check(&dir);
        assert_refused(&missing, REGISTRY_FIXTURE);
        assert!(outcomes
            .iter()
            .any(|o| o.name == "registry-fixture" && !o.ok));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_maps_absent_inputs_to_unreadable() {
        let dir = scratch_dir("check-absent");
        let (outcomes, missing) = check(&dir);
        assert!(missing.iter().any(|m| m == PARITY_DOC));
        assert!(missing.iter().any(|m| m == CUTOVER_DOC));
        for &unit in REQUIRED_TIMERS {
            assert!(missing.iter().any(|m| m == unit));
        }
        assert!(missing.iter().any(|m| m == REGISTRY_FIXTURE));
        assert_eq!(outcomes.len(), 5);
        fs::remove_dir_all(&dir).unwrap();
    }
}
