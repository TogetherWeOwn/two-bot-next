//! Pure V6 conditional evaluation for voice-room name templates, written from
//! `docs/voice-rooms.md` §V6 only.
//!
//! A `{{cond ?? yes // no}}` node is split into a typed [`Condition`] and its
//! branch templates, the condition is decided from caller-supplied
//! [`ConditionFacts`] plus the [`RoomContext`] the tokens render from, and the
//! selected branch is rendered through
//! [`Evaluation::evaluate_selected_branch`], so random picks keep their
//! positions when the condition flips. This module performs no I/O and holds
//! no Discord or store types: resolving member roles, activities and streams
//! into [`ConditionFacts`] belongs to the parent runtime.
//!
//! [`Conditions`] is an [`ExtensionPolicy`] on its own (styling stays
//! literal, as with [`PassthroughExtensions`]). A parent V6 policy that also
//! styles text calls [`Conditions::evaluate`] from its own `conditional`.

use std::cell::Cell;

use crate::voice_naming::{
    parse, ChannelKind, Evaluation, Extension, ExtensionPolicy, PartyInfo, PassthroughExtensions,
    RoomContext, Segment, Template,
};

/// Deepest conditional nesting evaluated, counting conditionals inside
/// conditions and branches. Deeper nodes render as their literal source.
pub const MAX_CONDITION_NESTING: usize = 16;

/// Longest condition text, after nested conditions resolve, that is parsed.
/// Longer conditions are [`Condition::Unknown`] and therefore false.
pub const MAX_CONDITION_BYTES: usize = 256;

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

/// Room facts that the name tokens do not carry, resolved by the caller from
/// authoritative guild state. IDs are opaque strings (Discord snowflakes in
/// decimal) and match exactly.
///
/// Member counts, the user limit, the room number, parties, the channel kind
/// and the local time come from the [`RoomContext`] instead, so a condition
/// always agrees with the tokens rendered next to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionFacts {
    /// The room owner (`OWNER:id`).
    pub owner_id: Option<String>,
    /// Roles the owner has (`ROLE:id`).
    pub owner_role_ids: Vec<String>,
    /// Members currently in the room, including the owner (`MEMBER:id`).
    pub member_ids: Vec<String>,
    /// Union of the roles of the members currently in the room (`ANY_ROLE:id`).
    pub member_role_ids: Vec<String>,
    /// The owner shows a game activity (`PLAYING`).
    pub owner_playing: bool,
    /// The owner is streaming through Discord (`LIVE`, `LIVE_DISCORD`).
    pub owner_live_discord: bool,
    /// The owner is live on an external platform (`LIVE`, `LIVE_EXTERNAL`).
    pub owner_live_external: bool,
    /// Members in the room streaming through Discord (`ANY_LIVE`).
    pub live_discord_count: u32,
    /// Members in the room live on an external platform (`ANY_LIVE`).
    pub live_external_count: u32,
    /// Resolved, aliased game titles the room name shows: the majority title,
    /// both titles on a two-way tie, empty when no game wins (`GAME`). Fill it
    /// from [`crate::voice_naming::majority_games`].
    pub games: Vec<String>,
    /// The room is locked or hidden (`PRIVATE`).
    pub private: bool,
}

/// A parsed condition. Anything outside the grammar is [`Condition::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// A bare keyword such as `FULL` or `LIVE`.
    Keyword(Keyword),
    /// `ROLE:id`, `ANY_ROLE:id`, `MEMBER:id` or `OWNER:id`.
    Person { scope: PersonScope, id: String },
    /// `GAME:text` (contains), `GAME=title` or `GAME!=title` (exact).
    Game { op: GameOp, title: String },
    /// A numeric comparison of two operands.
    Compare {
        left: Operand,
        op: Comparison,
        right: Operand,
    },
    /// Unknown or malformed; always false.
    Unknown,
}

/// Bare keywords that need no operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    /// The owner shows a game activity.
    Playing,
    /// The owner is live on Discord or an external platform.
    Live,
    /// The owner is streaming through Discord.
    LiveDiscord,
    /// The owner is live on an external platform.
    LiveExternal,
    /// Anyone in the room is live on either source.
    AnyLive,
    /// The owner is in the room.
    Owner,
    /// The room shows a game title.
    Game,
    /// Anyone in the room is playing (`@@num_playing@@` above zero).
    Players,
    /// The largest party advertises a maximum and has reached it.
    Max,
    /// Someone in the room shows a rich-presence party.
    Rich,
    /// The room has a user limit and is at or over it.
    Full,
    /// The room is private; never on standalone channels.
    Private,
    /// Saturday or Sunday in the room's local time.
    Weekend,
    /// Monday to Friday in the room's local time.
    Weekday,
}

