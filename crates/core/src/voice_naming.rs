//! Temporary voice-room name template engine (spec V5, `docs/voice-rooms.md`).
//!
//! Pure, dependency-free parser plus evaluator over [`RoomContext`]. This
//! module knows nothing about Discord types: callers resolve display names,
//! activities and room numbers into a [`RoomContext`] and get back a final
//! channel name that is never empty and never over [`MAX_NAME_LEN`]
//! characters.
//!
//! ## Template syntax (V5)
//!
//! ```text
//! ##                  room number as `#N`
//! $#                  room number, bare
//! $0#, $00#, ...      room number, zero-padded (each extra `0` adds a digit)
//! +#                  room number as a Roman numeral
//! @@nato@@            room number as a NATO word (wraps after 26: `Alpha 2`)
//! @@owner@@           room member's display name (`@@creator@@` is an alias)
//! @@original_creator@@
//! @@num@@             humans in the room
//! @@num_others@@      humans excluding the owner
//! @@num_live@@        members streaming
//! @@limit@@           the user limit (`0` when unlimited)
//! @@slots@@           free places left (blank when unlimited)
//! @@game_name@@       majority game (two-way tie shows both, three-way tie
//!                     shows the no-game label)
//! @@stream_name@@     owner's stream title while live
//! @@num_playing@@     largest party size, else members playing
//! @@party_size@@      party maximum, falling back to the limit
//! @@party_state@@ / @@party_details@@  from the largest party
//! @@weekday@@ / @@month@@ / @@hour@@   guild time zone (default UTC)
//! @@random_emoji@@    seeded emoji pick, stable across renames
//! <<singular/plural>>       singular only with exactly one member
//! <<singular\plural>>       counts members excluding the owner
//! <<singular|plural>>       counts players in the largest party
//! [[a/b/c]]           seeded pick from the options
//! [[list:name]]       seeded pick from a named guild list
//! __resting/in use__  standalone channels: split on the first `/` only,
//!                     resting side when empty, in-use side when occupied
//! ```
//!
//! Evaluation order: conditionals (innermost first) → token substitution →
//! styling → trim → truncate to [`MAX_NAME_LEN`] characters → fallback name
//! when empty.
//!
//! ## V6 extension point
//!
//! Conditionals (`{{cond ?? yes // no}}`) and styling (`""mode:text""`) are
//! parsed into [`Segment::Extension`] nodes but evaluated through the
//! [`ExtensionPolicy`] trait. V5 ships [`PassthroughExtensions`], which
//! leaves them as literal text; the V6 slice implements the trait with real
//! conditional and styling passes without touching the parser or pipeline.

use std::collections::{HashMap, HashSet};
use std::fmt;

/// Maximum channel name length enforced by the evaluator.
pub const MAX_NAME_LEN: usize = 100;

/// Built-in fallback used when the configured fallback name is blank.
pub const DEFAULT_FALLBACK_NAME: &str = "Voice Room";

/// NATO alphabet words for the `@@nato@@` token, index 0 = room number 1.
const NATO_WORDS: [&str; 26] = [
    "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India", "Juliett",
    "Kilo", "Lima", "Mike", "November", "Oscar", "Papa", "Quebec", "Romeo", "Sierra", "Tango",
    "Uniform", "Victor", "Whiskey", "Xray", "Yankee", "Zulu",
];

/// Built-in emoji set for `@@random_emoji@@`, rolled from the room seed.
const RANDOM_EMOJI: [&str; 24] = [
    "🎮", "🎧", "🎤", "🎲", "🎯", "🎨", "🎭", "🎬", "🎸", "🎺", "🎻", "🥁", "🎹", "🏆", "⚔️", "🛡️",
    "🚀", "🌙", "⭐", "🔥", "💎", "🍕", "☕", "🌊",
];

const WEEKDAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// A parsed name template: an ordered list of [`Segment`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template(pub Vec<Segment>);

/// One parsed piece of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Literal text, emitted unchanged. Adjacent literals are always merged,
    /// so two parses of the same source produce identical segment lists.
    Text(String),
    /// A room-number token (`##`, `$#`, `$0#`, `+#`).
    Number(NumberStyle),
    /// A `@@name@@` token; the name is lowercased at parse time.
    /// Unknown names evaluate to an empty string.
    Token(String),
    /// `<<singular/plural>>`, `<<singular\plural>>` or `<<singular|plural>>`.
    /// Branches are sub-templates, split on the first separator only.
    Plural {
        singular: Template,
        plural: Template,
        counter: PluralCounter,
    },
    /// `[[a/b/c]]` seeded choice, or `[[list:name]]` seeded named-list pick.
    Choice(Choice),
    /// `__resting/in use__`, split on the first `/` only.
    Resting { resting: Template, in_use: Template },
    /// `{{...}}` or `""mode:text""`: opaque to V5, evaluated by the
    /// [`ExtensionPolicy`].
    Extension(Extension),
}

/// Room-number rendering styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberStyle {
    /// `##` → `#N`.
    Hash,
    /// `$#`, `$0#`, `$00#`, … → bare or zero-padded to `width` digits.
    Bare { width: usize },
    /// `+#` → Roman numeral.
    Roman,
}

/// Which headcount a `<<singular/plural>>` block tests for "exactly one".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralCounter {
    /// All humans in the room.
    Members,
    /// Humans excluding the owner (`\` separator).
    Others,
    /// Players in the largest rich-presence party (`|` separator).
    Party,
}

/// A `[[...]]` seeded choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// `[[a/b/c]]`: pick one option.
    Options(Vec<Template>),
    /// `[[list:name]]`: pick from a named guild list.
    NamedList(String),
}

/// A V6 conditional or styling node, carried opaquely through V5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extension {
    /// `{{...}}`: full source including the braces.
    Conditional { source: String },
    /// `""modes:body""`: full source plus the split modes and body.
    Styled {
        source: String,
        modes: String,
        body: Template,
    },
}

/// Largest-party snapshot for the party tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartyInfo {
    /// Players in the party.
    pub size: u32,
    /// Party maximum, if advertised.
    pub max: Option<u32>,
    /// Party state text.
    pub state: String,
    /// Party details text.
    pub details: String,
}

