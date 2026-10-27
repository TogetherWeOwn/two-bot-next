//! Shared onboarding destination syntax for settings writes and runtime readers.
//! Parsing does not prove channel existence, type, visibility or permissions.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("destination must contain nonzero Discord snowflakes")]
pub struct InvalidDestination;

fn id(value: &str) -> Result<u64, InvalidDestination> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or(InvalidDestination)
}

/// Missing and whitespace-only values mean no optional destination. A runtime
/// may separately require the destination for its selected onboarding mode.
pub fn parse_optional_id(value: Option<&str>) -> Result<Option<u64>, InvalidDestination> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(id)
        .transpose()
}

/// Trim CSV entries and omit empty segments, preserving order and duplicates.
pub fn parse_channel_ids(value: Option<&str>) -> Result<Vec<u64>, InvalidDestination> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(id)
        .collect()
}