/// Whose roles or identity a person condition checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonScope {
    /// `ROLE:id`: the owner has the role.
    Role,
    /// `ANY_ROLE:id`: someone in the room has the role.
    AnyRole,
    /// `MEMBER:id`: the user is in the room.
    Member,
    /// `OWNER:id`: the user owns the room.
    Owner,
}

/// Game title match, case-insensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameOp {
    /// `GAME:text`: a shown title contains the text.
    Contains,
    /// `GAME=title`: a shown title equals the title.
    Equals,
    /// `GAME!=title`: no shown title equals the title.
    NotEquals,
}

/// Numeric comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// `<`
    Less,
    /// `>`
    Greater,
    /// `<=`
    LessOrEqual,
    /// `>=`
    GreaterOrEqual,
    /// `=` (and `:` after `WEEKDAY` or `MONTH`)
    Equal,
    /// `!=`
    NotEqual,
}

impl Comparison {
    fn holds(self, left: i64, right: i64) -> bool {
        match self {
            Comparison::Less => left < right,
            Comparison::Greater => left > right,
            Comparison::LessOrEqual => left <= right,
            Comparison::GreaterOrEqual => left >= right,
            Comparison::Equal => left == right,
            Comparison::NotEqual => left != right,
        }
    }
}

/// A numeric comparison operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// An integer literal, or a weekday/month name compared with that keyword.
    Number(i64),
    /// A counter token.
    Counter(Counter),
    /// `$#` (any zero padding): the room number. `##` and `+#` are not numeric.
    RoomNumber,
    /// `WEEKDAY`: 1 (Monday) to 7 (Sunday) in the room's local time.
    Weekday,
    /// `MONTH`: 1 (January) to 12 (December) in the room's local time.
    Month,
}

/// Counter tokens usable in comparisons. Each takes the value the token
/// renders; a blank token (`@@slots@@` without a limit) has no value and
/// makes the comparison false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// `@@num@@`
    Members,
    /// `@@num_others@@`
    Others,
    /// `@@num_live@@`
    Live,
    /// `@@num_playing@@`
    Playing,
    /// `@@limit@@`
    Limit,
    /// `@@slots@@`
    Slots,
    /// `@@party_size@@`
    PartySize,
    /// `@@hour@@`
    Hour,
}

impl Counter {
    const ALL: [Counter; 8] = [
        Counter::Members,
        Counter::Others,
        Counter::Live,
        Counter::Playing,
        Counter::Limit,
        Counter::Slots,
        Counter::PartySize,
        Counter::Hour,
    ];

    fn token(self) -> &'static str {
        match self {
            Counter::Members => "num",
            Counter::Others => "num_others",
            Counter::Live => "num_live",
            Counter::Playing => "num_playing",
            Counter::Limit => "limit",
            Counter::Slots => "slots",
            Counter::PartySize => "party_size",
            Counter::Hour => "hour",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Calendar {
    Weekday,
    Month,
}

impl Calendar {
    fn of(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("WEEKDAY") {
            Some(Calendar::Weekday)
        } else if text.eq_ignore_ascii_case("MONTH") {
            Some(Calendar::Month)
        } else {
            None
        }
    }

    fn names(self) -> &'static [&'static str] {
        match self {
            Calendar::Weekday => &WEEKDAY_NAMES,
            Calendar::Month => &MONTH_NAMES,
        }
    }

    fn token(self) -> &'static str {
        match self {
            Calendar::Weekday => "weekday",
            Calendar::Month => "month",
        }
    }

    fn operand(self) -> Operand {
        match self {
            Calendar::Weekday => Operand::Weekday,
            Calendar::Month => Operand::Month,
        }
    }

    /// 1-based position of a full English name, case-insensitive.
    fn index_of(self, name: &str) -> Option<i64> {
        self.names()
            .iter()
            .position(|known| known.eq_ignore_ascii_case(name))
            .and_then(|index| i64::try_from(index + 1).ok())
    }