/// Options for [`resolve_majority_game`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameOptions {
    /// Alias map: alternative title → canonical title (V7 fills this;
    /// empty in V5).
    pub aliases: HashMap<String, String>,
    /// Force a single game, preferring the owner's.
    pub force_single: bool,
    /// Members with no visible activity swell the leading game's count,
    /// collapsing near-ties toward the plurality leader (alphabetical).
    pub count_idle_toward_majority: bool,
    /// Label shown when no game wins (default `General`).
    pub no_game_label: String,
}

/// Everything the evaluator needs: plain data, no Discord types.
///
/// Display names are resolved by the caller (nicknames land in V7); the game
/// title is the already-resolved majority game (see
/// [`resolve_majority_game`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomContext {
    /// Room number `N` for the numbering tokens.
    pub room_number: u32,
    /// Owner's display name.
    pub owner_name: String,
    /// Original creator's display name.
    pub original_creator_name: String,
    /// Humans currently in the room.
    pub member_count: u32,
    /// Whether the owner is among them (affects `@@num_others@@`).
    pub owner_present: bool,
    /// Members streaming.
    pub live_count: u32,
    /// User limit (`0` = unlimited).
    pub user_limit: u32,
    /// Resolved majority game title (empty = none).
    pub game_name: String,
    /// Owner's stream title while live (empty = not live).
    pub stream_title: String,
    /// Members with any game activity (used when there is no party).
    pub members_playing: u32,
    /// Known rich-presence parties; the unique largest wins, ties → none.
    pub parties: Vec<PartyInfo>,
    /// Unix timestamp for the time tokens.
    pub timestamp: i64,
    /// Guild time-zone offset in minutes east of UTC (default `0` = UTC).
    pub tz_offset_minutes: i32,
    /// Per-room seed stored at creation; random picks never re-roll.
    pub seed: u64,
    /// Named guild lists for `[[list:name]]`.
    pub named_lists: HashMap<String, Vec<String>>,
    /// Fallback name when the rendered name is empty.
    pub fallback_name: String,
}

/// Evaluates [`Segment::Extension`] nodes.
///
/// V5 ships [`PassthroughExtensions`]; the conditionals/styling slice
/// implements this trait without reworking the parser or the pipeline.
pub trait ExtensionPolicy {
    /// Evaluate a `{{...}}` node; `source` is the full node text.
    fn conditional(&self, source: &str, ctx: &RoomContext) -> String;
    /// Evaluate a `""modes:body""` node.
    fn styled(&self, modes: &str, body: &Template, source: &str, ctx: &RoomContext) -> String;
}

/// V5 policy: extension nodes render as their literal source text.
#[derive(Debug, Clone, Copy, Default)]
pub struct PassthroughExtensions;

impl ExtensionPolicy for PassthroughExtensions {
    fn conditional(&self, source: &str, _ctx: &RoomContext) -> String {
        source.to_string()
    }

    fn styled(&self, _modes: &str, _body: &Template, source: &str, _ctx: &RoomContext) -> String {
        source.to_string()
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.rest().starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump_char(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }
}

/// Which delimiters terminate the segment list currently being parsed.
/// Each bracket construct only honours its own closers, so e.g. a `>>`
/// inside a `[[...]]` branch stays literal and degrades gracefully.
#[derive(Clone, Copy)]
struct Stops {
    singles: &'static [char],
    plural: bool,
    choice: bool,
    resting: bool,
}

impl Stops {
    const NONE: Self = Self {
        singles: &[],
        plural: false,
        choice: false,
        resting: false,
    };
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Parse a template; unclosed delimiters become literal text, so this never
/// fails.
#[must_use]
pub fn parse(input: &str) -> Template {
    let mut cursor = Cursor::new(input);
    Template(parse_segments(&mut cursor, Stops::NONE))
}

fn parse_segments(cursor: &mut Cursor, stops: Stops) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    // Push a pending literal, merging is automatic: everything literal flows
    // through the one buffer, so adjacent `Text` segments never occur.
    let flush = |segments: &mut Vec<Segment>, literal: &mut String| {
        if !literal.is_empty() {
            segments.push(Segment::Text(std::mem::take(literal)));
        }
    };
    // A failed construct parse: keep its opener as literal text and continue
    // just past the opener.
    macro_rules! try_construct {
        ($parse:expr, $opener:literal, $segments:expr, $literal:expr) => {
            match $parse {
                Some(seg) => $segments.push(seg),
                None => {
                    $literal.push_str($opener);
                    cursor.pos += $opener.len();
                }
            }
        };
    }

