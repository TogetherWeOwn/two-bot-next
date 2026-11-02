//! Bounded reads for untrusted import files.
//!
//! Every cutover import CLI buffers an operator- or workflow-supplied file
//! before parsing it. All seven intake sites share this one helper: reads go
//! through `.take(INTAKE_CAP_BYTES + 1)` so an over-cap file is refused
//! without ever buffering it whole, and the boundary is strict `>`: exactly
//! the cap is admitted, one byte more is refused before any parse, database
//! or network touch.
//!
//! Refusals use fixed vocabulary so agents can match them: [`over_cap_message`]
//! renders `<role> exceeds 8 MiB`, naming the file role and the cap. Row and
//! problem bounds live with their parsers ([`crate::mee6_xp`],
//! [`crate::mee6_rewards`], [`crate::rollback_import`]); this module only
//! bounds the byte read and the shared problem-list truncation receipt.

use std::io::Read;
use std::path::Path;

/// The intake byte cap every cutover import read enforces: 8 MiB, matching
/// the capped operator-tool siblings (`ghost_cleanup`, `raid-remove`).
pub const INTAKE_CAP_BYTES: usize = 8 * 1024 * 1024;

/// How the cap renders in refusals.
pub const INTAKE_CAP_LABEL: &str = "8 MiB";

/// Maximum accumulated problems kept before truncation; the receipt preserves
/// the total count while the content is cut.
pub const MAX_KEPT_PROBLEMS: usize = 100;

/// Bounded-read failure: over the cap vs unreadable (missing file,
/// directory, I/O error, or invalid UTF-8 for the string read, which
/// surfaces as `InvalidData` exactly like `read_to_string`). The I/O detail
/// travels with the error so callers keep their existing `cannot read`
/// wording byte for byte.
#[derive(Debug)]
pub enum IntakeReadError {
    OverCap,
    Unreadable(std::io::Error),
}

/// Fixed-vocabulary refusal naming the file role and the cap.
#[must_use]
pub fn over_cap_message(role: &str) -> String {
    format!("{role} exceeds {INTAKE_CAP_LABEL}")
}

/// Fixed truncation receipt preserving the total problem count. Appended
/// after the first [`MAX_KEPT_PROBLEMS`] problems when the input produced
/// more; counts preserved, content truncated.
#[must_use]
pub fn truncation_receipt(total_problems: usize) -> String {
    let omitted = total_problems.saturating_sub(MAX_KEPT_PROBLEMS);
    format!(
        "truncated: showing the first {MAX_KEPT_PROBLEMS} of {total_problems} problems ({omitted} more omitted)"
    )
}

/// Read a file as bytes, refusing before buffering past the cap. Exactly
/// [`INTAKE_CAP_BYTES`] is admitted; one byte more is [`IntakeReadError::OverCap`].
pub fn read_bytes_capped(path: impl AsRef<Path>) -> Result<Vec<u8>, IntakeReadError> {
    let file = std::fs::File::open(path.as_ref()).map_err(IntakeReadError::Unreadable)?;
    let mut bytes = Vec::new();
    file.take(INTAKE_CAP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(IntakeReadError::Unreadable)?;
    if bytes.len() > INTAKE_CAP_BYTES {
        return Err(IntakeReadError::OverCap);
    }
    Ok(bytes)
}

/// Read a file as UTF-8 text, refusing before buffering past the cap.
/// Invalid UTF-8 reads as [`IntakeReadError::Unreadable`], like the
/// `read_to_string` path it replaces.
pub fn read_string_capped(path: impl AsRef<Path>) -> Result<String, IntakeReadError> {
    let bytes = read_bytes_capped(path)?;
    String::from_utf8(bytes).map_err(|e| {
        IntakeReadError::Unreadable(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR").map(std::path::PathBuf::from))
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&base);
        base.join(format!("intake-cap-{}-{tag}", std::process::id()))
    }

    #[test]
    fn over_cap_message_names_role_and_cap() {
        assert_eq!(over_cap_message("MEE6 export"), "MEE6 export exceeds 8 MiB");
    }

    #[test]
    fn truncation_receipt_preserves_the_total() {
        let receipt = truncation_receipt(200_000);
        assert!(
            receipt.contains("200000") || receipt.contains("200,000"),
            "{receipt}"
        );
        assert!(receipt.contains("more"), "{receipt}");
    }

    #[test]
    fn boundary_exactly_the_cap_is_admitted_one_more_is_refused() {
        let at_cap = scratch("at-cap");
        std::fs::write(&at_cap, vec![b'a'; INTAKE_CAP_BYTES]).unwrap();
        assert_eq!(read_bytes_capped(&at_cap).unwrap().len(), INTAKE_CAP_BYTES);
        let over_cap = scratch("over-cap");
        std::fs::write(&over_cap, vec![b'a'; INTAKE_CAP_BYTES + 1]).unwrap();
        assert!(matches!(
            read_bytes_capped(&over_cap).unwrap_err(),
            IntakeReadError::OverCap
        ));
        let _ = std::fs::remove_file(&at_cap);
        let _ = std::fs::remove_file(&over_cap);
    }

    #[test]
    fn missing_file_and_directory_are_unreadable_not_over_cap() {
        let missing = scratch("absent");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(
            read_bytes_capped(&missing).unwrap_err(),
            IntakeReadError::Unreadable(_)
        ));
        assert!(matches!(
            read_string_capped(std::env::temp_dir()).unwrap_err(),
            IntakeReadError::Unreadable(_)
        ));
    }

    #[test]
    fn invalid_utf8_is_unreadable() {
        let path = scratch("invalid-utf8");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        let err = read_string_capped(&path).unwrap_err();
        assert!(
            matches!(err, IntakeReadError::Unreadable(_)),
            "invalid UTF-8 must read as unreadable"
        );
        let _ = std::fs::remove_file(&path);
    }
}
