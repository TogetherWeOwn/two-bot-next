//! Pure V7d game-alias resolution core, derived from `docs/voice-rooms.md`
//! §V7 only.
//!
//! `/alias` lets admins add, edit or remove game-name aliases. Aliases apply
//! to `@@game_name@@` and to `GAME` conditions. This module is the pure
//! matching core behind that panel:
//!
//! - [`normalize_game_name`] folds a raw activity name into its comparison
//!   form (matching only; display strings are never touched).
//! - [`resolve_alias`] matches a normalized name against the alias table
//!   with exact-then-longest-prefix semantics.
//! - [`applies_to`] answers whether a template token or condition head is
//!   alias-subject.
//!
//! Storage owns the table. V7a ([TOG-12015]) owns the alias table shape
//! (`AliasTable` with add/edit/remove plus `/nick`); this core deliberately
//! duplicates none of it and instead defines the minimal read-only view it
//! needs ([`AliasRow`]). When V7a merges, its `AliasTable::entries()` maps
//! row-for-row onto this view (key/target pairs, keys unique under folding),
//! and the single-hop chain rule V7a upholds keeps resolution here a pure
//! function of `(input, table)`. This module performs no I/O and holds no
//! Discord, store, clock or database types.
//!
//! [TOG-12015]: https://github.com/TogetherWeOwn/two-bot-next/issues/12015

use unicode_normalization::UnicodeNormalization;

/// One alias row as this core sees it: the raw game name (**key**) the admin
/// typed, and the **target** canonical name `@@game_name@@` and `GAME`
/// conditions see when the key matches.
///
/// Both halves are borrowed exactly as stored (trimming and case are handled
/// by matching, never by mutating the row). Well-formed tables carry keys
/// unique under [`normalize_game_name`], which is what V7a's `AliasTable`
/// guarantees; duplicate folded keys resolve deterministically (first row
/// wins) but indicate a table that violates that invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AliasRow<'a> {
    pub key: &'a str,
    pub target: &'a str,
}

/// The comparison form of a game name, for matching only.
///
/// Applies NFKC compatibility folding, trims surrounding whitespace, then
/// case-folds. Compatibility forms (full-width letters, ligatures, `™`) and
/// canonically equivalent spellings therefore match their plain-ASCII case
/// twin (`Ａｐｅｘ Ｌｅｇｅｎｄｓ` folds to `apex legends`), while interior
/// whitespace and punctuation are preserved. Display strings are untouched:
/// callers render the stored target or the raw detected name, never this.
#[must_use]
pub fn normalize_game_name(raw: &str) -> String {
    raw.nfkc().collect::<String>().trim().to_lowercase()
}

/// Resolve a normalized game name against the alias table.
///
/// `normalized` must already be in [`normalize_game_name`] form; keys are
/// normalized the same way before comparison, so stored keys may use any case
/// or compatibility spelling. Semantics:
///
/// 1. An exact match on a key returns that row's target.
/// 2. Otherwise the longest key that prefixes the input returns its target
///    (`apex` matches `apex legends custom lobby`, preferring `apex legends`
///    over `apex` when both are keys).
/// 3. Anything else returns [`None`]; the caller falls back to the raw
///    detected name.
///
/// Resolution is a pure function of `(normalized, rows)`: same inputs give
/// the same output regardless of row order, provided keys are unique under
/// folding (the V7a invariant). The returned target is borrowed from the
/// matching row, exactly as stored.
#[must_use]
pub fn resolve_alias<'a>(normalized: &str, rows: &[AliasRow<'a>]) -> Option<&'a str> {
    // Exact match first: it always beats any prefix, whatever the row order.
    if let Some(row) = rows
        .iter()
        .find(|row| normalize_game_name(row.key) == normalized)
    {
        return Some(row.target);
    }
    // Longest prefix wins; strictly-greater comparison keeps the first row on
    // ties, which cannot occur in a well-formed (fold-unique) table.
    let mut best: Option<(&'a str, usize)> = None;
    for row in rows {
        let key = normalize_game_name(row.key);
        if !key.is_empty() && normalized.starts_with(key.as_str()) {
            let len = key.len();
            if best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((row.target, len));
            }
        }
    }
    best.map(|(target, _)| target)
}

/// Whether a template token or condition head is alias-subject.
///
/// Returns `true` for the `@@game_name@@` token name (`game_name`, matched
/// case-insensitively — the naming engine lowercases token names at parse)
/// and for the `GAME` condition head (`game`, matched case-insensitively).
/// Every other token (`@@owner@@`, `PLAYERS`, `COUNT`, …) returns `false` and
/// is never rewritten by alias resolution.
///
/// `token` is the parsed token name or the condition head word, without
/// `@@` markers; surrounding whitespace is tolerated.
#[must_use]
pub fn applies_to(token: &str) -> bool {
    let folded = token.trim().to_lowercase();
    folded == "game_name" || folded == "game"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_and_case_folds() {
        assert_eq!(normalize_game_name("  Apex Legends "), "apex legends");
        assert_eq!(normalize_game_name("\tVALORANT\n"), "valorant");
    }

    #[test]
    fn normalize_nfkc_folds_compatibility_forms() {
        // Full-width spellings fold to their ASCII twins.
        assert_eq!(
            normalize_game_name("\u{FF21}pex"),
            normalize_game_name("Apex")
        );
        // The ligature ﬁ (U+FB01) compatibility-decomposes to "fi".
        assert_eq!(normalize_game_name("\u{FB01}nal Fantasy"), "final fantasy");
    }

    #[test]
    fn normalize_preserves_interior_spacing() {
        assert_eq!(normalize_game_name("Apex  Legends"), "apex  legends");
    }
}