    while cursor.peek_char().is_some() {
        if matches!(cursor.peek_char(), Some(c) if stops.singles.contains(&c)) {
            break;
        }
        let rest = cursor.rest();
        if (stops.plural && rest.starts_with(">>"))
            || (stops.choice && rest.starts_with("]]"))
            || (stops.resting && rest.starts_with("__"))
        {
            break;
        }

        if rest.starts_with("##") {
            flush(&mut segments, &mut literal);
            cursor.pos += 2;
            segments.push(Segment::Number(NumberStyle::Hash));
        } else if let Some(width) = bare_number_width(rest) {
            flush(&mut segments, &mut literal);
            cursor.pos += 1 + width.saturating_sub(1) + 1;
            segments.push(Segment::Number(NumberStyle::Bare { width }));
        } else if rest.starts_with("+#") {
            flush(&mut segments, &mut literal);
            cursor.pos += 2;
            segments.push(Segment::Number(NumberStyle::Roman));
        } else if rest.starts_with('$') || rest.starts_with('+') {
            // Lone `$`/`+` that starts no token: one literal char.
            literal.push(cursor.bump_char().expect("peeked char"));
        } else if rest.starts_with("@@") {
            flush(&mut segments, &mut literal);
            try_construct!(parse_at_token(cursor), "@@", segments, literal);
        } else if rest.starts_with("<<") {
            flush(&mut segments, &mut literal);
            try_construct!(parse_plural(cursor), "<<", segments, literal);
        } else if rest.starts_with("[[") {
            flush(&mut segments, &mut literal);
            try_construct!(parse_choice(cursor), "[[", segments, literal);
        } else if rest.starts_with("__") {
            flush(&mut segments, &mut literal);
            try_construct!(parse_resting(cursor), "__", segments, literal);
        } else if rest.starts_with("{{") {
            flush(&mut segments, &mut literal);
            try_construct!(parse_conditional(cursor), "{{", segments, literal);
        } else if rest.starts_with("\"\"") {
            flush(&mut segments, &mut literal);
            try_construct!(parse_styled(cursor), "\"\"", segments, literal);
        } else {
            literal.push(cursor.bump_char().expect("peeked char"));
        }
    }
    flush(&mut segments, &mut literal);
    segments
}

/// Width of a `$#` / `$0#` / `$00#` … token (1 = bare), or `None` when the
/// rest is not `$` followed by zeros and a `#` terminator.
fn bare_number_width(rest: &str) -> Option<usize> {
    let after_dollar = rest.strip_prefix('$')?;
    let zeros = after_dollar.chars().take_while(|c| *c == '0').count();
    if zeros == 0 && !after_dollar.starts_with('#') {
        return None;
    }
    after_dollar[zeros..].strip_prefix('#').map(|_| zeros + 1)
}

/// Try to parse `@@name@@` (cursor at the opening `@@`). Returns the
/// token segment with a lowercased name, leaving the cursor untouched on
/// failure.
fn parse_at_token(cursor: &mut Cursor) -> Option<Segment> {
    let after_open = cursor.rest().strip_prefix("@@")?;
    let mut name = String::new();
    for c in after_open.chars() {
        if is_token_char(c) {
            name.push(c);
        } else {
            break;
        }
    }
    if name.is_empty() {
        return None;
    }
    if after_open[name.len()..].starts_with("@@") {
        cursor.pos += 2 + name.len() + 2;
        Some(Segment::Token(name.to_lowercase()))
    } else {
        None
    }
}

/// Try to parse `<<singular/plural>>` (cursor at the opening `<<`).
/// Splits on the first `/`, `\` or `|`; unclosed input rewinds and yields
/// `None`.
fn parse_plural(cursor: &mut Cursor) -> Option<Segment> {
    let start = cursor.pos;
    cursor.pos += 2;
    let singular = parse_segments(
        cursor,
        Stops {
            singles: &['/', '\\', '|'],
            plural: true,
            ..Stops::NONE
        },
    );
    // A `?` here would skip the rewind below and strand the cursor past
    // the opener, so handle end-of-input explicitly.
    let Some(sep) = cursor.bump_char() else {
        cursor.pos = start;
        return None;
    };
    if sep == '>' {
        // `>>` closer with no separator: singular-only plural.
        if cursor.rest().starts_with('>') {
            cursor.pos += 1;
            return Some(Segment::Plural {
                singular: Template(singular),
                plural: Template(Vec::new()),
                counter: PluralCounter::Members,
            });
        }
        cursor.pos = start;
        return None;
    }
    let counter = match sep {
        '/' => PluralCounter::Members,
        '\\' => PluralCounter::Others,
        '|' => PluralCounter::Party,
        _ => {
            cursor.pos = start;
            return None;
        }
    };
    let plural = parse_segments(
        cursor,
        Stops {
            plural: true,
            ..Stops::NONE
        },
    );
    if cursor.eat(">>") {
        Some(Segment::Plural {
            singular: Template(singular),
            plural: Template(plural),
            counter,
        })
    } else {
        cursor.pos = start;
        None
    }
}

/// Try to parse `[[a/b/c]]` or `[[list:name]]` (cursor at the opening).
/// Unclosed input rewinds and yields `None`.
fn parse_choice(cursor: &mut Cursor) -> Option<Segment> {
    let start = cursor.pos;
    cursor.pos += 2;
    let branch_stops = Stops {
        singles: &['/'],
        choice: true,
        ..Stops::NONE
    };
    let mut options = Vec::new();
    loop {
        let branch = parse_segments(cursor, branch_stops);
        if cursor.eat("]]") {
            options.push(Template(branch));
            break;
        }
        if !cursor.eat("/") {
            cursor.pos = start;
            return None;
        }
        options.push(Template(branch));
    }
    if options.len() == 1 {
        let body = options[0].to_string();
        if let Some(name) = body.strip_prefix("list:") {
            let name = name.trim().to_string();
            if !name.is_empty() {
                return Some(Segment::Choice(Choice::NamedList(name)));
            }
        }
    }
    Some(Segment::Choice(Choice::Options(options)))
}

/// Try to parse `__resting/in use__` (cursor at the opening). Splits on the
/// first `/` only; unclosed input rewinds and yields `None`.
fn parse_resting(cursor: &mut Cursor) -> Option<Segment> {
    let start = cursor.pos;
    cursor.pos += 2;
    let first = parse_segments(
        cursor,
        Stops {
            singles: &['/'],
            resting: true,
            ..Stops::NONE
        },
    );
    if cursor.eat("__") {
        // No `/`: both sides render the same content.
        let template = Template(first);
        return Some(Segment::Resting {
            resting: template.clone(),
            in_use: template,
        });
    }
    if !cursor.eat("/") {
        cursor.pos = start;
        return None;
    }
    let second = parse_segments(
        cursor,
        Stops {
            resting: true,
            ..Stops::NONE
        },
    );
    if cursor.eat("__") {
        Some(Segment::Resting {
            resting: Template(first),
            in_use: Template(second),
        })
    } else {
        cursor.pos = start;
        None
    }
}

/// Try to parse `{{...}}` with balanced braces (cursor at the opening).
/// Unclosed input rewinds and yields `None`.
fn parse_conditional(cursor: &mut Cursor) -> Option<Segment> {
    let start = cursor.pos;
    cursor.pos += 2;
    let mut depth = 1usize;
    while let Some(c) = cursor.bump_char() {
        if c == '{' && cursor.rest().starts_with('{') {
            cursor.pos += 1;
            depth += 1;
        } else if c == '}' && cursor.rest().starts_with('}') {
            cursor.pos += 1;
            depth -= 1;
            if depth == 0 {
                return Some(Segment::Extension(Extension::Conditional {
                    source: cursor.src[start..cursor.pos].to_string(),
                }));
            }
        }
    }
    cursor.pos = start;
    None
}

/// Try to parse `""modes:body""` (cursor at the opening). Unclosed input
/// rewinds and yields `None`.
fn parse_styled(cursor: &mut Cursor) -> Option<Segment> {
    let start = cursor.pos;
    cursor.pos += 2;
    let modes_start = cursor.pos;
    let mut found_colon = false;
    while let Some(c) = cursor.peek_char() {
        if c == ':' {
            found_colon = true;
            cursor.pos += 1;
            break;
        }
        if c == '"' || c == '\n' {
            break;
        }
        cursor.pos += c.len_utf8();
    }
    if !found_colon {
        cursor.pos = start;
        return None;
    }
    let modes = cursor.src[modes_start..cursor.pos - 1].to_string();
    let body_start = cursor.pos;
    // First `""` wins as the closer; nested styling semantics belong to V6.
    let mut body_end = None;
    while cursor.pos < cursor.src.len() {
        if cursor.rest().starts_with("\"\"") {
            body_end = Some(cursor.pos);
            break;
        }
        cursor.bump_char();
    }
    let Some(end) = body_end else {
        cursor.pos = start;
        return None;
    };
    let body = parse(&cursor.src[body_start..end]);
    cursor.pos = end + 2;
    Some(Segment::Extension(Extension::Styled {
        source: cursor.src[start..cursor.pos].to_string(),
        modes,
        body,
    }))
}

// ---------------------------------------------------------------------------
// Display (canonical rendering, used for round-trip property tests)
// ---------------------------------------------------------------------------

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&display_template(&self.0))
    }
}

