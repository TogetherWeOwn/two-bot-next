//! Job-emission conformance: every scrape job maps to a live emitter.
//!
//! The generic supervisor records success and failure for framework jobs, and
//! the log-volume guard pins the job count. Counts alone cannot catch a job
//! that stops being scheduled or a new emitter that bypasses the framework,
//! so this suite parses the rendered scrape exposition for `job="..."` labels
//! (the parsed job templates) and requires each one to name a live emitter
//! source or an explicit parked exemption with its reason.
//!
//! The framework pattern is a supervisor import (`crate::jobs`) plus the
//! startup-jitter schedule or a built `Job`: audit_runtime shows the shape a
//! supervised emitter keeps. A file that calls `job_success` / `job_failure`
//! as a method (`.job_success(`) without joining the explicit bypass list
//! with its reason bypasses the supervisor and fails here — including files
//! the suite has never seen before, which a whole-`src` walk catches.
//! Registration is checked where it happens (`website_jobs.rs`), not just
//! where jobs are defined. Nothing here changes runtime behavior; the suite
//! is fully offline.

use std::collections::{BTreeMap, BTreeSet};

use two_bot_core::metrics;

/// `needle` present with a word boundary: the byte before the match must not
/// be an identifier character, so `website_jobs::` never satisfies a `jobs::`
/// marker.
fn has_bounded_marker(source: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = source[from..].find(needle) {
        let start = from + rel;
        if start == 0
            || !matches!(
                source.as_bytes()[start - 1],
                b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_'
            )
        {
            return true;
        }
        from = start + needle.len();
    }
    false
}

/// A direct supervisor bypass: a `.job_success(` / `.job_failure(` method
/// call. The leading dot keeps helper names such as
/// `invalid_onboarding_job_failure` from tripping the scan.
fn emits_directly(source: &str) -> bool {
    source.contains(".job_success(") || source.contains(".job_failure(")
}

/// The shape a supervised emitter keeps: a supervisor import plus either the
/// startup-jitter schedule or a built `Job`. `feed_jobs` takes the second
/// arm: it mints `Job`s with a zero jitter for the gated register path
/// instead of calling `startup_jitter` itself.
fn has_framework_marker(source: &str) -> bool {
    let imports_supervisor =
        source.contains("crate::jobs") || has_bounded_marker(source, "jobs::{");
    imports_supervisor
        && (has_bounded_marker(source, "jobs::startup_jitter(") || source.contains("Job {"))
}

/// Emitter sources read at compile time so the suite is fully offline.
const EMITTER_SOURCES: [(&str, &str); 11] = [
    ("website_jobs.rs", include_str!("website_jobs.rs")),
    ("settings_jobs.rs", include_str!("settings_jobs.rs")),
    ("community_jobs.rs", include_str!("community_jobs.rs")),
    ("scheduled_jobs.rs", include_str!("scheduled_jobs.rs")),
    ("audit_runtime.rs", include_str!("audit_runtime.rs")),
    ("member_runtime.rs", include_str!("member_runtime.rs")),
    (
        "self_role_handlers.rs",
        include_str!("self_role_handlers.rs"),
    ),
    ("feed_jobs.rs", include_str!("feed_jobs.rs")),
    ("lfg_jobs.rs", include_str!("lfg_jobs.rs")),
    ("automod_gateway.rs", include_str!("automod_gateway.rs")),
    ("gateway_metrics.rs", include_str!("gateway_metrics.rs")),
];

/// Every scrape job with a supervised emitter, paired with the file that
/// schedules it. The file must name the job and keep the framework marker.
const LIVE_JOB_EMITTERS: [(&str, &str); 14] = [
    ("counter", "website_jobs.rs"),
    ("rank", "website_jobs.rs"),
    ("scheduled_events", "website_jobs.rs"),
    ("settings", "settings_jobs.rs"),
    ("presence_probe", "community_jobs.rs"),
    ("community_scorecard", "community_jobs.rs"),
    ("inactivity", "community_jobs.rs"),
    ("audit_retry", "audit_runtime.rs"),
    ("scheduled_messages", "scheduled_jobs.rs"),
    ("feeds", "feed_jobs.rs"),
    ("member_unban_sweep", "member_runtime.rs"),
    ("self_role_recovery", "self_role_handlers.rs"),
    ("automod_expiry", "automod_gateway.rs"),
    ("lfg_sweep", "lfg_jobs.rs"),
];