    /// 1-based value in the room's local time, as the token renders it.
    fn value(self, room: &RoomContext) -> Option<i64> {
        self.index_of(&render_token(room, self.token()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Colon,
    Compare(Comparison),
}

/// Parse condition text (nested conditions already resolved). Keywords match
/// ASCII case-insensitively; IDs match exactly. Never panics: anything outside
/// the grammar is [`Condition::Unknown`].
#[must_use]
pub fn parse_condition(text: &str) -> Condition {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_CONDITION_BYTES {
        return Condition::Unknown;
    }
    let Some((at, width, operator)) = find_operator(text) else {
        return bare_keyword(text).map_or(Condition::Unknown, Condition::Keyword);
    };
    let left = text[..at].trim();
    let right = text[at + width..].trim();
    if right.is_empty() {
        return Condition::Unknown;
    }
    if let Some(scope) = person_scope(left) {
        return match operator {
            Operator::Colon => Condition::Person {
                scope,
                id: right.to_string(),
            },
            Operator::Compare(_) => Condition::Unknown,
        };
    }
    if left.eq_ignore_ascii_case("GAME") {
        let op = match operator {
            Operator::Colon => GameOp::Contains,
            Operator::Compare(Comparison::Equal) => GameOp::Equals,
            Operator::Compare(Comparison::NotEqual) => GameOp::NotEquals,
            Operator::Compare(_) => return Condition::Unknown,
        };
        return Condition::Game {
            op,
            title: right.to_string(),
        };
    }
    let op = match operator {
        Operator::Compare(op) => op,
        // `WEEKDAY:Friday` and `MONTH:12` read as equality.
        Operator::Colon if Calendar::of(left).is_some() => Comparison::Equal,
        Operator::Colon => return Condition::Unknown,
    };
    match (
        parse_operand(left, Calendar::of(right)),
        parse_operand(right, Calendar::of(left)),
    ) {
        (Some(left), Some(right)) => Condition::Compare { left, op, right },
        _ => Condition::Unknown,
    }
}

/// First operator in the text. Keywords and operands contain no operator
/// characters, so anything after it (a game title, an ID) is the right side.
fn find_operator(text: &str) -> Option<(usize, usize, Operator)> {
    let bytes = text.as_bytes();
    bytes.iter().copied().enumerate().find_map(|(at, byte)| {
        let next_is_equals = bytes.get(at + 1) == Some(&b'=');
        let (width, operator) = match (byte, next_is_equals) {
            (b'<', true) => (2, Operator::Compare(Comparison::LessOrEqual)),
            (b'>', true) => (2, Operator::Compare(Comparison::GreaterOrEqual)),
            (b'!', true) => (2, Operator::Compare(Comparison::NotEqual)),
            (b'<', false) => (1, Operator::Compare(Comparison::Less)),
            (b'>', false) => (1, Operator::Compare(Comparison::Greater)),
            (b'=', _) => (1, Operator::Compare(Comparison::Equal)),
            (b':', _) => (1, Operator::Colon),
            _ => return None,
        };
        Some((at, width, operator))
    })
}

fn bare_keyword(text: &str) -> Option<Keyword> {
    const KEYWORDS: [(&str, Keyword); 14] = [
        ("PLAYING", Keyword::Playing),
        ("LIVE", Keyword::Live),
        ("LIVE_DISCORD", Keyword::LiveDiscord),
        ("LIVE_EXTERNAL", Keyword::LiveExternal),
        ("ANY_LIVE", Keyword::AnyLive),
        ("OWNER", Keyword::Owner),
        ("GAME", Keyword::Game),
        ("PLAYERS", Keyword::Players),
        ("MAX", Keyword::Max),
        ("RICH", Keyword::Rich),
        ("FULL", Keyword::Full),
        ("PRIVATE", Keyword::Private),
        ("WEEKEND", Keyword::Weekend),
        ("WEEKDAY", Keyword::Weekday),
    ];
    KEYWORDS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(text))
        .map(|(_, keyword)| *keyword)
}

fn person_scope(text: &str) -> Option<PersonScope> {
    [
        ("ROLE", PersonScope::Role),
        ("ANY_ROLE", PersonScope::AnyRole),
        ("MEMBER", PersonScope::Member),
        ("OWNER", PersonScope::Owner),
    ]
    .into_iter()
    .find(|(name, _)| name.eq_ignore_ascii_case(text))
    .map(|(_, scope)| scope)
}

/// `names` lets a weekday or month name stand for its number when the other
/// side of the comparison is that keyword.
fn parse_operand(text: &str, names: Option<Calendar>) -> Option<Operand> {
    if let Some(calendar) = Calendar::of(text) {
        return Some(calendar.operand());
    }
    if let Some(index) = names.and_then(|calendar| calendar.index_of(text)) {
        return Some(Operand::Number(index));
    }
    if let Some(name) = text
        .strip_prefix("@@")
        .and_then(|rest| rest.strip_suffix("@@"))
    {
        return Counter::ALL
            .into_iter()
            .find(|counter| counter.token().eq_ignore_ascii_case(name))
            .map(Operand::Counter);
    }
    if let Some(zeros) = text
        .strip_prefix('$')
        .and_then(|rest| rest.strip_suffix('#'))
    {
        return zeros
            .bytes()
            .all(|b| b == b'0')
            .then_some(Operand::RoomNumber);
    }
    text.parse().ok().map(Operand::Number)
}

impl Operand {
    fn value(self, room: &RoomContext) -> Option<i64> {
        match self {
            Operand::Number(n) => Some(n),
            Operand::Counter(counter) => render_token(room, counter.token()).parse().ok(),
            Operand::RoomNumber => Some(i64::from(room.room_number)),
            Operand::Weekday => Calendar::Weekday.value(room),
            Operand::Month => Calendar::Month.value(room),
        }
    }
}

impl Condition {
    /// Whether the condition holds for the room. Unknown conditions and
    /// comparisons with a valueless operand are false.
    #[must_use]
    pub fn holds(&self, facts: &ConditionFacts, room: &RoomContext) -> bool {
        match self {
            Condition::Keyword(keyword) => keyword.holds(facts, room),
            Condition::Person { scope, id } => match scope {
                PersonScope::Role => facts.owner_role_ids.contains(id),
                PersonScope::AnyRole => facts.member_role_ids.contains(id),
                PersonScope::Member => facts.member_ids.contains(id),
                PersonScope::Owner => facts.owner_id.as_ref() == Some(id),
            },
            Condition::Game { op, title } => {
                let title = title.to_lowercase();
                let mut shown = facts.games.iter().map(|game| game.to_lowercase());
                match op {
                    GameOp::Contains => shown.any(|game| game.contains(&title)),
                    GameOp::Equals => shown.any(|game| game == title),
                    GameOp::NotEquals => !shown.any(|game| game == title),
                }
            }
            Condition::Compare { left, op, right } => match (left.value(room), right.value(room)) {
                (Some(left), Some(right)) => op.holds(left, right),
                _ => false,
            },
            Condition::Unknown => false,
        }
    }
}

impl Keyword {
    fn holds(self, facts: &ConditionFacts, room: &RoomContext) -> bool {
        match self {
            Keyword::Playing => facts.owner_playing,
            Keyword::Live => facts.owner_live_discord || facts.owner_live_external,
            Keyword::LiveDiscord => facts.owner_live_discord,
            Keyword::LiveExternal => facts.owner_live_external,
            Keyword::AnyLive => facts.live_discord_count > 0 || facts.live_external_count > 0,
            Keyword::Owner => room.owner_present,
            Keyword::Game => !facts.games.is_empty(),
            Keyword::Players => Operand::Counter(Counter::Playing)
                .value(room)
                .is_some_and(|playing| playing > 0),
            Keyword::Max => largest_party(room)
                .is_some_and(|party| party.max.is_some_and(|max| max > 0 && party.size >= max)),
            Keyword::Rich => !room.parties.is_empty(),
            Keyword::Full => room.user_limit > 0 && room.member_count >= room.user_limit,
            Keyword::Private => facts.private && room.channel_kind != ChannelKind::Standalone,
            Keyword::Weekend => Calendar::Weekday.value(room).is_some_and(|day| day >= 6),
            Keyword::Weekday => Calendar::Weekday
                .value(room)
                .is_some_and(|day| (1..=5).contains(&day)),
        }
    }
}

/// The party whose metadata the party tokens show: the first of the largest,
/// none when three or more parties tie for largest.
fn largest_party(room: &RoomContext) -> Option<&PartyInfo> {
    let top = room.parties.iter().map(|party| party.size).max()?;
    let mut leaders = room.parties.iter().filter(|party| party.size == top);
    let first = leaders.next()?;
    leaders.next();
    leaders.next().is_none().then_some(first)
}

/// A token's unfinalized value, exactly as the template renders it.
fn render_token(room: &RoomContext, name: &str) -> String {
    Evaluation::new(room, &PassthroughExtensions)
        .evaluate(&Template(vec![Segment::Token(name.to_string())]))
}

/// A conditional node split at its first top-level `??` and `//`. Separators
/// inside nested syntax (choices, plurals, styling, other conditionals) never
/// split the node; unclosed syntax stays literal and cannot hide them.
struct Node {
    condition: Vec<Segment>,
    branches: Vec<Template>,
}

impl Node {
    fn split(source: &str) -> Option<Self> {
        let inner = source.strip_prefix("{{")?.strip_suffix("}}")?;
        let (condition, rest) = split_at(parse(inner).0, "??").ok()?;
        let branches = match split_at(rest, "//") {
            Ok((yes, no)) => vec![Template(yes), Template(no)],
            Err(yes) => vec![Template(yes)],
        };
        Some(Node {
            condition,
            branches,
        })
    }
}

/// Split at the first separator in a top-level literal, or give the segments
/// back unchanged.
fn split_at(
    segments: Vec<Segment>,
    separator: &str,
) -> Result<(Vec<Segment>, Vec<Segment>), Vec<Segment>> {
    let Some((index, at)) =
        segments
            .iter()
            .enumerate()
            .find_map(|(index, segment)| match segment {
                Segment::Text(text) => text.find(separator).map(|at| (index, at)),
                _ => None,
            })
    else {
        return Err(segments);
    };
    let mut before = segments;
    let mut after = before.split_off(index);
    if let Some(Segment::Text(text)) = after.first_mut() {
        let tail = text.split_off(at)[separator.len()..].to_string();
        if text.is_empty() {
            after.remove(0);
        } else {
            before.push(after.remove(0));
        }
        if !tail.is_empty() {
            after.insert(0, Segment::Text(tail));
        }
    }
    Ok((before, after))
}

/// V6 conditional evaluation over one room's [`ConditionFacts`].
///
/// Use it directly as an [`ExtensionPolicy`] (styling stays literal), or call
/// [`Conditions::evaluate`] from a parent policy's `conditional` to compose
/// with styling. One value serves one render at a time.
#[derive(Debug)]
pub struct Conditions<'f> {
    facts: &'f ConditionFacts,
    depth: Cell<usize>,
}

/// Holds one level of the nesting budget until dropped.
struct Level<'c>(&'c Cell<usize>);

