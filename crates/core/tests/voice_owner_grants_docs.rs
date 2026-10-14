//! Regression: the voice owner-grant journal must be documented.
//!
//! The `voice_owner_grants` journal appears in all four executable
//! inventories (backup dump order, member-erasure plan, role matrix, rollback
//! specs) but was missing from the four operator documents that explain what
//! those inventories mean. This test pins the documentation side only: each
//! of the four docs must name the journal. It checks for the table name as a
//! substring, never for exact prose, so the Builder can word the docs freely.
//!
//! Database-free and deterministic: it reads committed text files from the
//! repository root and asserts substring presence. The Builder owns the docs
//! fix and must not edit this file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const JOURNAL: &str = "voice_owner_grants";

const DOC_PATHS: &[&str] = &[
    "docs/backup.md",
    "docs/privacy.md",
    "docs/database-roles.md",
    "docs/rollback-delta.md",
];

const EXECUTABLE_PATHS: &[&str] = &[
    "crates/core/src/backup/dump_file.rs",
    "crates/cutover/src/member_erasure_plan.json",
    "sql/database_role_matrix.sql",
    "crates/cutover/src/rollback_delta.rs",
];

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_files(root: &Path, paths: &[&str]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for path in paths {
        let content =
            std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"));
        out.insert((*path).to_owned(), content);
    }
    out
}

fn check_journal_documentation(
    docs: &BTreeMap<String, String>,
    executables: &BTreeMap<String, String>,
) -> Result<(), String> {
    let missing_executables: Vec<&str> = executables
        .iter()
        .filter(|(_, content)| !content.contains(JOURNAL))
        .map(|(path, _)| path.as_str())
        .collect();
    if !missing_executables.is_empty() {
        return Err(format!(
            "executable inventory no longer lists {JOURNAL}: {}",
            missing_executables.join(", ")
        ));
    }
    let missing_docs: Vec<&str> = docs
        .iter()
        .filter(|(_, content)| !content.contains(JOURNAL))
        .map(|(path, _)| path.as_str())
        .collect();
    if !missing_docs.is_empty() {
        return Err(format!(
            "operator docs omit {JOURNAL}: {}. Each listed doc must name the journal.",
            missing_docs.join(", ")
        ));
    }
    Ok(())
}

#[test]
fn committed_docs_cover_owner_grant_journal() {
    let root = repository_root();
    let docs = read_files(&root, DOC_PATHS);
    let executables = read_files(&root, EXECUTABLE_PATHS);
    check_journal_documentation(&docs, &executables).unwrap();
}

#[test]
fn rejects_deliberate_doc_omission() {
    // Synthetic passing pair proves the helper itself is sound, independent
    // of the current committed docs (which omit the journal until the
    // Builder lands the docs fix).
    let passing_docs: BTreeMap<String, String> = DOC_PATHS
        .iter()
        .map(|path| ((*path).to_owned(), format!("coverage for {JOURNAL}")))
        .collect();
    let passing_executables: BTreeMap<String, String> = EXECUTABLE_PATHS
        .iter()
        .map(|path| ((*path).to_owned(), format!("inventory lists {JOURNAL}")))
        .collect();
    check_journal_documentation(&passing_docs, &passing_executables).unwrap();

    // Dropping the journal from any single doc must fail naming that doc.
    for omitted in DOC_PATHS {
        let mut docs = passing_docs.clone();
        docs.insert((*omitted).to_owned(), "no journal named here".to_owned());
        let error = check_journal_documentation(&docs, &passing_executables).unwrap_err();
        assert!(
            error.contains(omitted),
            "omitting {omitted} should name it: {error}"
        );
        assert!(
            error.contains(JOURNAL),
            "omission error should name the journal: {error}"
        );
    }

    // Dropping the journal from an executable must fail naming it.
    let mut executables = passing_executables.clone();
    executables.insert(
        EXECUTABLE_PATHS[0].to_owned(),
        "no journal named here".to_owned(),
    );
    let error = check_journal_documentation(&passing_docs, &executables).unwrap_err();
    assert!(
        error.contains(EXECUTABLE_PATHS[0]),
        "unexpected error: {error}"
    );
}
