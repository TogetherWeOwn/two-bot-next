//! TOG-11808 staging shape-check for temp-voice (no runtime).
//!
//! Ports the S6 staging shape-check only: generator/category/protected-channel
//! config shape against the spec. There is no voice runtime, store, migration,
//! Discord or database contact here. All cases are pure fixtures loaded from
//! `fixtures/temp_voice_shape.json` plus in-memory mutations.
//!
//! Spec sources (frozen):
//! - parity matrix §9 drop 6: temp-voice runtime never shipped on legacy
//!   `main`; only the staging shape-check ports under S6.
//! - soak s8-08: temp-voice is staging shape-check only, no runtime row.
//! - settings catalogue: `TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID` (voice channel
//!   used to request a temporary room), `TWO_TEMP_VOICE_CATEGORY_ID`
//!   (category for generated rooms), `TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS`
//!   (channels cleanup may not remove). Classification only; no runtime wiring.
//! - preflight view-only references: generator/category/protected IDs are
//!   comma-separated nonzero snowflakes, checked for existence and View.
//!
//! Shape rules pinned here:
//! 1. Unknown keys refused at every object boundary (`deny_unknown_fields`).
//! 2. `protected_channel_ids` ⊆ `known_channels` IDs.
//! 3. Category/generator consistency: both known, generator is voice,
//!    category is a category, IDs distinct.
//! 4. Snowflakes are canonical nonzero decimal `u64` strings (no leading
//!    zeros, no numbers, no blanks, no out-of-range).

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ChannelKind {
    Voice,
    Stage,
    Text,
    Category,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelEntry {
    id: String,
    kind: ChannelKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct TempVoiceShape {
    version: u32,
    generator_channel_id: String,
    category_id: String,
    known_channels: Vec<ChannelEntry>,
    protected_channel_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum ShapeError {
    Malformed(String),
    Invalid(String),
    UnknownReference(String),
}

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(field) => write!(f, "malformed shape at {field}"),
            Self::Invalid(field) => write!(f, "invalid shape at {field}"),
            Self::UnknownReference(field) => write!(f, "unknown channel reference at {field}"),
        }
    }
}

fn is_canonical_snowflake(id: &str) -> bool {
    if id.is_empty() || id.starts_with('0') {
        return false;
    }
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    id.parse::<u64>().is_ok_and(|n| n > 0)
}

fn parse_shape(bytes: &[u8]) -> Result<TempVoiceShape, ShapeError> {
    serde_json::from_slice::<TempVoiceShape>(bytes)
        .map_err(|_| ShapeError::Malformed("document".to_owned()))
}

fn validate_shape(shape: &TempVoiceShape) -> Result<BTreeMap<String, ChannelKind>, ShapeError> {
    if shape.version != 1 {
        return Err(ShapeError::Invalid("version".to_owned()));
    }
    if !is_canonical_snowflake(&shape.generator_channel_id) {
        return Err(ShapeError::Malformed("generator_channel_id".to_owned()));
    }
    if !is_canonical_snowflake(&shape.category_id) {
        return Err(ShapeError::Malformed("category_id".to_owned()));
    }
    if shape.known_channels.is_empty() {
        return Err(ShapeError::Invalid("known_channels".to_owned()));
    }
    let mut known: BTreeMap<String, ChannelKind> = BTreeMap::new();
    for (i, entry) in shape.known_channels.iter().enumerate() {
        let field = format!("known_channels[{i}].id");
        if !is_canonical_snowflake(&entry.id) {
            return Err(ShapeError::Malformed(field));
        }
        if known.insert(entry.id.clone(), entry.kind).is_some() {
            return Err(ShapeError::Invalid(field));
        }
    }
    let generator_kind = known
        .get(&shape.generator_channel_id)
        .ok_or_else(|| ShapeError::UnknownReference("generator_channel_id".to_owned()))?;
    if *generator_kind != ChannelKind::Voice {
        return Err(ShapeError::Invalid("generator_channel_id".to_owned()));
    }
    let category_kind = known
        .get(&shape.category_id)
        .ok_or_else(|| ShapeError::UnknownReference("category_id".to_owned()))?;
    if *category_kind != ChannelKind::Category {
        return Err(ShapeError::Invalid("category_id".to_owned()));
    }
    if shape.generator_channel_id == shape.category_id {
        return Err(ShapeError::Invalid("category_id".to_owned()));
    }
    let mut seen = BTreeSet::new();
    for (i, id) in shape.protected_channel_ids.iter().enumerate() {
        let field = format!("protected_channel_ids[{i}]");
        if !is_canonical_snowflake(id) {
            return Err(ShapeError::Malformed(field));
        }
        if !known.contains_key(id) {
            return Err(ShapeError::UnknownReference(field));
        }
        if !seen.insert(id) {
            return Err(ShapeError::Invalid(field));
        }
    }
    Ok(known)
}