impl Drop for Level<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

impl<'f> Conditions<'f> {
    /// Evaluate conditions against these room facts.
    #[must_use]
    pub fn new(facts: &'f ConditionFacts) -> Self {
        Self {
            facts,
            depth: Cell::new(0),
        }
    }

    /// Evaluate one `{{...}}` node: resolve the condition (innermost
    /// conditionals first, name tokens left unexpanded), then render the
    /// selected branch in the shared session. Random positions in both
    /// branches stay reserved whichever branch is chosen. A false condition
    /// without `// no` renders nothing. A node without `??`, or nested deeper
    /// than [`MAX_CONDITION_NESTING`], renders as its literal source.
    pub fn evaluate<E: ExtensionPolicy>(
        &self,
        source: &str,
        evaluation: &mut Evaluation<'_, E>,
    ) -> String {
        let Some(_level) = self.enter() else {
            return source.to_string();
        };
        let Some(node) = Node::split(source) else {
            return source.to_string();
        };
        let holds = self.holds(&node.condition, evaluation.context());
        // Out of range (false, no else branch) reserves all branches and
        // renders nothing.
        evaluation.evaluate_selected_branch(&node.branches, usize::from(!holds))
    }

    fn enter(&self) -> Option<Level<'_>> {
        let depth = self.depth.get();
        if depth >= MAX_CONDITION_NESTING {
            return None;
        }
        self.depth.set(depth + 1);
        Some(Level(&self.depth))
    }

    fn holds(&self, condition: &[Segment], room: &RoomContext) -> bool {
        parse_condition(&self.condition_text(condition, room)).holds(self.facts, room)
    }

    /// Condition source with nested conditionals replaced by their selected
    /// branch's source. Nothing is rendered, so name tokens stay unexpanded
    /// and no random position is consumed.
    fn condition_text(&self, segments: &[Segment], room: &RoomContext) -> String {
        let mut text = String::new();
        for segment in segments {
            match segment {
                Segment::Text(literal) => text.push_str(literal),
                Segment::Extension(Extension::Conditional { source, .. }) => {
                    text.push_str(&self.resolve_nested(source, room));
                }
                other => text.push_str(&Template(vec![other.clone()]).to_string()),
            }
        }
        text
    }

    fn resolve_nested(&self, source: &str, room: &RoomContext) -> String {
        let Some(_level) = self.enter() else {
            return source.to_string();
        };
        let Some(node) = Node::split(source) else {
            return source.to_string();
        };
        let selected = usize::from(!self.holds(&node.condition, room));
        node.branches
            .get(selected)
            .map_or_else(String::new, |branch| self.condition_text(&branch.0, room))
    }
}

impl ExtensionPolicy for Conditions<'_> {
    fn conditional(&self, source: &str, evaluation: &mut Evaluation<'_, Self>) -> String {
        self.evaluate(source, evaluation)
    }

    fn styled(
        &self,
        _modes: &str,
        _body: &Template,
        source: &str,
        _evaluation: &mut Evaluation<'_, Self>,
    ) -> String {
        source.to_string()
    }
}
