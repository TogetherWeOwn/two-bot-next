//! A help flag must not bypass pool preflight in consumers that ignore it.

use std::process::Command;

#[test]
fn ignored_help_flags_cannot_bypass_pool_preflight() {
    for bin in [
        env!("CARGO_BIN_EXE_levels-import-mee6"),
        env!("CARGO_BIN_EXE_levels-role-rewards"),
        env!("CARGO_BIN_EXE_levels-import-rewards-probe"),
    ] {
        let mut command = Command::new(bin);
        command
            .env_clear()
            .env("TWO_DB_POOL_MAX", "private-pool-sentinel")
            .env("TWO_DATABASE_URL", "not-a-database-url")
            .args(["--guild", "100000000000000001", "--help"]);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        let output = command.output().expect("run offline CLI");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
        assert_eq!(output.status.code(), Some(2), "{bin}: {stderr}");
        assert!(output.stdout.is_empty(), "{bin}: unexpected plan");
        assert!(
            stderr.starts_with("input error: code=invalid_pool_max "),
            "{bin}: {stderr}"
        );
        assert!(stderr.contains("TWO_DB_POOL_MAX"), "{bin}: {stderr}");
        assert!(stderr.contains("next_action="), "{bin}: {stderr}");
        assert!(!stderr.contains("private-pool-sentinel"), "{bin}: {stderr}");
        assert!(!stderr.contains("not-a-database-url"), "{bin}: {stderr}");
    }
}