fn fixture_bytes() -> Vec<u8> {
    include_str!("fixtures/temp_voice_shape.json")
        .as_bytes()
        .to_vec()
}

fn fixture_shape() -> TempVoiceShape {
    let bytes = fixture_bytes();
    let shape = parse_shape(&bytes).expect("fixture must parse");
    validate_shape(&shape).expect("fixture must validate");
    shape
}

fn fixture_value() -> Value {
    serde_json::from_slice(&fixture_bytes()).expect("fixture must be JSON")
}

#[test]
fn valid_fixture_passes_shape_check() {
    let shape = fixture_shape();
    assert_eq!(shape.version, 1);
    assert_eq!(shape.generator_channel_id, "101");
    assert_eq!(shape.category_id, "200");
    let known = validate_shape(&shape).unwrap();
    assert_eq!(known.len(), 5);
    assert_eq!(known["101"], ChannelKind::Voice);
    assert_eq!(known["200"], ChannelKind::Category);
    // protected ⊆ known.
    for id in &shape.protected_channel_ids {
        assert!(known.contains_key(id), "protected {id} must be known");
    }
    assert_eq!(
        shape.protected_channel_ids,
        vec!["102".to_owned(), "103".to_owned()]
    );
    // Fixture is the documented staging example: do not silently shrink it.
    let value = fixture_value();
    assert_eq!(value["known_channels"].as_array().unwrap().len(), 5);
    assert_eq!(value["protected_channel_ids"].as_array().unwrap().len(), 2);
}

#[test]
fn unknown_top_level_keys_are_refused() {
    let mut value = fixture_value();
    value["unrecognized"] = json!("untrusted-content");
    let err = parse_shape(&serde_json::to_vec(&value).unwrap()).unwrap_err();
    assert_eq!(err, ShapeError::Malformed("document".to_owned()));
    assert!(!err.to_string().contains("untrusted-content"));
}