/// Every `website_jobs.rs` call that schedules a live job. Naming the job in
/// its defining file is not enough: a registration that stops being called
/// must fail here even while the name still appears elsewhere.
const REGISTER_CALLS: [&str; 9] = [
    "crate::lfg_jobs::register(",
    "crate::settings_jobs::job(",
    "scheduled_jobs::register(",
    "community_jobs::register(",
    "audit_runtime::register(",
    "feed_jobs::register(",
    "member_runtime::sweep_job(",
    ".recovery_job()",
    "automod_gateway::expiry_job(",
];

/// Sinks that prove a scheduled job actually reaches `registered`, the vec
/// that feeds `jobs::supervise`. Call text alone is not enough: `let _ = ...`
/// or `drop(...)` keeps the call while the job never registers, so each entry
/// pins the sink together with the call, matched against the source with
/// full-line `//` comments removed.
const REGISTER_SINKS: [&str; 8] = [
    "registered.push(crate::settings_jobs::job(",
    "registered.push(Job {",
    "registered.extend(scheduled)",
    "registered.push(member_runtime::sweep_job(",
    "registered.extend(registration.jobs)",
    "registered.push(crate::automod_gateway::expiry_job(",
    "registered.push(service.recovery_job())",
    "Some(job) => registered.push(job)",
];

/// Entries the shared `NAMES` loop schedules. Pinned inside the array block
/// so a quiet removal fails here even when the name still appears elsewhere
/// in the file.
const NAMES_ENTRIES: [&str; 4] = [
    "\"counter\"",
    "\"rank\"",
    "\"scheduled_events\"",
    "settings_jobs::NAME",
];

/// Rows of the `community_jobs.rs` register table, matched with all
/// whitespace removed so reformatting cannot silently unpin them. The bare
/// quoted name is not enough: `NAMES` and log fields repeat it, so each entry
/// pins the name together with its `Kind`, which only the registering row
/// keeps adjacent.
const COMMUNITY_TABLE_ROWS: [&str; 3] = [
    "\"presence_probe\",Kind::PresenceProbe",
    "\"community_scorecard\",Kind::Scorecard",
    "\"inactivity\",Kind::Inactivity",
];

/// Pins that turn a community table row into a registered job, matched with
/// all whitespace removed (full-line `//` comments stripped first) so
/// reformatting cannot silently unpin them. The table rows alone are not
/// enough: `out.push(Job {` builds each `Job`, `Registration { jobs: out,`
/// returns them, the `if !enabled { continue; }` gate keeps disabled jobs
/// parked, and each row's trailing `enabled` expression decides the default.
/// Any of these silently changing must fail here, the same way a dropped
/// `registered.*` sink fails for `website_jobs.rs`.
const COMMUNITY_REGISTER_PINS: [&str; 6] = [
    "out.push(Job{",
    "Registration{jobs:out,",
    "if!enabled{continue;}",
    "\"presence_probe\",Kind::PresenceProbe,gates.presence",
    "\"community_scorecard\",Kind::Scorecard,gates.scorecard.is_some()",
    "\"inactivity\",Kind::Inactivity,gates.inactivity_days.is_some()",
];

/// Direct emission outside the supervisor. `session_checkpoint` is the dispatch
/// commit marker recorded by DispatchTimer on every durable gateway commit:
/// success-only, no failure path, never a scheduled tick.
const DIRECT_BYPASS: [(&str, &str, &str); 1] = [(
    "session_checkpoint",
    "gateway_metrics.rs",
    "dispatch commit marker via DispatchTimer, success-only, not a supervisor tick",
)];