fn display_template(segments: &[Segment]) -> String {
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Text(t) => out.push_str(t),
            Segment::Number(NumberStyle::Hash) => out.push_str("##"),
            Segment::Number(NumberStyle::Bare { width }) => {
                out.push('$');
                out.push_str(&"0".repeat(width.saturating_sub(1)));
                out.push('#');
            }
            Segment::Number(NumberStyle::Roman) => out.push_str("+#"),
            Segment::Token(name) => {
                out.push_str("@@");
                out.push_str(name);
                out.push_str("@@");
            }
            Segment::Plural {
                singular,
                plural,
                counter,
            } => {
                let sep = match counter {
                    PluralCounter::Members => '/',
                    PluralCounter::Others => '\\',
                    PluralCounter::Party => '|',
                };
                out.push_str("<<");
                out.push_str(&display_template(&singular.0));
                out.push(sep);
                out.push_str(&display_template(&plural.0));
                out.push_str(">>");
            }
            Segment::Choice(Choice::Options(options)) => {
                out.push_str("[[");
                let parts: Vec<String> = options.iter().map(|o| display_template(&o.0)).collect();
                out.push_str(&parts.join("/"));
                out.push_str("]]");
            }
            Segment::Choice(Choice::NamedList(name)) => {
                out.push_str("[[list:");
                out.push_str(name);
                out.push_str("]]");
            }
            Segment::Resting { resting, in_use } => {
                out.push_str("__");
                out.push_str(&display_template(&resting.0));
                // Canonical form always carries the separator.
                out.push('/');
                out.push_str(&display_template(&in_use.0));
                out.push_str("__");
            }
            Segment::Extension(Extension::Conditional { source })
            | Segment::Extension(Extension::Styled { source, .. }) => out.push_str(source),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Deterministic SplitMix64 dice: random picks derive from the room seed
/// plus their position in the template, so renames never re-roll.
struct Dice {
    state: u64,
}

impl Dice {
    fn new(seed: u64, index: u64) -> Self {
        Self {
            state: seed.wrapping_add(index.wrapping_mul(0x9E3779B97F4A7C15)),
        }
    }

    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64).max(1)) as usize
    }
}

/// Render a parsed template against a room context.
///
/// Pipeline tail: trim → truncate to [`MAX_NAME_LEN`] characters →
/// fallback name when empty.
pub fn render<E: ExtensionPolicy>(template: &Template, ctx: &RoomContext, ext: &E) -> String {
    let mut dice_index = 0u64;
    let rendered = render_segments(&template.0, ctx, ext, &mut dice_index);
    // Pipeline tail: trim → truncate → fallback.
    let mut out = truncate_chars(rendered.trim(), MAX_NAME_LEN);
    out = out.trim_end().to_string();
    if out.is_empty() {
        let fallback = ctx.fallback_name.trim();
        if fallback.is_empty() {
            out = DEFAULT_FALLBACK_NAME.to_string();
        } else {
            out = truncate_chars(fallback, MAX_NAME_LEN);
            out = out.trim_end().to_string();
            if out.is_empty() {
                out = DEFAULT_FALLBACK_NAME.to_string();
            }
        }
    }
    out
}

/// Parse and render in one step with the V5 passthrough extension policy.
pub fn render_str(input: &str, ctx: &RoomContext) -> String {
    render(&parse(input), ctx, &PassthroughExtensions)
}