#[test]
fn unknown_nested_keys_are_refused() {
    let mut value = fixture_value();
    value["known_channels"][0]["unrecognized"] = json!(true);
    assert_eq!(
        parse_shape(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
        ShapeError::Malformed("document".to_owned())
    );
    // Unknown kind variants are also refused, not defaulted.
    let mut value = fixture_value();
    value["known_channels"][0]["kind"] = json!("forum");
    assert_eq!(
        parse_shape(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
        ShapeError::Malformed("document".to_owned())
    );
}

#[test]
fn protected_must_be_subset_of_known() {
    let shape = fixture_shape();
    let mut bad = shape.clone();
    bad.protected_channel_ids.push("999".to_owned());
    let err = validate_shape(&bad).unwrap_err();
    assert_eq!(
        err,
        ShapeError::UnknownReference("protected_channel_ids[2]".to_owned())
    );
    // Empty protected list is allowed (nothing to sweep-protect); unknown is not.
    let mut empty = shape.clone();
    empty.protected_channel_ids.clear();
    validate_shape(&empty).unwrap();
}

#[test]
fn generator_and_category_must_be_known_with_expected_kinds() {
    let shape = fixture_shape();
    // Generator must be a voice channel: pointing at the category fails.
    let mut bad = shape.clone();
    bad.generator_channel_id = shape.category_id.clone();
    assert_eq!(
        validate_shape(&bad).unwrap_err(),
        ShapeError::Invalid("generator_channel_id".to_owned())
    );
    // Category must be a category: pointing at a voice channel fails.
    let mut bad = shape.clone();
    bad.category_id = shape.generator_channel_id.clone();
    assert_eq!(
        validate_shape(&bad).unwrap_err(),
        ShapeError::Invalid("category_id".to_owned())
    );
    // Unknown generator/category IDs fail as unknown references, not silently.
    for (field, expected) in [
        ("generator_channel_id", "generator_channel_id"),
        ("category_id", "category_id"),
    ] {
        let mut bad = shape.clone();
        if field == "generator_channel_id" {
            bad.generator_channel_id = "999".to_owned();
        } else {
            bad.category_id = "999".to_owned();
        }
        assert_eq!(
            validate_shape(&bad).unwrap_err(),
            ShapeError::UnknownReference(expected.to_owned()),
            "{field}"
        );
    }
    // Stage/text channels are not valid generator or category targets.
    let mut bad = shape.clone();
    bad.generator_channel_id = "300".to_owned();
    assert_eq!(
        validate_shape(&bad).unwrap_err(),
        ShapeError::Invalid("generator_channel_id".to_owned())
    );
    let mut bad = shape.clone();
    bad.category_id = "300".to_owned();
    assert_eq!(
        validate_shape(&bad).unwrap_err(),
        ShapeError::Invalid("category_id".to_owned())
    );
}

#[test]
fn generator_and_category_must_be_distinct() {
    let mut shape = fixture_shape();
    // Force both to the same known voice channel: distinctness + kind checks
    // must refuse (here the category kind check fires first).
    shape.category_id = shape.generator_channel_id.clone();
    assert!(validate_shape(&shape).is_err());
}

#[test]
fn snowflakes_reject_noncanonical_ids() {
    let shape = fixture_shape();
    for id in [
        "",
        "0",
        "01",
        " 101",
        "101 ",
        "+101",
        "-1",
        "1.0",
        "abc",
        "18446744073709551616",
    ] {
        let mut bad = shape.clone();
        bad.generator_channel_id = id.to_owned();
        assert_eq!(
            validate_shape(&bad).unwrap_err(),
            ShapeError::Malformed("generator_channel_id".to_owned()),
            "{id}"
        );
        let mut bad = shape.clone();
        bad.category_id = id.to_owned();
        assert_eq!(
            validate_shape(&bad).unwrap_err(),
            ShapeError::Malformed("category_id".to_owned()),
            "{id}"
        );
        let mut bad = shape.clone();
        bad.protected_channel_ids[0] = id.to_owned();
        assert_eq!(
            validate_shape(&bad).unwrap_err(),
            ShapeError::Malformed("protected_channel_ids[0]".to_owned()),
            "{id}"
        );
    }
    // JSON numbers are not snowflakes on this wire: strings only.
    let mut value = fixture_value();
    value["generator_channel_id"] = json!(101);
    assert_eq!(
        parse_shape(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
        ShapeError::Malformed("document".to_owned())
    );
}

#[test]
fn duplicate_known_and_protected_entries_are_rejected() {
    let shape = fixture_shape();
    let mut bad = shape.clone();
    bad.known_channels.push(bad.known_channels[0].clone());
    assert_eq!(
        validate_shape(&bad).unwrap_err(),
        ShapeError::Invalid("known_channels[5].id".to_owned())
    );
    let mut bad = shape.clone();
    bad.protected_channel_ids
        .push(bad.protected_channel_ids[0].clone());
    assert_eq!(
        validate_shape(&bad).unwrap_err(),
        ShapeError::Invalid("protected_channel_ids[2]".to_owned())
    );
}

#[test]
fn malformed_documents_and_versions_are_refused() {
    for bytes in [b"".as_slice(), b"{", b"[]", b"null", b"{}", b"{} trailing"] {
        assert_eq!(
            parse_shape(bytes).unwrap_err(),
            ShapeError::Malformed("document".to_owned())
        );
    }
    let mut value = fixture_value();
    value["version"] = json!(2);
    assert_eq!(
        validate_shape(&parse_shape(&serde_json::to_vec(&value).unwrap()).unwrap()).unwrap_err(),
        ShapeError::Invalid("version".to_owned())
    );
    let mut value = fixture_value();
    value.as_object_mut().unwrap().remove("version");
    assert_eq!(
        parse_shape(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
        ShapeError::Malformed("document".to_owned())
    );
}

#[test]
fn empty_inventory_and_missing_sections_are_refused() {
    let shape = fixture_shape();
    let mut bad = shape.clone();
    bad.known_channels.clear();
    assert_eq!(
        validate_shape(&bad).unwrap_err(),
        ShapeError::Invalid("known_channels".to_owned())
    );
    for field in [
        "generator_channel_id",
        "category_id",
        "known_channels",
        "protected_channel_ids",
    ] {
        let mut value = fixture_value();
        value.as_object_mut().unwrap().remove(field);
        assert_eq!(
            parse_shape(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            ShapeError::Malformed("document".to_owned()),
            "missing {field}"
        );
    }
}
