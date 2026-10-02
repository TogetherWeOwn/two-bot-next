# Independent voice-template corpus

Source of truth: [`docs/voice-rooms.md`](../../docs/voice-rooms.md), V5/V6 and
V7's alias/nickname rules. Expectations were authored from that specification,
not from a renderer or another implementation. The full source document's
SHA-256 is pinned in `corpus.json`; a specification change requires a conscious
fixture/index update, not automatic regeneration of expected outputs.

## Run (Python 3.10+, standard library only)

From the repository root:

```sh
python3 tests/voice_templates/validate.py
python3 -m unittest discover -s tests/voice_templates -p 'test_*.py' -v
```

Both run in the existing `check` CI job. No package installation, network, DB,
Discord client, secrets, renderer, or paid runner is added. The validator accepts
`--corpus`, `--coverage`, and `--spec` paths for consumer/negative-fixture checks.
Malformed data exits 1 with `INVALID:` on stderr; success exits 0 with a compact
JSON count/digest report. The CLI itself rereads and validates twice.

## What is committed

| Contract | Count | What it proves |
|---|---:|---|
| Distinct exact input/context/output cases | 260 | An independently specified expected string is available |
| Invariant-only cases | 11 | Nonempty, length, deterministic random behaviour; **not** an exact oracle |
| Deferred probes | 48 | The input is recorded, but exact semantics await clarification |
| Total cases | 319 | All have IDs, inputs, context references and typed expectations |
| Coverage features | 93 | Every listed V5/V6 token, condition and style has a reverse case index |
| Ambiguity records | 15 | No renderer-specific decisions are silently selected |
| Cross-rename stability groups | 4 | Same input/seed, changed non-seed room facts, equal random result required |

`coverage.json` is the machine-readable coverage index, **not a claim that all
features have an exact output oracle**. It maps each feature to a spec literal,
section and case IDs. Resolve those IDs in `corpus.json` to see exact, invariant
or deferred coverage. It also maps each open ambiguity to affected case IDs.
The independent spec-inventory extractor makes an omitted/new token, keyword or
style fail validation even if someone updates both fixture and index together.

| Coverage family | Entries |
|---|---:|
| Number, people, counts, game, party, time and random tokens | 24 |
| Member/other/party plurals, random choice/list, resting names | 6 |
| Conditional keywords | 19 |
| Numeric comparison operators | 6 |
| Style modes (including aliases and `<N>w` family) | 26 |
| Evaluation, finalization, unknown and nesting rules | 12 |

The 150-case minimum counts **distinct exact** input/context pairs only. Neither
unresolved probes, invariant probes nor duplicated copies can satisfy it.

## Consumer contract (renderer integration belongs to the parent slice)

Each case has `id`, `input` (template string), `context` (key into the bundle's
`contexts` map), `expected`, and `covers` (feature IDs). Context is deliberately
an implementation-independent snapshot, not a proposed runtime API:

- `members` contains humans only. IDs are opaque fixture labels, not real member
  identifiers. Each human has a display name, optional nick, roles, game,
  Discord/external live flags, optional stream title and optional rich party.
  No bot filtering or Discord event processing is exercised by these fixtures.
- `owner_id` selects the current owner among populated snapshots;
  `original_creator_name` is a separately retained, already-resolved name.
  Empty snapshots retain the last owner ID. Lifecycle and transfer decisions
  are outside this corpus.
- `number` is already allocated. Creator/category allocation and starting-number
  configuration remain integration tests, not template-rendering tests.
- `limit` is 0–99; zero means unlimited. `private` is a room fact. On standalone
  channels the PRIVATE condition must still be false.
- `clock` is an **already guild-local civil clock** (English weekday/month, hour
  0–23 and timezone label). UTC is used in this corpus. Timezone conversion,
  timestamp acquisition and DST boundary testing remain the integration's job.
- `settings` supplies the no-game label, aliases, named lists and the two game
  selection toggles. A party's `id` deduplicates observations of the same party;
  size/maximum can include players outside the voice room.
