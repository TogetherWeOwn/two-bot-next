//! Runs the independent voice-template corpus (`tests/voice_templates/`)
//! against the naming engine with the full V6 policy, [`TemplateExtensions`].
//!
//! The corpus was authored from `docs/voice-rooms.md`, not from this engine.
//! Expected outputs are never edited here: a spec-vs-engine disagreement is a
//! finding for the engine owner, reported as case ID plus expected vs actual.
//!
//! - `exact` cases compare byte-for-byte.
//! - `invariant` cases check nonempty output, the 100-character ceiling,
//!   repeat-render equality, `allowed_outputs` and `casefold_equals`; every
//!   `stability_groups` entry must render the same output across contexts.
//! - `deferred` cases await spec clarification: skipped and counted.
//!
//! Every non-deferred case must pass, including the V6 conditional and
//! styling cases.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;
use time::{Date, Month};
use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_naming::{
    majority_games, parse, render, resolve_majority_game, ChannelKind, GameOptions, PartyInfo,
    RoomContext,
};
use two_bot_core::voice_template::TemplateExtensions;

const CORPUS: &str = include_str!("../../../tests/voice_templates/corpus.json");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    version: u32,
    #[allow(dead_code)]
    spec: serde_json::Value,
    contexts: BTreeMap<String, CorpusContext>,
    cases: Vec<Case>,
    stability_groups: Vec<StabilityGroup>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusContext {
    channel_kind: String,
    number: u32,
    limit: u32,
    private: bool,
    seed: String,
    owner_id: String,
    original_creator_name: String,
    members: Vec<Member>,
    clock: Clock,
    settings: Settings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    id: String,
    display_name: String,
    nick: Option<String>,
    roles: Vec<String>,
    game: Option<String>,
    live_discord: bool,
    live_external: bool,
    stream_title: Option<String>,
    party: Option<Party>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Party {
    id: String,
    size: u32,
    maximum: Option<u32>,
    state: String,
    details: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Clock {
    weekday: String,
    month: String,
    hour: u8,
    timezone: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    no_game: String,
    aliases: HashMap<String, String>,
    named_lists: HashMap<String, Vec<String>>,
    force_single_game: bool,
    include_inactive: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    input: String,
    context: String,
    expected: Expected,
    #[allow(dead_code)]
    covers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Expected {
    Exact {
        output: String,
    },
    Invariant {
        nonempty: bool,
        max_characters: usize,
        stable_for_same_context: bool,
        #[serde(default)]
        allowed_outputs: Option<Vec<String>>,
        #[serde(default)]
        casefold_equals: Option<String>,
    },
    Deferred {
        ambiguity_id: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StabilityGroup {
    id: String,
    case_ids: Vec<String>,
}

/// One corpus context as the engine sees it: name tokens and V6 condition
/// facts.
struct Room {
    context: RoomContext,
    facts: ConditionFacts,
}

/// Adapt the implementation-independent snapshot to the engine's context,
/// following V5 to V7: owner name prefers `/nick`, the game is the
/// alias-resolved majority, and parties are deduplicated by ID in first-seen
/// order. Condition facts come from the same snapshot, so `GAME` reads the
/// titles `@@game_name@@` shows and roles are those of the members present.
fn room(context: &CorpusContext) -> Room {
    let owner = context.members.iter().find(|m| m.id == context.owner_id);
    let mut parties: Vec<(&str, PartyInfo)> = Vec::new();
    for party in context.members.iter().filter_map(|m| m.party.as_ref()) {
        if !parties.iter().any(|(id, _)| *id == party.id) {
            parties.push((
                party.id.as_str(),
                PartyInfo {
                    size: party.size,
                    max: party.maximum,
                    state: party.state.clone(),
                    details: party.details.clone(),
                },
            ));
        }
    }
    let activities: Vec<Option<String>> = context.members.iter().map(|m| m.game.clone()).collect();
    let game_options = GameOptions {
        aliases: context.settings.aliases.clone(),
        force_single: context.settings.force_single_game,
        count_idle_toward_majority: context.settings.include_inactive,
        no_game_label: context.settings.no_game.clone(),
    };
    let owner_game = owner.and_then(|m| m.game.as_deref());
    let count = |is: fn(&Member) -> bool| context.members.iter().filter(|m| is(m)).count() as u32;
    let room_context = RoomContext {
        channel_kind: match context.channel_kind.as_str() {
            "temporary" => ChannelKind::Temporary,
            "standalone" | "stage" => ChannelKind::Standalone,
            other => panic!("unknown channel kind {other}"),
        },
        room_number: context.number,
        owner_name: owner
            .map(|m| m.nick.clone().unwrap_or_else(|| m.display_name.clone()))
            .unwrap_or_default(),
        original_creator_name: context.original_creator_name.clone(),
        member_count: context.members.len() as u32,
        owner_present: owner.is_some(),
        live_count: count(is_live),
        user_limit: context.limit,
        game_name: resolve_majority_game(&activities, owner_game, &game_options),
        stream_title: owner
            .filter(|m| is_live(m))
            .and_then(|m| m.stream_title.clone())
            .unwrap_or_default(),
        members_playing: context.members.iter().filter(|m| m.game.is_some()).count() as u32,
        parties: parties.into_iter().map(|(_, party)| party).collect(),
        timestamp: civil_timestamp(&context.clock),
        tz_offset_minutes: 0,
        seed: seed(&context.seed),
        named_lists: context.settings.named_lists.clone(),
        fallback_name: String::new(),
    };
    let facts = ConditionFacts {
        owner_id: Some(context.owner_id.clone()),
        owner_role_ids: owner.map(|m| m.roles.clone()).unwrap_or_default(),
        member_ids: context.members.iter().map(|m| m.id.clone()).collect(),
        member_role_ids: context
            .members
            .iter()
            .flat_map(|m| m.roles.iter().cloned())
            .collect(),
        owner_playing: owner_game.is_some(),
        owner_live_discord: owner.is_some_and(|m| m.live_discord),
        owner_live_external: owner.is_some_and(|m| m.live_external),
        live_discord_count: count(|m| m.live_discord),
        live_external_count: count(|m| m.live_external),
        games: majority_games(&activities, owner_game, &game_options),
        private: context.private,
    };
    Room {
        context: room_context,
        facts,
    }
}

/// V5: `@@num_live@@` counts members streaming in Discord or externally.
fn is_live(member: &Member) -> bool {
    member.live_discord || member.live_external
}

/// The corpus clock is already guild-local civil time. Pick the first day of
/// that month in 2026 with that weekday, at that hour, as a UTC timestamp.
fn civil_timestamp(clock: &Clock) -> i64 {
    assert_eq!(clock.timezone, "UTC", "corpus clocks are UTC");
    let month = (1..=12u8)
        .map(|m| Month::try_from(m).expect("month"))
        .find(|m| m.to_string() == clock.month)
        .unwrap_or_else(|| panic!("unknown month {}", clock.month));
    let mut date = Date::from_calendar_date(2026, month, 1).expect("date");
    while date.weekday().to_string() != clock.weekday {
        date = date.next_day().expect("next day");
        assert_eq!(date.month(), month, "unknown weekday {}", clock.weekday);
    }
    date.with_hms(clock.hour, 0, 0)
        .expect("hour")
        .assume_utc()
        .unix_timestamp()
}

/// The corpus seed is opaque; map it consistently with 64-bit FNV-1a.
fn seed(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn render_case(case: &Case, rooms: &BTreeMap<String, Room>) -> String {
    let room = &rooms[&case.context];
    render(
        &parse(&case.input),
        &room.context,
        &TemplateExtensions::new(&room.facts),
    )
}

/// Why an invariant case fails, or `None` when every declared constraint holds.
fn invariant_failure(case: &Case, rooms: &BTreeMap<String, Room>) -> Option<String> {
    let Expected::Invariant {
        nonempty,
        max_characters,
        stable_for_same_context,
        allowed_outputs,
        casefold_equals,
    } = &case.expected
    else {
        unreachable!("invariant case");
    };
    let output = render_case(case, rooms);
    let mut failures = Vec::new();
    if *nonempty && output.is_empty() {
        failures.push("empty output".to_string());
    }
    if output.chars().count() > *max_characters {
        failures.push(format!("over {max_characters} characters"));
    }
    if *stable_for_same_context && render_case(case, rooms) != output {
        failures.push("repeat render differs".to_string());
    }
    if let Some(allowed) = allowed_outputs
        .as_ref()
        .filter(|allowed| !allowed.contains(&output))
    {
        failures.push(format!("not one of {allowed:?}"));
    }
    if let Some(target) = casefold_equals {
        // Lowercasing equals Unicode case folding for ASCII targets only.
        assert!(target.is_ascii(), "{}: non-ASCII case-fold target", case.id);
        if output.to_lowercase() != *target {
            failures.push(format!(
                "case-folds to {:?}, not {target:?}",
                output.to_lowercase()
            ));
        }
    }
    (!failures.is_empty())
        .then(|| format!("{}: {} (actual {output:?})", case.id, failures.join("; ")))
}

#[test]
fn voice_template_corpus_matches_engine() {
    let corpus: Corpus = serde_json::from_str(CORPUS).expect("corpus parses");
    assert_eq!(corpus.version, 1, "corpus version");
    let rooms: BTreeMap<String, Room> = corpus
        .contexts
        .iter()
        .map(|(name, context)| (name.clone(), room(context)))
        .collect();

    let mut ids = BTreeSet::new();
    let (mut exact_pass, mut invariant_pass) = (0usize, 0usize);
    let mut deferred: BTreeMap<&str, usize> = BTreeMap::new();
    let mut failures = Vec::new();

    for case in &corpus.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate case ID {}",
            case.id
        );
        assert!(
            rooms.contains_key(&case.context),
            "{}: missing context",
            case.id
        );
        let failure = match &case.expected {
            Expected::Deferred { ambiguity_id } => {
                *deferred.entry(ambiguity_id.as_str()).or_default() += 1;
                continue;
            }
            Expected::Exact { output } => {
                let actual = render_case(case, &rooms);
                (actual != *output)
                    .then(|| format!("{}: expected {output:?}, actual {actual:?}", case.id))
            }
            Expected::Invariant { .. } => invariant_failure(case, &rooms),
        };
        match failure {
            Some(message) => failures.push(message),
            None if matches!(case.expected, Expected::Exact { .. }) => exact_pass += 1,
            None => invariant_pass += 1,
        }
    }

    for group in &corpus.stability_groups {
        let outputs: BTreeSet<String> = group
            .case_ids
            .iter()
            .map(|id| {
                let case = corpus.cases.iter().find(|c| &c.id == id);
                render_case(case.expect("stability case exists"), &rooms)
            })
            .collect();
        if outputs.len() != 1 {
            failures.push(format!(
                "{}: outputs differ across renames: {outputs:?}",
                group.id
            ));
        }
    }

    let deferred_total: usize = deferred.values().sum();
    eprintln!(
        "voice template corpus: {} cases | exact {exact_pass} pass | invariant {invariant_pass} \
         pass | {deferred_total} deferred | {} stability groups",
        corpus.cases.len(),
        corpus.stability_groups.len(),
    );
    eprintln!("deferred by ambiguity: {deferred:?}");

    assert!(
        failures.is_empty(),
        "{} corpus case(s) disagree with the engine:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // Corpus totals come from an untyped read, independent of the loop above.
    let raw: serde_json::Value = serde_json::from_str(CORPUS).expect("corpus parses");
    let raw_cases = raw["cases"].as_array().expect("cases array");
    let total_of = |kind: &str| {
        raw_cases
            .iter()
            .filter(|case| case["expected"]["kind"] == kind)
            .count()
    };
    assert_eq!(exact_pass, total_of("exact"), "exact counts reconcile");
    assert_eq!(
        invariant_pass,
        total_of("invariant"),
        "invariant counts reconcile"
    );
    assert_eq!(
        deferred_total,
        total_of("deferred"),
        "deferred counts reconcile"
    );
    assert_eq!(
        exact_pass + invariant_pass + deferred_total,
        raw_cases.len(),
        "pass + deferred reconcile with the corpus total"
    );
}
