//! Independent V3/V7/V10 access golden corpus (spec-only, hermetic).
//!
//! Fixture `fixtures/voice_access_golden.json` is written from
//! `docs/voice-rooms.md` V3 (`/private`, `/public`, the Join channel), V7
//! (`/alias`, `/nick`) and V10 (the guild-level controls bullet) without
//! reading any implementation. The V3b/V7a/V10b core cards wire this corpus
//! into their tests once both sides have merged.
//!
//! This inventory test depends on no unmerged module: it checks fixture
//! integrity (at least 40 cases, each with a clause ID, an input and an
//! expected outcome), requires at least one positive and one negative case
//! per clause ID, and requires every spec bullet in the fixture's bullet map
//! to resolve to at least one clause.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/voice_access_golden.json"))
        .expect("voice access golden fixture parses")
}

fn clause_ids(fixture: &Value) -> Vec<&str> {
    fixture["clauses"]
        .as_array()
        .expect("clauses is an array")
        .iter()
        .map(|clause| clause["id"].as_str().expect("every clause has a string id"))
        .collect()
}

fn cases(fixture: &Value) -> Vec<&Value> {
    fixture["cases"]
        .as_array()
        .expect("cases is an array")
        .iter()
        .collect()
}

#[test]
fn corpus_has_at_least_forty_cases() {
    let fixture = fixture();
    assert_eq!(fixture["version"], 1);
    assert!(
        cases(&fixture).len() >= 40,
        "independent corpus needs at least 40 cases"
    );
}

#[test]
fn clause_and_case_ids_are_unique() {
    let fixture = fixture();
    let clauses = clause_ids(&fixture);
    assert_eq!(
        clauses.len(),
        clauses.iter().collect::<BTreeSet<_>>().len(),
        "duplicate clause id"
    );
    let case_ids: Vec<&str> = cases(&fixture)
        .iter()
        .map(|case| case["id"].as_str().expect("every case has a string id"))
        .collect();
    assert_eq!(
        case_ids.len(),
        case_ids.iter().collect::<BTreeSet<_>>().len(),
        "duplicate case id"
    );
}

#[test]
fn every_case_has_a_known_clause_an_input_and_an_expected_outcome() {
    let fixture = fixture();
    let known: BTreeSet<&str> = clause_ids(&fixture).into_iter().collect();
    for case in cases(&fixture) {
        let id = case["id"].as_str().expect("every case has a string id");
        let clause = case["clause"]
            .as_str()
            .unwrap_or_else(|| panic!("{id} has no clause id"));
        assert!(known.contains(clause), "{id} names unknown clause {clause}");
        assert!(case["input"].is_object(), "{id} has no input object");
        assert!(
            case["expected"].is_object() && !case["expected"].as_object().unwrap().is_empty(),
            "{id} has no expected outcome"
        );
    }
}

#[test]
fn every_clause_has_a_positive_and_a_negative_case() {
    let fixture = fixture();
    let mut polarity: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for case in cases(&fixture) {
        let id = case["id"].as_str().expect("every case has a string id");
        let clause = case["clause"]
            .as_str()
            .unwrap_or_else(|| panic!("{id} has no clause id"));
        let side = case["polarity"]
            .as_str()
            .unwrap_or_else(|| panic!("{id} has no polarity"));
        assert!(
            side == "positive" || side == "negative",
            "{id} has unknown polarity {side}"
        );
        polarity.entry(clause).or_default().insert(side);
    }
    for clause in clause_ids(&fixture) {
        let sides = polarity.get(clause).cloned().unwrap_or_default();
        assert!(sides.contains("positive"), "{clause} has no positive case");
        assert!(sides.contains("negative"), "{clause} has no negative case");
    }
}

#[test]
fn every_spec_bullet_maps_to_at_least_one_clause() {
    let fixture = fixture();
    let known: BTreeSet<&str> = clause_ids(&fixture).into_iter().collect();
    let bullets = fixture["bullets"]
        .as_array()
        .expect("bullet map is an array");
    assert!(!bullets.is_empty(), "bullet map must list the spec bullets");
    let mut referenced: BTreeSet<&str> = BTreeSet::new();
    for bullet in bullets {
        let id = bullet["id"].as_str().expect("every bullet has a string id");
        let targets = bullet["clauses"]
            .as_array()
            .unwrap_or_else(|| panic!("bullet {id} names no clauses"));
        assert!(!targets.is_empty(), "bullet {id} maps to no clause");
        for target in targets {
            let clause = target
                .as_str()
                .unwrap_or_else(|| panic!("bullet {id} has a non-string clause"));
            assert!(
                known.contains(clause),
                "bullet {id} maps to unknown clause {clause}"
            );
            referenced.insert(clause);
        }
    }
    // Every clause must back at least one bullet, and every bullet id must be
    // claimed by at least one clause, so neither side of the map can drift.
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    for clause in fixture["clauses"].as_array().expect("clauses is an array") {
        let id = clause["id"].as_str().expect("every clause has a string id");
        let bullets = clause["bullets"]
            .as_array()
            .unwrap_or_else(|| panic!("clause {id} claims no bullets"));
        assert!(!bullets.is_empty(), "clause {id} claims no bullets");
        for bullet in bullets {
            claimed.insert(
                bullet
                    .as_str()
                    .unwrap_or_else(|| panic!("clause {id} has a non-string bullet")),
            );
        }
    }
    let mapped: BTreeSet<&str> = bullets
        .iter()
        .map(|bullet| bullet["id"].as_str().expect("every bullet has a string id"))
        .collect();
    assert_eq!(
        claimed, mapped,
        "clause bullet refs must match the bullet map"
    );
    for clause in known {
        assert!(referenced.contains(clause), "{clause} backs no spec bullet");
    }
}