- `seed` is an opaque persisted per-room seed, not a prescribed PRNG/hash/seed
  encoding. Adapters must map it consistently to their engine's seed type.

Consumers should load the bundle and adapt context to the renderer under test:

1. For `expected.kind == exact`, render and compare the string byte-for-byte.
2. For `invariant`, enforce nonempty output, the specified 100-character ceiling,
   repeat-render equality, and any supplied `allowed_outputs` membership and
   `casefold_equals` constraints together. The case-fold target must already be
   case-folded; if both constraints exist, at least one allowed output must match
   it. Case folding can expand characters, so the target itself may exceed 100
   characters even though every rendered/allowed output remains bounded.
   Even without an allowed-output set, the target must have a Unicode case-fold
   preimage of at most 100 characters. The validator computes possible preimage
   lengths using Python's Unicode expansion table: 200 `s` characters can come
   from 100 `ß` characters, but 101 ASCII `x` characters are infeasible.
   Targets cannot start with whitespace. Trailing whitespace requires a preimage
   of **exactly 100 characters**, consistent with a trim-then-truncate boundary;
   the consumer still enforces the rendered output's 100-character ceiling.
   For each `stability_groups` entry, render all referenced
   cases and require the **same output across the changed contexts**. Do not
   assume distinct seeds necessarily yield different results.
3. For `deferred`, report the ambiguity ID; do not silently count a probe as
   passed or manufacture its expected output from the engine's current result.
4. Report the exact/invariant/deferred counts separately. Revisit deferred cases
   only after the approved spec provides the missing semantics.

Examples pinned verbatim by the spec include `Apex #3` and
`Charlie · 4 people`. Boundary cases cover singular at exactly one human,
owner-excluding plurals, party plurals, zero/unlimited/full limits, zero/one/many
members, ASCII truncation at 99/100/101, empty fallback, Unicode and RTL names,
three-way ties, nickname/alias handling, condition nesting, token-vs-token
comparison, and condition → token → style → trim/truncate ordering.
Conditional branches avoid delimiter padding so fixtures do not choose
unwritten whitespace-preservation rules. Number examples start at 1, matching
the numbered rooms shown by the spec; no zero/negative Roman behaviour is chosen.

## Explicit residuals and limits

The validator checks schema, referential integrity, exact-output shape, coverage
inventory and reverse indexes, duplicate/conflicting expectations, seed-group
integrity, spec drift and deterministic serialization. It **does not evaluate
templates** or establish that the authored expected strings are correct by
running an implementation. The Code Reviewer must independently check those
strings against the specification. V5 trims **before** truncating: a valid
100-character prefix can therefore end in whitespace. Leading whitespace and
trailing whitespace below the ceiling remain invalid. Regression controls cover
that boundary for exact and allowed outputs, mixed ordinary-choice/named-list
coverage, compatible/contradictory case-fold invariants, and standalone folded
target feasibility with two- and three-character Unicode expansion (including
expanded boundary prefixes). Keyword coverage distinguishes bare `OWNER` from
`OWNER:id`; comparison coverage requires numeric operands in a conditional
header, not operator-like literal text in the template or a branch. These are
fixture coverage controls, not an implementation of the template parser.

Key unresolved areas in `coverage.json`: exact NATO spelling/wrap formatting;
two-game tie ordering/separator; owner-preference and inactive voting; offline
stream output; missing/tied parties; several conditional scopes/operand grammars;
small-caps/spacing/acronym/novelty/font codepoint mappings; word boundaries;
random selection algorithm and missing lists; resting syntax on other channel
kinds; fallback literal/Unicode length metric; malformed template syntax.
Unicode fonts are mapped to probes, not claimed to have renderer unit tests.

V5/V6 renderer conformance, parser property tests, status/name channel wiring,
clock conversion, bot filtering, ownership events, restart persistence, Discord
rename pacing and a staging-guild soak remain on the parent slice. This leaf
neither depends on those implementations nor proves runtime parity. No guild,
staging/production database, or production activation is touched.
