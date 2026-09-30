//! How many backups survive the nightly prune, and which ones.
//!
//! Port of legacy `src/store/backupRetention.ts`. The failure mode is silent,
//! total and destructive, so the arithmetic is pinned by tests that run
//! without a database.
//!
//! The bug this exists to prevent: a malformed `TWO_BACKUP_KEEP` must never
//! select the whole list for deletion. A backup system whose response to a
//! typo in its own setting is to delete all the backups is worse than not
//! having one, because the failure is invisible until the day you need a
//! restore.

use thiserror::Error;

/// Backups kept when `TWO_BACKUP_KEEP` is not set at all. Two weeks of nights.
pub const DEFAULT_KEEP: usize = 14;

/// A malformed retention setting. Refused, never guessed at.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct RetentionError(pub String);

/// Parse `TWO_BACKUP_KEEP`.
///
/// Unset, or whitespace, means the default — the scheduler cannot always tell
/// "unset" from "empty", and defaulting there is safe because it errs towards
/// keeping files. Anything else must be a positive whole number, and if it is
/// not we throw rather than guess: there is no interpretation of `keep=0` or
/// `keep=NaN` that a person typing it into a timer file actually wanted, and
/// every silent interpretation deletes everything.
pub fn parse_keep(raw: Option<&str>) -> Result<usize, RetentionError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_KEEP);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_KEEP);
    }
    // ASCII digits only: rejects "1_4", "1e3", "0x0", "1.5", "-1", "fourteen".
    // (Rust's own parser would accept the underscores and the float forms.)
    if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(RetentionError(format!(
            "TWO_BACKUP_KEEP={raw:?} is not a positive whole number"
        )));
    }
    let keep: usize = trimmed.parse().map_err(|_| {
        RetentionError(format!(
            "TWO_BACKUP_KEEP={raw:?} is not a positive whole number"
        ))
    })?;
    if keep < 1 {
        return Err(RetentionError(format!(
            "TWO_BACKUP_KEEP={raw:?} is not a positive whole number"
        )));
    }
    Ok(keep)
}

/// Given backup entries newest-first, return the ones to delete.
///
/// Separate from the parse so the arithmetic is pinned independently: `keep`
/// arriving here as anything other than a positive integer is a programming
/// error, and is refused rather than silently selecting the whole list.
pub fn to_prune<T>(newest_first: &[T], keep: usize) -> &[T] {
    assert!(keep >= 1, "refusing to prune with keep={keep}");
    if newest_first.len() <= keep {
        return &[];
    }
    &newest_first[keep..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_means_the_default() {
        assert_eq!(parse_keep(None), Ok(DEFAULT_KEEP));
    }

    #[test]
    fn empty_or_whitespace_means_the_default_not_zero() {
        for raw in ["", " ", "\t", "\n"] {
            assert_eq!(parse_keep(Some(raw)), Ok(DEFAULT_KEEP), "input {raw:?}");
        }
    }

    #[test]
    fn positive_whole_numbers_pass_through() {
        assert_eq!(parse_keep(Some("1")), Ok(1));
        assert_eq!(parse_keep(Some("7")), Ok(7));
        assert_eq!(parse_keep(Some(" 30 ")), Ok(30));
    }

    #[test]
    fn values_that_used_to_prune_everything_are_now_refused() {
        // Each of these previously reached slice() as 0 or NaN.
        for raw in [
            "fourteen", "1_4", "0", "-1", "1.5", "1e3", "0x0", "null", "NaN", "14 days",
        ] {
            assert!(
                parse_keep(Some(raw)).is_err(),
                "TWO_BACKUP_KEEP={raw:?} should be refused, not guessed at"
            );
        }
    }

    #[test]
    fn keeps_the_newest_and_returns_the_rest() {
        let five = ["n1", "n2", "n3", "n4", "n5"]; // newest first
        assert_eq!(to_prune(&five, 2), &["n3", "n4", "n5"]);
        assert_eq!(to_prune(&five, 1), &["n2", "n3", "n4", "n5"]);
    }

    #[test]
    fn keep_larger_than_the_list_prunes_nothing() {
        let five = ["n1", "n2", "n3", "n4", "n5"];
        assert!(to_prune(&five, 14).is_empty());
    }
}