fn render_segments<E: ExtensionPolicy>(
    segments: &[Segment],
    ctx: &RoomContext,
    ext: &E,
    dice_index: &mut u64,
) -> String {
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Text(t) => out.push_str(t),
            Segment::Number(style) => out.push_str(&render_number(*style, ctx.room_number)),
            Segment::Token(name) => out.push_str(&render_token(name, ctx)),
            Segment::Plural {
                singular,
                plural,
                counter,
            } => {
                let count = match counter {
                    PluralCounter::Members => ctx.member_count,
                    PluralCounter::Others => others_count(ctx),
                    PluralCounter::Party => largest_party(ctx).map_or(0, |p| p.size),
                };
                out.push_str(&render_selected_branch(
                    [singular, plural],
                    usize::from(count != 1),
                    ctx,
                    ext,
                    dice_index,
                ));
            }
            Segment::Choice(choice) => {
                let index = *dice_index;
                *dice_index += 1;
                match choice {
                    Choice::Options(options) => {
                        if !options.is_empty() {
                            let mut dice = Dice::new(ctx.seed, index);
                            let pick = dice.below(options.len());
                            out.push_str(&render_selected_branch(
                                options.iter(),
                                pick,
                                ctx,
                                ext,
                                dice_index,
                            ));
                        }
                    }
                    Choice::NamedList(name) => {
                        if let Some(list) = ctx.named_lists.get(name) {
                            if !list.is_empty() {
                                let mut dice = Dice::new(ctx.seed, index);
                                out.push_str(&list[dice.below(list.len())]);
                            }
                        }
                    }
                }
            }
            Segment::Resting { resting, in_use } => {
                out.push_str(&render_selected_branch(
                    [resting, in_use],
                    usize::from(ctx.member_count > 0),
                    ctx,
                    ext,
                    dice_index,
                ));
            }
            Segment::Extension(Extension::Conditional { source }) => {
                out.push_str(&ext.conditional(source, ctx));
            }
            Segment::Extension(Extension::Styled {
                source,
                modes,
                body,
            }) => {
                out.push_str(&ext.styled(modes, body, source, ctx));
            }
        }
    }
    out
}

// Reserve random positions in every branch, including inactive branches.
// Otherwise a headcount change can re-roll choices later in the template.
fn random_choice_count(template: &Template) -> u64 {
    template
        .0
        .iter()
        .map(|segment| match segment {
            Segment::Choice(Choice::Options(options)) => {
                1 + options.iter().map(random_choice_count).sum::<u64>()
            }
            Segment::Choice(Choice::NamedList(_)) => 1,
            Segment::Plural {
                singular, plural, ..
            } => random_choice_count(singular) + random_choice_count(plural),
            Segment::Resting { resting, in_use } => {
                random_choice_count(resting) + random_choice_count(in_use)
            }
            _ => 0,
        })
        .sum()
}

fn render_selected_branch<'a, E: ExtensionPolicy>(
    branches: impl IntoIterator<Item = &'a Template>,
    selected: usize,
    ctx: &RoomContext,
    ext: &E,
    dice_index: &mut u64,
) -> String {
    let mut out = String::new();
    for (index, branch) in branches.into_iter().enumerate() {
        if index == selected {
            out = render_segments(&branch.0, ctx, ext, dice_index);
        } else {
            *dice_index += random_choice_count(branch);
        }
    }
    out
}

fn render_number(style: NumberStyle, n: u32) -> String {
    match style {
        NumberStyle::Hash => format!("#{n}"),
        NumberStyle::Bare { width } => {
            if width <= 1 {
                format!("{n}")
            } else {
                format!("{n:0>width$}", width = width)
            }
        }
        NumberStyle::Roman => roman(n),
    }
}

fn render_token(name: &str, ctx: &RoomContext) -> String {
    match name {
        "owner" | "creator" => ctx.owner_name.clone(),
        "original_creator" => ctx.original_creator_name.clone(),
        "num" => ctx.member_count.to_string(),
        "num_others" => others_count(ctx).to_string(),
        "num_live" => ctx.live_count.to_string(),
        "limit" => ctx.user_limit.to_string(),
        "slots" => {
            if ctx.user_limit == 0 {
                String::new()
            } else {
                ctx.user_limit.saturating_sub(ctx.member_count).to_string()
            }
        }
        "game_name" => ctx.game_name.clone(),
        "stream_name" => ctx.stream_title.clone(),
        "num_playing" => largest_party(ctx)
            .map_or(ctx.members_playing, |p| p.size)
            .to_string(),
        "party_size" => largest_party(ctx)
            .and_then(|p| p.max)
            .unwrap_or(ctx.user_limit)
            .to_string(),
        "party_state" => largest_party(ctx).map_or(String::new(), |p| p.state.clone()),
        "party_details" => largest_party(ctx).map_or(String::new(), |p| p.details.clone()),
        "weekday" | "month" | "hour" => render_time_token(name, ctx),
        "random_emoji" => {
            let mut dice = Dice::new(ctx.seed, u64::MAX);
            RANDOM_EMOJI[dice.below(RANDOM_EMOJI.len())].to_string()
        }
        "nato" => nato(ctx.room_number),
        _ => String::new(),
    }
}

fn others_count(ctx: &RoomContext) -> u32 {
    ctx.member_count
        .saturating_sub(u32::from(ctx.owner_present))
}

/// The unique largest party, or `None` on a tie (spec: empty on a tie).
fn largest_party(ctx: &RoomContext) -> Option<&PartyInfo> {
    let top = ctx.parties.iter().map(|p| p.size).max()?;
    let mut best: Option<&PartyInfo> = None;
    for party in &ctx.parties {
        if party.size == top {
            if best.is_some() {
                return None;
            }
            best = Some(party);
        }
    }
    best
}

fn render_time_token(name: &str, ctx: &RoomContext) -> String {
    let (weekday, month, hour) = civil_parts(ctx.timestamp, ctx.tz_offset_minutes);
    match name {
        "weekday" => WEEKDAY_NAMES[weekday].to_string(),
        "month" => MONTH_NAMES[month].to_string(),
        _ => hour.to_string(),
    }
}

/// Split a timestamp plus zone offset into weekday (Mon 0 – Sun 6),
/// month (0-based) and hour (0–23) using proleptic Gregorian math.
fn civil_parts(timestamp: i64, offset_minutes: i32) -> (usize, usize, u32) {
    let adjusted = timestamp + i64::from(offset_minutes) * 60;
    let days = adjusted.div_euclid(86_400);
    let hour = (adjusted.rem_euclid(86_400) / 3600) as u32;
    // 1970-01-01 was a Thursday; Monday-based index.
    let weekday = ((days + 3).rem_euclid(7)) as usize;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let month = (m - 1) as usize;
    (weekday, month, hour)
}

