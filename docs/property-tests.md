# Bounded parser and wire-format properties

The `property_` tests use proptest 1.11.0 with 64 cases each and bounded
strings/byte vectors. They use synthetic fixtures only: no Discord, network,
production/staging database, fuzzing harness or subprocess timeout worker.
Only proptest's `std` feature is enabled (not `fork`, `timeout` or `tempfile`).
New active dependencies offer MIT or Apache-2.0; `deny.toml` stays unchanged.

## Contracts

| Surface | Properties |
| --- | --- |
| LFG role specification | Arbitrary Unicode never panics; accepted normalized entries reparse identically; generated unique roles round-trip; 1–20 roles, 1–99 slots and 1–80 UTF-16-unit labels |
| LFG title / starts-at | ECMAScript trim and 1–100 UTF-16 units; normalization is idempotent; generated timezone offsets normalize to UTC milliseconds; equal/past instants refuse |
| RSVP event ID / status | Exactly 17–20 ASCII digits (not numeric u64 validation); status values are exact/case-sensitive and format/parse round-trip |
| Command options / registry | Builder metadata and serde round-trip; parity §1 numeric limits and required flags; first-wins ordered merge and 100-command ceiling |
| Internal signing / keys | Arbitrary bytes/strings never panic; accepted key specifications reparse; validated timestamp/nonce framing has exactly five fields; changing timestamp, nonce or raw body changes canonical text/signature; a repeated key ID or reused secret refuses naming IDs only; a JSON key repeated at any depth refuses the body |
| Internal moderation numbers | Every case exercises both inclusive edges and adjacent refusals; noninteger JSON and missing required fields refuse |
| Runtime caps (tempban/timeout ceilings, schedule windows, sticky debounce, UTF-16 labels) | Wire-level inclusive edges plus adjacent refusals for every cap; string-coerced/noninteger/missing numbers refuse; LFG/schedule lengths count UTF-16 units (astral scalars cost two) |
| Moderation audit marker | Generated recognized actions and valid actors mint/parse to the same marker; changed MAC or guild refuses; arbitrary Unicode marker text never panics |
| Voice configuration | Arbitrary bytes never panic; accepted documents round-trip; generated valid configuration preserves Unicode template source, nullable fields and flags; creator numeric bounds are exact |
| Automod NFKC / word boundaries | NFKC+lowercase normalization is idempotent; fullwidth ASCII folds to ASCII; whitespace/zero-width gaps between bad-word letters still match; affixed/embedded words refuse; empty-normalization words and empty content never match |

Builder parity and runtime caps are separate: tempban/timeout builders advertise
minimum 60 with no maximum (parity §1). Internal runtime validation caps them
at 365 days and 28 days respectively. Schedule advertises 1–525600 / 60–525600
minutes; sticky debounce is 1–300 seconds. LFG lengths use UTF-16 units, while
voice literal names use Unicode scalars.

Signing supports only **POST /internal/actions**. The injectivity property is
on the validated request domain: timestamp digits and 32 hex nonce characters
cannot contain framing newlines. The pure signing helpers themselves do not
validate those strings, so injectivity is not claimed for arbitrary newline-bearing
arguments. Different raw bodies producing different digests assumes SHA-256
collision resistance; a finite property suite is not a mathematical proof.
Moderation marker prose is deliberately not authenticated. Voice configuration
stores template source, and does not compile or render it.

## Execution and regressions

On the controller, use the bounded build wrapper (see [build-cache.md](build-cache.md)):

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib property_
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_config property_
```

Hosted `check` runs all properties in the normal unit/integration targets, then
reuses the same workspace feature/target graph for a timed `property_` pass.
It reports elapsed seconds and fails at 30 seconds or above; compilation is
outside that timing step. Shrinking and saved proptest regressions remain enabled.
A discovered defect needs a minimal deterministic unit regression and a recorded
follow-up note; do not silence a failure by rejecting its input in the generator.
