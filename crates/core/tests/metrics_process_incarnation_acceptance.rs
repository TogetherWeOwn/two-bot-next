//! Process attribution is bounded numeric exposition, stable for a process and
//! independent of each registry/render. No database or gateway is needed.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::process::Command;
use two_bot_core::metrics::{global, Metrics};

fn incarnation(text: &str) -> [u32; 4] {
    assert!(
        text.lines()
            .any(|line| line == "# TYPE two_bot_process_incarnation gauge"),
        "process identity must be an always-emitted gauge family"
    );
    let rows: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("two_bot_process_incarnation"))
        .collect();
    assert_eq!(rows.len(), 4, "exactly four identity samples: {rows:?}");
    let mut words = BTreeMap::new();
    for line in rows {
        let (series, value) = line.split_once(' ').expect("numeric identity sample");
        let labels = series
            .strip_prefix("two_bot_process_incarnation{part=\"")
            .and_then(|rest| rest.strip_suffix("\"}"))
            .expect("identity must have only a fixed part label, never an identity label");
        assert!(matches!(labels, "0" | "1" | "2" | "3"));
        let numeric: f64 = value.parse().expect("finite numeric limb");
        assert!(numeric.is_finite());
        assert_eq!(numeric.fract(), 0.0);
        assert!((0.0..=f64::from(u32::MAX)).contains(&numeric));
        let word: u32 = value.parse().expect("unsigned decimal u32 limb");
        assert!(words.insert(labels, word).is_none(), "duplicate part");
    }
    let result = [words["0"], words["1"], words["2"], words["3"]];
    assert_ne!(
        result, [0; 4],
        "all-zero is an unknown identity, not an incarnation"
    );
    result
}

#[test]
fn process_incarnation_is_four_finite_u32_words_with_only_fixed_labels() {
    incarnation(&Metrics::default().render(None));
}

#[test]
fn process_incarnation_survives_repeated_renders_and_counter_changes() {
    let metrics = Metrics::default();
    let initial = incarnation(&metrics.render(None));
    for _ in 0..16 {
        metrics.gateway_disconnect();
        assert_eq!(incarnation(&metrics.render(None)), initial);
    }
}

#[test]
fn process_incarnation_is_shared_by_registries_global_and_threads() {
    let initial = incarnation(&Metrics::default().render(None));
    assert_eq!(incarnation(&Metrics::default().render(None)), initial);
    assert_eq!(incarnation(&global().render(None)), initial);
    let peers: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| incarnation(&Metrics::default().render(None))))
        .collect();
    for peer in peers {
        assert_eq!(peer.join().expect("render thread"), initial);
    }
}

// Invoked with --exact by the process-lifetime test below. The normal test run
// also checks the child probe's exposition; it never relies on a hidden env flag.
#[test]
fn process_incarnation_child_probe() {
    let words = incarnation(&Metrics::default().render(None));
    // Libtest may leave its single-threaded test-name prefix on this line.
    writeln!(
        io::stdout().lock(),
        "\nPROCESS_INCARNATION={:08x}{:08x}{:08x}{:08x}",
        words[0],
        words[1],
        words[2],
        words[3]
    )
    .expect("write child identity receipt");
}

fn child_identity() -> String {
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "process_incarnation_child_probe",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("run isolated process identity probe");
    assert!(output.status.success(), "child identity contract must pass");
    let stdout = String::from_utf8(output.stdout).expect("child output");
    let identities: Vec<_> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("PROCESS_INCARNATION="))
        .collect();
    assert_eq!(identities.len(), 1, "exactly one child identity receipt");
    let identity = identities[0];
    assert_eq!(identity.len(), 32, "child identity must be 32 hex digits");
    assert!(
        identity
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "child identity must be canonical lowercase hex"
    );
    assert!(
        identity.bytes().any(|byte| byte != b'0'),
        "child identity must not be all-zero"
    );
    identity.to_owned()
}

#[test]
fn process_incarnation_changes_between_processes_not_between_renders() {
    let first = child_identity();
    let second = child_identity();
    assert_ne!(
        first, second,
        "a constant or reused identity hides replacement"
    );
}