/// `1` → `I`, `4` → `IV`, `2026` → `MMXXVI`; `0` renders as `N`.
fn roman(n: u32) -> String {
    if n == 0 {
        return "N".to_string();
    }
    const TABLE: [(u32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    let mut rest = n;
    for (value, glyph) in TABLE {
        while rest >= value {
            out.push_str(glyph);
            rest -= value;
        }
    }
    out
}

/// `1` → `Alpha`; after 26 the words wrap and a cycle number is appended
/// (`27` → `Alpha 2`, `52` → `Zulu 2`).
fn nato(n: u32) -> String {
    let word = NATO_WORDS[((n.max(1) - 1) % 26) as usize];
    if (1..=26).contains(&n) {
        word.to_string()
    } else {
        format!("{word} {}", (n.max(1) - 1) / 26 + 1)
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let end = s.char_indices().nth(max_chars).map_or(s.len(), |(i, _)| i);
    s[..end].to_string()
}

// ---------------------------------------------------------------------------
// Room-number allocation and majority-game resolution
// ---------------------------------------------------------------------------

/// Lowest free room number at or above `start`, given the numbers already
/// in use. The caller passes the creator channel's numbers, or the
/// category's numbers when grouping is on.
#[must_use]
pub fn allocate_room_number(used: &[u32], start: u32) -> u32 {
    let taken: HashSet<u32> = used.iter().copied().collect();
    let mut candidate = start;
    while taken.contains(&candidate) {
        candidate = candidate.saturating_add(1);
        if candidate == u32::MAX {
            break;
        }
    }
    candidate
}

/// Resolve the majority game title from one entry per member (`None` = no
/// visible activity).
///
/// Rules: a two-way tie shows both names (`"A & B"`, alphabetical); three or
/// more tied shows the no-game label; [`GameOptions::force_single`] picks one
/// game preferring `owner_game`, then the highest count, then the alphabet.
#[must_use]
pub fn resolve_majority_game(
    activities: &[Option<String>],
    owner_game: Option<&str>,
    options: &GameOptions,
) -> String {
    let no_game = if options.no_game_label.is_empty() {
        "General".to_string()
    } else {
        options.no_game_label.clone()
    };
    let canonical = |title: &str| -> String {
        options
            .aliases
            .get(title)
            .cloned()
            .unwrap_or_else(|| title.to_string())
    };

    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut idle = 0usize;
    for activity in activities {
        match activity {
            Some(title) => {
                *counts.entry(canonical(title)).or_insert(0) += 1;
            }
            None => idle += 1,
        }
    }
    if counts.is_empty() {
        return no_game;
    }
    if options.force_single {
        if let Some(owner) = owner_game {
            let owned = canonical(owner);
            if counts.contains_key(&owned) {
                return owned;
            }
        }
        let mut ranked: Vec<&String> = counts.keys().collect();
        ranked.sort_by(|a, b| counts[*a].cmp(&counts[*b]).reverse().then_with(|| a.cmp(b)));
        return ranked[0].clone();
    }

    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if options.count_idle_toward_majority && idle > 0 {
        ranked[0].1 += idle;
    }
    let top = ranked[0].1;
    let leaders: Vec<&String> = ranked
        .iter()
        .filter(|(_, count)| *count == top)
        .map(|(name, _)| name)
        .collect();
    match leaders.len() {
        1 => leaders[0].clone(),
        2 => format!("{} & {}", leaders[0], leaders[1]),
        _ => no_game,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RoomContext {
        RoomContext {
            room_number: 3,
            owner_name: "Ava".to_string(),
            original_creator_name: "Ava".to_string(),
            member_count: 4,
            owner_present: true,
            live_count: 1,
            user_limit: 10,
            game_name: "Apex".to_string(),
            stream_title: String::new(),
            members_playing: 3,
            parties: Vec::new(),
            timestamp: 1_790_683_200, // 2026-09-29 12:00:00 UTC (a Tuesday).
            tz_offset_minutes: 0,
            seed: 42,
            named_lists: HashMap::new(),
            fallback_name: "Lounge".to_string(),
        }
    }

    // -- golden corpus --------------------------------------------------------

    #[test]
    fn golden_spec_examples() {
        let c = ctx();
        // `@@game_name@@ ##` → `Apex #3`.
        assert_eq!(render_str("@@game_name@@ ##", &c), "Apex #3");
        // `@@nato@@ · @@num@@ <<person/people>>` → `Charlie · 4 people`.
        assert_eq!(
            render_str("@@nato@@ · @@num@@ <<person/people>>", &c),
            "Charlie · 4 people"
        );
    }

    #[test]
    fn golden_table() {
        let mut c = ctx();
        c.named_lists.insert(
            "maps".to_string(),
            vec!["Kings".to_string(), "Edge".to_string()],
        );
        for (template, expected) in [
            ("##", "#3"),
            ("$#", "3"),
            ("$0#", "03"),
            ("$00#", "003"),
            ("+#", "III"),
            ("@@nato@@", "Charlie"),
            ("@@owner@@", "Ava"),
            ("@@creator@@", "Ava"),
            ("@@original_creator@@", "Ava"),
            ("@@num@@", "4"),
            ("@@num_others@@", "3"),
            ("@@num_live@@", "1"),
            ("@@limit@@", "10"),
            ("@@slots@@", "6"),
            ("@@weekday@@", "Tuesday"),
            ("@@month@@", "September"),
            ("@@hour@@", "12"),
            ("<<person/people>>", "people"),
            ("<<member\\members>>", "members"),
            ("__Empty/Full__", "Full"),
            ("plain name", "plain name"),
            ("@@unknown_token@@", "Lounge"),
        ] {
            assert_eq!(render_str(template, &c), expected, "template {template:?}");
        }
        // Seed-dependent outcomes: pin determinism, not the value.
        for template in ["[[x/y]]", "[[list:maps]]"] {
            let first = render_str(template, &c);
            assert!(!first.is_empty(), "template {template:?}");
            assert_eq!(first, render_str(template, &c));
        }
    }

    #[test]
    fn default_template_renders() {
        let c = ctx();
        let template = "@@random_emoji@@ @@owner@@'s [[den/crew/lair/hangout/base/club]]";
        let name = render_str(template, &c);
        assert!(name.contains("Ava's "), "unexpected {name:?}");
        assert!(name.chars().count() <= MAX_NAME_LEN);
        // Stable across renames: same template + seed, changed headcount.
        let mut renamed = c.clone();
        renamed.member_count = 7;
        assert_eq!(name, render_str(template, &renamed));
    }

    #[test]
    fn random_picks_stay_stable_when_branches_change() {
        for seed in 0..100 {
            let mut c = ctx();
            c.seed = seed;
            for template in [
                "<<[[one/two]]/many>> · [[a/b/c/d/e/f/g]]",
                "__[[rest/idle]]/in use__ · [[a/b/c/d/e/f/g]]",
                "<<one/[[many/several]]>> · [[a/b/c/d/e/f/g]]",
            ] {
                c.member_count = 0;
                let empty = render_str(template, &c);
                c.member_count = 1;
                let solo = render_str(template, &c);
                c.member_count = 4;
                let occupied = render_str(template, &c);
                let pick = |name: &str| name.rsplit(" · ").next().unwrap().to_string();
                assert_eq!(pick(&empty), pick(&solo), "seed {seed}, {template}");
                assert_eq!(pick(&solo), pick(&occupied), "seed {seed}, {template}");
            }
        }
    }

    // -- numbering ------------------------------------------------------------

    #[test]
    fn numbering_styles() {
        let mut c = ctx();
        for (n, hash, bare, padded, roman_n, nato_n) in [
            (1u32, "#1", "1", "01", "I", "Alpha"),
            (4, "#4", "4", "04", "IV", "Delta"),
            (9, "#9", "9", "09", "IX", "India"),
            (14, "#14", "14", "14", "XIV", "November"),
            (26, "#26", "26", "26", "XXVI", "Zulu"),
            (27, "#27", "27", "27", "XXVII", "Alpha 2"),
            (52, "#52", "52", "52", "LII", "Zulu 2"),
            (53, "#53", "53", "53", "LIII", "Alpha 3"),
        ] {
            c.room_number = n;
            assert_eq!(render_str("##", &c), hash, "n={n}");
            assert_eq!(render_str("$#", &c), bare, "n={n}");
            assert_eq!(render_str("$0#", &c), padded, "n={n}");
            assert_eq!(render_str("+#", &c), roman_n, "n={n}");
            assert_eq!(render_str("@@nato@@", &c), nato_n, "n={n}");
        }
    }

    #[test]
    fn allocate_lowest_free_number() {
        assert_eq!(allocate_room_number(&[], 1), 1);
        assert_eq!(allocate_room_number(&[1, 2, 4], 1), 3);
        assert_eq!(allocate_room_number(&[2, 3], 1), 1);
        assert_eq!(allocate_room_number(&[5, 6], 5), 7);
        assert_eq!(allocate_room_number(&[1, 1, 2], 1), 3);
    }

    // -- plurals, slots, resting ----------------------------------------------

    #[test]
    fn plural_counters() {
        let mut c = ctx();
        c.member_count = 1;
        assert_eq!(render_str("<<person/people>>", &c), "person");
        c.member_count = 0;
        assert_eq!(render_str("<<person/people>>", &c), "people");
        // `\` counts excluding the owner (owner present: 1→0, 2→1).
        c.member_count = 1;
        assert_eq!(render_str("<<friend\\friends>>", &c), "friends");
        c.member_count = 2;
        assert_eq!(render_str("<<friend\\friends>>", &c), "friend");
        // `|` counts the largest party.
        c.parties = vec![PartyInfo {
            size: 1,
            max: None,
            state: String::new(),
            details: String::new(),
        }];
        assert_eq!(render_str("<<player|players>>", &c), "player");
        c.parties[0].size = 3;
        assert_eq!(render_str("<<player|players>>", &c), "players");
    }

    #[test]
    fn slots_blank_when_unlimited_and_clamped() {
        let mut c = ctx();
        c.user_limit = 0;
        assert_eq!(render_str("[@@slots@@]", &c), "[]");
        c.user_limit = 2;
        c.member_count = 5;
        assert_eq!(render_str("[@@slots@@]", &c), "[0]");
    }

    #[test]
    fn resting_split_on_first_slash_only() {
        let c = ctx();
        assert_eq!(render_str("__Rest/a/b__", &c), "a/b");
        let mut empty = ctx();
        empty.member_count = 0;
        assert_eq!(render_str("__Rest/a/b__", &empty), "Rest");
    }

    // -- game resolution -------------------------------------------------------

    #[test]
    fn majority_game_rules() {
        let opts = GameOptions::default();
        let games = |names: &[&str]| -> Vec<Option<String>> {
            names.iter().map(|n| Some(n.to_string())).collect()
        };
        assert_eq!(
            resolve_majority_game(&games(&["Apex", "Apex", "Valorant"]), None, &opts),
            "Apex"
        );
        // Two-way tie shows both, alphabetical.
        assert_eq!(
            resolve_majority_game(&games(&["Valorant", "Apex"]), None, &opts),
            "Apex & Valorant"
        );
        // Three-way tie → no-game label.
        assert_eq!(
            resolve_majority_game(&games(&["A", "B", "C"]), None, &opts),
            "General"
        );
        // No activity → no-game label.
        assert_eq!(resolve_majority_game(&[None, None], None, &opts), "General");
        // force_single prefers the owner's game.
        let forced = GameOptions {
            force_single: true,
            ..GameOptions::default()
        };
        assert_eq!(
            resolve_majority_game(
                &games(&["Apex", "Apex", "Valorant"]),
                Some("Valorant"),
                &forced
            ),
            "Valorant"
        );
        // Aliases canonicalise before counting.
        let aliased = GameOptions {
            aliases: HashMap::from([("APEX".to_string(), "Apex".to_string())]),
            ..GameOptions::default()
        };
        assert_eq!(
            resolve_majority_game(&games(&["APEX", "Apex"]), None, &aliased),
            "Apex"
        );
        // Idle members swell the leading game, collapsing a two-way tie.
        let idle_counts = GameOptions {
            count_idle_toward_majority: true,
            ..GameOptions::default()
        };
        let mut tied: Vec<Option<String>> = games(&["Valorant", "Apex"]);
        tied.push(None);
        assert_eq!(resolve_majority_game(&tied, None, &idle_counts), "Apex");
    }

    // -- parser edge cases -----------------------------------------------------

    #[test]
    fn parser_edge_cases() {
        let c = ctx();
        // Unclosed delimiters and lone sigils stay literal.
        for raw in [
            "<<a/b", "[[a/b", "__a/b", "{{a", "\"\"a:b", "@@owner", "@@ @@", "$99", "$0x", "+",
            "#", "$", "@", "<", "[", "_", "{", "\"",
        ] {
            assert_eq!(render_str(raw, &c), raw, "input {raw:?}");
        }
        // Token names are case-insensitive.
        assert_eq!(render_str("@@OWNER@@", &c), "Ava");
        // Unknown tokens vanish (fallback covers a fully-empty name).
        assert_eq!(render_str("@@bogus@@", &c), "Lounge");
        // Empty template → fallback; blank fallback → built-in.
        assert_eq!(render_str("   ", &c), "Lounge");
        let mut no_fallback = ctx();
        no_fallback.fallback_name = String::new();
        assert_eq!(render_str("@@bogus@@", &no_fallback), "Voice Room");
        // V5 passthrough leaves extension syntax literal.
        assert_eq!(
            render_str("{{PLAYING ?? live // idle}}", &c),
            "{{PLAYING ?? live // idle}}"
        );
        assert_eq!(render_str("\"\"upper:hi\"\"", &c), "\"\"upper:hi\"\"");
        // Named list missing → empty → fallback.
        assert_eq!(render_str("[[list:nope]]", &c), "Lounge");
    }

    #[test]
    fn output_bounds() {
        let c = ctx();
        // Long literals truncate to exactly MAX_NAME_LEN on a char boundary.
        let long = "x".repeat(500);
        let out = render_str(&long, &c);
        assert_eq!(out.chars().count(), MAX_NAME_LEN);
        // Multibyte truncation never splits a character.
        let emoji = "🎮".repeat(200);
        let out = render_str(&emoji, &c);
        assert_eq!(out.chars().count(), MAX_NAME_LEN);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn time_tokens_with_offset() {
        let mut c = ctx(); // 2026-09-29 12:00 UTC, Tuesday, September.
        assert_eq!(render_str("@@hour@@", &c), "12");
        c.tz_offset_minutes = 180; // UTC+3 → 15:00.
        assert_eq!(render_str("@@hour@@", &c), "15");
        c.tz_offset_minutes = -720; // UTC-12 → 00:00 same day.
        assert_eq!(render_str("@@hour@@", &c), "0");
        c.tz_offset_minutes = -780; // UTC-13 → previous day, Monday 23:00.
        assert_eq!(render_str("@@weekday@@", &c), "Monday");
        assert_eq!(render_str("@@hour@@", &c), "23");
    }

    // -- hand-rolled property tests (seeded, deterministic) ---------------------

    /// Tiny deterministic generator so the "property" runs are reproducible
    /// without extra dev-dependencies.
    struct Gen {
        state: u64,
    }

    impl Gen {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next(&mut self) -> u64 {
            self.state = self
                .state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.state >> 33
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % (n as u64).max(1)) as usize
        }

        fn ctx(&mut self) -> RoomContext {
            let mut c = ctx();
            c.room_number = self.below(60) as u32;
            c.member_count = self.below(12) as u32;
            c.owner_present = self.below(2) == 0;
            c.user_limit = [0, 2, 5, 10][self.below(4)];
            c.seed = self.next();
            c
        }
    }

    const FUZZ_ALPHABET: &[&str] = &[
        "a", "Z", " ", "@", "#", "$", "0", "+", "<", ">", "/", "\\", "|", "[", "]", "_", "{", "}",
        "\"", ":", "?", ".", "🎮", "é",
    ];

    fn fuzz_template(gen: &mut Gen, max_pieces: usize) -> String {
        let mut out = String::new();
        for _ in 0..gen.below(max_pieces) {
            // Bias toward delimiter-heavy strings to stress the parser.
            out.push_str(FUZZ_ALPHABET[gen.below(FUZZ_ALPHABET.len())]);
        }
        out
    }

    #[test]
    fn prop_parser_never_panics_and_output_bounded() {
        let mut gen = Gen::new(0xC10C);
        for _ in 0..20_000 {
            let input = fuzz_template(&mut gen, 24);
            let template = parse(&input);
            let c = gen.ctx();
            let out = render(&template, &c, &PassthroughExtensions);
            assert!(!out.is_empty(), "empty output for {input:?}");
            assert!(
                out.chars().count() <= MAX_NAME_LEN,
                "overlong output for {input:?}"
            );
        }
    }

    #[test]
    fn prop_parse_display_round_trip() {
        let mut gen = Gen::new(0x5EED);
        for _ in 0..20_000 {
            let input = fuzz_template(&mut gen, 24);
            let once = parse(&input);
            let text = once.to_string();
            let twice = parse(&text);
            assert_eq!(once, twice, "unstable round-trip for {input:?}");
        }
    }

    #[test]
    fn prop_render_deterministic() {
        let mut gen = Gen::new(0xDE7);
        for _ in 0..5_000 {
            let input = fuzz_template(&mut gen, 24);
            let template = parse(&input);
            let c = gen.ctx();
            let a = render(&template, &c, &PassthroughExtensions);
            let b = render(&template, &c, &PassthroughExtensions);
            assert_eq!(a, b, "nondeterministic render for {input:?}");
        }
    }

    #[test]
    fn prop_literals_pass_through() {
        // Templates without any opener render unchanged (modulo
        // trim/truncate/fallback).
        let mut gen = Gen::new(0x11A7);
        for _ in 0..5_000 {
            let mut input = String::new();
            for _ in 0..gen.below(20) {
                input.push_str(["a", "Z", " ", "0", ":", "?", ".", "é"][gen.below(8)]);
            }
            let mut expected = truncate_chars(input.trim(), MAX_NAME_LEN);
            expected = expected.trim_end().to_string();
            if expected.is_empty() {
                expected = "Lounge".to_string();
            }
            assert_eq!(render_str(&input, &ctx()), expected, "input {input:?}");
        }
    }
}
