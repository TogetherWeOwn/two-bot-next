//! Unique-name collision folding: NFKC + Unicode lowercase + trim.
//!
//! Pins the legacy `rename.ts` rule on [`two_bot_core::voice_room_controls::name_conflicts`]:
//! names that differ only by case, compatibility forms (full-width,
//! ligatures) or surrounding whitespace collide when unique names are on,
//! and nothing collides when the setting is off.

use two_bot_core::voice_room_controls::name_conflicts;

#[test]
fn case_only_difference_conflicts() {
    let existing = ["Lounge"];
    assert!(name_conflicts("lounge", &existing, true));
    assert!(name_conflicts("LOUNGE", &existing, true));
    assert!(name_conflicts("lOuNgE", &existing, true));
}

#[test]
fn fullwidth_compatibility_form_conflicts() {
    // U+FF2C U+FF4F U+FF55 U+FF4E U+FF47 U+FF45 NFKC-fold to ASCII "Lounge".
    let existing = ["Lounge"];
    assert!(name_conflicts("Ｌｏｕｎｇｅ", &existing, true));
    assert!(name_conflicts("ｌｏｕｎｇｅ", &existing, true));
}

#[test]
fn ligature_folds_to_ascii() {
    // U+FB01 (ﬁ) NFKC-folds to "fi", so "ﬁsh" collides with "Fish".
    let existing = ["Fish"];
    assert!(name_conflicts("\u{fb01}sh", &existing, true));
    assert!(name_conflicts("FISH", &existing, true));
}

#[test]
fn surrounding_whitespace_is_trimmed() {
    let existing = ["Lounge"];
    assert!(name_conflicts(" Lounge", &existing, true));
    assert!(name_conflicts("Lounge ", &existing, true));
    assert!(name_conflicts("  Lounge\t", &existing, true));
    // Interior whitespace is significant: no fold removes it.
    assert!(!name_conflicts("Lo unge", &existing, true));
}

#[test]
fn distinct_names_do_not_conflict() {
    let existing = ["Lounge", "lounge room"];
    assert!(!name_conflicts("Den", &existing, true));
    assert!(!name_conflicts("Den", &[] as &[&str], true));
    assert!(!name_conflicts("Loung", &existing, true));
}

#[test]
fn off_switch_never_conflicts_even_folded() {
    let existing = ["Lounge"];
    assert!(!name_conflicts("Lounge", &existing, false));
    assert!(!name_conflicts("LOUNGE", &existing, false));
    assert!(!name_conflicts("Ｌｏｕｎｇｅ", &existing, false));
    assert!(!name_conflicts(" Lounge ", &[] as &[&str], false));
}
