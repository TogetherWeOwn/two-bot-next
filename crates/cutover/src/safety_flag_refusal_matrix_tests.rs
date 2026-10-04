use super::Args;

const SAFETY_FLAGS: [&str; 3] = ["apply", "allow-lower", "allow-live-guild"];

fn parse(argv: &[&str]) -> Result<Args, String> {
    Args::try_parse(&argv.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

#[test]
fn valued_opt_ins_refuse_with_exact_copy() {
    for flag in SAFETY_FLAGS {
        let expected = format!("--{flag} is a bare opt-in and does not accept a value");
        let bare = format!("--{flag}");
        for value in ["false", "true", "0", "1", "no", "", "fixture.json"] {
            let equals = format!("--{flag}={value}");
            for invalid in [vec![equals.as_str()], vec![bare.as_str(), value]] {
                let mut before = invalid.clone();
                before.extend_from_slice(&["--guild-id", "fixture-guild"]);
                let mut after = vec!["--guild-id", "fixture-guild"];
                after.extend_from_slice(&invalid);
                for argv in [invalid, before, after] {
                    assert_eq!(parse(&argv).unwrap_err(), expected, "{argv:?}");
                }
            }
        }
    }
}

#[test]
fn repeated_and_mixed_opt_ins_refuse_with_exact_copy() {
    for flag in SAFETY_FLAGS {
        let bare = format!("--{flag}");
        let equals_false = format!("--{flag}=false");
        let repeated = format!("--{flag} must not be repeated");
        let valued = format!("--{flag} is a bare opt-in and does not accept a value");
        for (argv, expected) in [
            (vec![bare.as_str(), bare.as_str()], repeated.as_str()),
            (
                vec![bare.as_str(), "--guild-id=fixture-guild", bare.as_str()],
                repeated.as_str(),
            ),
            (
                vec![bare.as_str(), bare.as_str(), "false"],
                repeated.as_str(),
            ),
            (vec![bare.as_str(), equals_false.as_str()], valued.as_str()),
            (vec![equals_false.as_str(), bare.as_str()], valued.as_str()),
            (vec![bare.as_str(), "false", bare.as_str()], valued.as_str()),
        ] {
            assert_eq!(parse(&argv).unwrap_err(), expected, "{argv:?}");
        }
    }
}

#[test]
fn apply_dry_run_conflicts_refuse_with_exact_copy() {
    for argv in [
        vec!["--apply", "--dry-run"],
        vec!["--dry-run", "--apply"],
        vec!["--apply", "--dry-run=false"],
        vec!["--dry-run", "false", "--apply"],
    ] {
        assert_eq!(
            parse(&argv).unwrap_err(),
            "--apply conflicts with --dry-run",
            "{argv:?}"
        );
    }
    let dry_run = parse(&["--dry-run"]).unwrap();
    assert!(dry_run.has("dry-run"));
    assert!(!dry_run.has("apply"));
}

#[test]
fn bare_opt_ins_remain_flags_alongside_ordinary_arguments() {
    for argv in [
        vec![
            "inventory",
            "--apply",
            "--guild-id",
            "fixture-guild",
            "--allow-lower",
            "--input=fixture.json",
            "--allow-live-guild",
        ],
        vec![
            "inventory",
            "--allow-live-guild",
            "--input",
            "fixture.json",
            "--guild-id=fixture-guild",
            "--allow-lower",
            "--apply",
        ],
    ] {
        let args = parse(&argv).unwrap();
        assert_eq!(args.positionals, ["inventory"]);
        for flag in SAFETY_FLAGS {
            assert!(args.flags.contains(flag));
            assert!(!args.values.contains_key(flag));
            assert!(args.has(flag));
            assert_eq!(args.get(flag), Some(""));
        }
        assert_eq!(args.get("guild-id"), Some("fixture-guild"));
        assert_eq!(args.get("input"), Some("fixture.json"));
    }
    let defaults = parse(&[]).unwrap();
    for flag in SAFETY_FLAGS {
        assert!(!defaults.has(flag));
        assert_eq!(defaults.get(flag), None);
    }
}