/// Scrape jobs with no live tick. `invite_snapshot` keeps its series so
/// scrapers and alerts keep a stable label while the gateway seeds snapshots
/// during the checkpoint commit instead of running a dedicated job.
const PARKED_EXEMPTIONS: [(&str, &str); 1] = [(
    "invite_snapshot",
    "legacy snapshot label retained for scrape continuity; snapshots seed during the checkpoint commit",
)];

/// Supervisor jobs that intentionally share the trailing collapse label so the
/// cardinality budget holds. Each must appear in its emitter source and must
/// not grow the job allowlist. Empty right now: the former collapsed jobs
/// earned their own allowlist rows and moved to [`LIVE_JOB_EMITTERS`]. The
/// seam stays so a future collapse lands here instead of passing silently.
const OTHER_COLLAPSED: [(&str, &str); 0] = [];

/// Every `job="..."` label in `two_bot_job_*` exposition lines, in first-seen
/// order deduplicated. Parses the scrape text, not the allowlist, so a dropped
/// or renamed series changes the set.
fn parse_scrape_job_labels(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        if !line.starts_with("two_bot_job_") {
            continue;
        }
        let mut search_from = 0;
        while let Some(rel) = line[search_from..].find("job=\"") {
            let start = search_from + rel + 5;
            let rest = &line[start..];
            match rest.find('"') {
                Some(end) => {
                    out.insert(rest[..end].to_owned());
                    search_from = start + end + 1;
                }
                None => break,
            }
        }
    }
    out
}

/// Fixture seam for the conformance rule: `live` names the jobs with an
/// emitter, `parked` maps an exempt job to its documented reason.
fn conformance_for<'a>(
    job: &str,
    live: &BTreeSet<&str>,
    parked: &BTreeMap<&str, &'a str>,
) -> Result<&'a str, &'static str> {
    if live.contains(job) {
        return Ok("live");
    }
    if let Some(reason) = parked.get(job) {
        if reason.is_empty() {
            return Err("parked exemption needs a documented reason");
        }
        return Ok("parked");
    }
    Err("job has no live emitter or parked exemption")
}

fn source_by_name(name: &str) -> &'static str {
    EMITTER_SOURCES
        .iter()
        .find_map(|(file, text)| (*file == name).then_some(*text))
        .unwrap_or_else(|| panic!("no emitter source for {name}"))
}

