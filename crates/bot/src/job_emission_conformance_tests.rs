//! Job-emission conformance: every scrape job maps to a live emitter.
//!
//! The generic supervisor records success and failure for framework jobs, and
//! the log-volume guard pins the job count. Counts alone cannot catch a job
//! that stops being scheduled or a new emitter that bypasses the framework,
//! so this suite parses the rendered scrape exposition for `job="..."` labels
//! (the parsed job templates) and requires each one to name a live emitter
//! source or an explicit parked exemption with its reason.
//!
//! The framework pattern is a `jobs::` use: audit_runtime shows the shape a
//! supervised emitter keeps. A file that calls `job_success` or `job_failure`
//! without that marker bypasses the supervisor and fails here until it joins
//! the explicit bypass list with its reason. Nothing here changes runtime
//! behavior; the suite is fully offline.

use std::collections::{BTreeMap, BTreeSet};

use two_bot_core::metrics;

/// Marker every supervised emitter keeps in its source.
const FRAMEWORK_MARKER: &str = "jobs::";

/// Emitter sources read at compile time so the suite is fully offline.
const EMITTER_SOURCES: [(&str, &str); 10] = [
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
    ("automod_gateway.rs", include_str!("automod_gateway.rs")),
    ("gateway_metrics.rs", include_str!("gateway_metrics.rs")),
];

/// Every scrape job with a supervised emitter, paired with the file that
/// schedules it. The file must name the job and keep the framework marker.
const LIVE_JOB_EMITTERS: [(&str, &str); 9] = [
    ("counter", "website_jobs.rs"),
    ("rank", "website_jobs.rs"),
    ("scheduled_events", "website_jobs.rs"),
    ("settings", "settings_jobs.rs"),
    ("presence_probe", "community_jobs.rs"),
    ("community_scorecard", "community_jobs.rs"),
    ("inactivity", "community_jobs.rs"),
    ("audit_retry", "audit_runtime.rs"),
    ("scheduled_messages", "scheduled_jobs.rs"),
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
/// not grow the job allowlist.
const OTHER_COLLAPSED: [(&str, &str); 4] = [
    ("feeds", "feed_jobs.rs"),
    ("member_unban_sweep", "member_runtime.rs"),
    ("self_role_recovery", "self_role_handlers.rs"),
    ("automod_expiry", "automod_gateway.rs"),
];

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
    for job in &parsed {
        if job == "other" {
            continue;
        }
        let live = LIVE_JOB_EMITTERS
            .iter()
            .any(|(name, _)| *name == job.as_str());
        let direct = DIRECT_BYPASS
            .iter()
            .any(|(name, _, _)| *name == job.as_str());
        let parked = PARKED_EXEMPTIONS
            .iter()
            .any(|(name, _)| *name == job.as_str());
        assert!(
            live || direct || parked,
            "scrape job {job} has no live emitter or parked exemption; \
             schedule it through the supervisor or record the parked reason here"
        );
    }
    for (job, file) in LIVE_JOB_EMITTERS {
        let source = source_by_name(file);
        assert!(
            source.contains(job),
            "live emitter {file} no longer names {job}; a dropped schedule must fail here"
        );
        assert!(
            source.contains(FRAMEWORK_MARKER),
            "live emitter {file} lost the {FRAMEWORK_MARKER} framework pattern for {job}"
        );
    }
    for (job, file, _) in DIRECT_BYPASS {
        let source = source_by_name(file);
        assert!(
            source.contains(job),
            "bypass emitter {file} no longer names {job}"
        );
    }
    for (job, reason) in PARKED_EXEMPTIONS {
        assert!(
            !reason.is_empty(),
            "parked exemption {job} needs a documented reason"
        );
    }
}

#[test]
fn emitters_stay_on_the_framework_pattern() {
    for (file, source) in EMITTER_SOURCES {
        let is_bypass = DIRECT_BYPASS.iter().any(|(_, name, _)| *name == file);
        let calls_direct = source.contains("job_success") || source.contains("job_failure");
        if is_bypass {
            assert!(
                calls_direct,
                "bypass emitter {file} lost its direct emission; remove the bypass row"
            );
            continue;
        }
        if file == "jobs.rs" {
            continue;
        }
        if calls_direct {
            panic!(
                "emitter {file} calls job_success/job_failure outside the supervisor; \
                 route it through the framework or record it in DIRECT_BYPASS"
            );
        }
        assert!(
            source.contains(FRAMEWORK_MARKER) || !source.contains("Job {"),
            "emitter {file} builds a supervisor job without the {FRAMEWORK_MARKER} pattern"
        );
    }
    let framework = include_str!("jobs.rs");
    assert!(
        framework.contains("job_success") && framework.contains("job_failure"),
        "supervisor lost its success/failure emission"
    );
    let audit = source_by_name("audit_runtime.rs");
    assert!(
        audit.contains(FRAMEWORK_MARKER),
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