/// Source with full-line `//` comments removed, so a sink hidden in a
/// comment cannot satisfy a registration pin.
fn code_text(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Source with all whitespace removed, so a registering row stays pinned
/// across reformatting while staying adjacent-only.
fn compacted(source: &str) -> String {
    source.chars().filter(|c| !c.is_whitespace()).collect()
}

/// A new module that emits directly must fail here even though it is not in
/// [`EMITTER_SOURCES`]: walk every file under `crates/bot/src` so an
/// unlisted emitter cannot bypass the supervisor silently, whether or not it
/// is wired into `main.rs`. Test-only sources (`*_tests.rs` files and
/// `tests` / `*_tests` directories) are skipped; the supervisor itself and
/// the [`DIRECT_BYPASS`] files are exempt by name.
fn scan_every_source_file_for_direct_emission() {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let bypass: BTreeSet<&str> = DIRECT_BYPASS.iter().map(|(_, file, _)| *file).collect();
    let mut stack = vec![root.to_path_buf()];
    let mut offenders = Vec::new();
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|_| panic!("read emitter dir {}", dir.display()))
            .map(|entry| entry.expect("src dir entry").path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                if !name.ends_with("tests") {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs")
                || name == "jobs.rs"
                || name.ends_with("_tests.rs")
                || bypass.contains(name.as_str())
            {
                continue;
            }
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("read emitter {name}"));
            if emits_directly(&text) {
                offenders.push(name);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "emitters call job_success/job_failure outside the supervisor: {}; \
         route them through the framework or record them in DIRECT_BYPASS",
        offenders.join(", ")
    );
}

#[test]
fn scrape_parser_covers_every_job_series() {
    let text = metrics::Metrics::default().render(None);
    let parsed = parse_scrape_job_labels(&text);
    let allowed: BTreeSet<String> = metrics::JOBS.iter().map(|job| (*job).to_owned()).collect();
    assert_eq!(
        parsed, allowed,
        "scrape parser drifted from the job allowlist; the parser must see every job series"
    );
}

#[test]
fn every_scrape_job_has_a_live_emitter_or_parked_reason() {
    let text = metrics::Metrics::default().render(None);
    let parsed = parse_scrape_job_labels(&text);
    let live: BTreeSet<&str> = LIVE_JOB_EMITTERS
        .iter()
        .map(|(name, _)| *name)
        .chain(DIRECT_BYPASS.iter().map(|(name, _, _)| *name))
        .collect();
    let parked: BTreeMap<&str, &str> = PARKED_EXEMPTIONS.into_iter().collect();
    for job in &parsed {
        if job == "other" {
            continue;
        }
        assert!(
            conformance_for(job, &live, &parked).is_ok(),
            "scrape job {job} has no live emitter or parked exemption; \
             schedule it through the supervisor or record the parked reason here"
        );
    }
    let website = source_by_name("website_jobs.rs");
    for (job, file) in LIVE_JOB_EMITTERS {
        let source = source_by_name(file);
        let quoted = format!("\"{job}\"");
        assert!(
            source.contains(&quoted),
            "live emitter {file} no longer names {job}; a dropped schedule must fail here"
        );
        assert!(
            has_framework_marker(source),
            "live emitter {file} lost the supervisor pattern for {job}"
        );
    }
    for snippet in REGISTER_CALLS {
        assert!(
            website.contains(snippet),
            "website_jobs.rs no longer calls {snippet}; a dropped registration must fail here"
        );
    }
    let website_code = code_text(website);
    for sink in REGISTER_SINKS {
        assert!(
            website_code.contains(sink),
            "website_jobs.rs no longer registers via {sink}; a job that stops reaching `registered` must fail here"
        );
    }
    // Feed, audit and LFG share this sink; the LFG call-to-sink adjacency
    // is also pinned separately below so one cannot mask another's removal.
    let push_job_sinks = website_code.matches("registered.push(job)").count();
    assert!(
        push_job_sinks >= 3,
        "website_jobs.rs lost a `registered.push(job)` arm (feed/audit/LFG); found {push_job_sinks}, need 3"
    );
    let community_rows = compacted(&code_text(source_by_name("community_jobs.rs")));
    for row in COMMUNITY_TABLE_ROWS {
        assert!(
            community_rows.contains(row),
            "community_jobs.rs register table lost row {row}; a dropped table row must fail here"
        );
    }
    for pin in COMMUNITY_REGISTER_PINS {
        assert!(
            community_rows.contains(pin),
            "community_jobs.rs register lost pin {pin}; a job that stops being built, returned, gated, or enabled must fail here"
        );
    }
    let names_block = website
        .split("pub const NAMES")
        .nth(1)
        .expect("website_jobs.rs NAMES array")
        .split("];")
        .next()
        .expect("NAMES array terminator");
    for entry in NAMES_ENTRIES {
        assert!(
            names_block.contains(entry),
            "website_jobs.rs NAMES no longer schedules {entry}; a dropped schedule must fail here"
        );
    }
    for (job, file, _) in DIRECT_BYPASS {
        let source = source_by_name(file);
        let quoted = format!("\"{job}\"");
        assert!(
            source.contains(&quoted),
            "bypass emitter {file} no longer names {job}"
        );
    }
}

#[test]
fn lfg_boot_registration_reaches_the_supervisor_and_readiness() {
    let website = compacted(&code_text(source_by_name("website_jobs.rs")));
    assert!(
        website.contains(
            "ifletSome(job)=crate::lfg_jobs::register(context.clone(),&activation){registered.push(job);}else{parked.push(crate::lfg_jobs::NAME);}"
        ),
        "LFG must reach the supervisor when enabled and stay explicitly parked otherwise"
    );
    assert!(
        website.contains(".chain([crate::lfg_jobs::NAME])"),
        "readiness must include LFG even when parked"
    );
    let main = compacted(&code_text(include_str!("main.rs")));
    assert!(main.contains("modlfg_jobs;"), "LFG module must be compiled");
    let lfg = compacted(&code_text(source_by_name("lfg_jobs.rs")));
    assert!(
        lfg.contains("timeout:Duration::from_secs(120)"),
        "LFG's bounded attempt timeout belongs to the existing supervisor"
    );
    for bypass in [
        "tokio::spawn(",
        "tokio::time::interval(",
        "tokio::time::sleep(",
    ] {
        assert!(
            !lfg.contains(bypass),
            "LFG must not privately schedule via {bypass}"
        );
    }
}

#[test]
fn emitters_stay_on_the_framework_pattern() {
    for (file, source) in EMITTER_SOURCES {
        let is_bypass = DIRECT_BYPASS.iter().any(|(_, name, _)| *name == file);
        if is_bypass {
            assert!(
                emits_directly(source),
                "bypass emitter {file} lost its direct emission; remove the bypass row"
            );
            continue;
        }
        if emits_directly(source) {
            panic!(
                "emitter {file} calls job_success/job_failure outside the supervisor; \
                 route it through the framework or record it in DIRECT_BYPASS"
            );
        }
        assert!(
            has_framework_marker(source),
            "emitter {file} lost the supervisor pattern for its jobs"
        );
    }
    scan_every_source_file_for_direct_emission();
    let framework = include_str!("jobs.rs");
    assert!(
        emits_directly(framework),
        "supervisor lost its success/failure emission"
    );
    let audit = source_by_name("audit_runtime.rs");
    assert!(
        has_framework_marker(audit),
        "audit_runtime lost the framework pattern this test locks in"
    );
}

#[test]
fn other_collapsed_supervisor_jobs_stay_explicit() {
    for (job, file) in OTHER_COLLAPSED {
        let source = source_by_name(file);
        assert!(
            source.contains(job),
            "collapsed job {job} missing from {file}"
        );
        assert!(
            !metrics::JOBS.contains(&job),
            "collapsed job {job} joined the allowlist without a budget row; update the guard"
        );
    }
    assert_eq!(
        metrics::JOBS.last(),
        Some(&"other"),
        "job allowlist lost its collapse trapdoor"
    );
}

#[test]
fn fixture_emitter_present_maps_to_live() {
    let live: BTreeSet<&str> = ["counter"].into_iter().collect();
    let parked: BTreeMap<&str, &str> = BTreeMap::new();
    assert_eq!(conformance_for("counter", &live, &parked), Ok("live"));
}

#[test]
fn fixture_emitter_missing_fails_closed() {
    let live: BTreeSet<&str> = ["counter"].into_iter().collect();
    let parked: BTreeMap<&str, &str> = BTreeMap::new();
    assert!(
        conformance_for("scheduled_messages", &live, &parked).is_err(),
        "a dropped schedule must fail, not pass silently"
    );
}

#[test]
fn fixture_parked_exemption_maps_to_parked() {
    let live: BTreeSet<&str> = BTreeSet::new();
    let parked: BTreeMap<&str, &str> = [("invite_snapshot", "retained for scrape continuity")]
        .into_iter()
        .collect();
    assert_eq!(
        conformance_for("invite_snapshot", &live, &parked),
        Ok("parked")
    );
    let empty_reason: BTreeMap<&str, &str> = [("invite_snapshot", "")].into_iter().collect();
    assert!(
        conformance_for("invite_snapshot", &live, &empty_reason).is_err(),
        "a parked exemption without a reason must fail"
    );
}
