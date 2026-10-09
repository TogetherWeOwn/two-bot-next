# Process history contract (V1, offline)

Normative schema, arithmetic and failure vocabulary for a private,
whole-process observation history: 120 minute-spaced snapshots of one bot
process, written once and read back by a different principal.

**This contract is offline only. No live adapter is installed.** The code is
`wrangler/src/process-history-contract.ts`; nothing imports it, and it reads
no operating-system file, clock, environment, network, database or storage.
Every input (observations, time strings, the hash function, the persisted-byte
map) is supplied by the caller. The tests in
`wrangler/test/process-history-contract.test.ts` and the synthetic vectors in
`wrangler/test/fixtures/process-history-v1.json` prove the semantics against an
in-memory stand-in. A synthetic write and readback proves **no** installed
protected principal, **no** backend capacity and **no** deployed bot behavior.

What this is not:

- Not a sampler, transport, storage binding or reader. Those need their own
  reviewed design, authority and receipts.
- Not a health verdict. `complete_structural` means the offline structure is
  whole. `acceptance` is always `UNDECIDED`; there is no pass flag, no
  flat-memory flag and no error-spike flag in the schema.
- Not 120 CPU minutes. 120 snapshots hold one baseline (slot 0) and **119**
  adjacent CPU deltas. Full 120-minute CPU coverage is **unmet**, and no extra
  boundary read is defined.

## Primitives

| Name | Definition |
|---|---|
| `U64` | JSON **string**, `0` or `[1-9][0-9]{0,19}`, value at most 18446744073709551615. Never converted through a floating-point number. |
| `UTC` | ASCII `YYYY-MM-DDTHH:mm:ss.sssZ` (24 bytes), valid Gregorian date, seconds 0-59, milliseconds, `Z` only. |
| `H32`, `H40`, `H64` | Lowercase hexadecimal of exactly 32, 40 or 64 characters. |
| `UUID` | Lowercase hyphenated UUID. |
| `Alias` | `H32`, random per run or principal; not a Discord identifier, not derived from content, no reverse mapping. |
| `Mask` | JSON unsigned integer 0..4294967295: 32 fixed flag bits. Bit 31 is 2147483648; masks are never signed. |
| `BuildId` | `null` or `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`. The literal `unknown` is normalized to `null` at the input boundary only and is rejected in stored bytes. |

`Nullable X` is `X` or `null`; a missing property is a schema error. `null`
never means zero, success or consent.

## Canonical bytes

Compact UTF-8 JSON: keys sorted, no whitespace, integers only, strings limited
to printable ASCII without escapes. Duplicate keys, floats, negative numbers,
exponents, leading zeros, integers above 2^53 - 1, escapes, non-ASCII and
nesting deeper than six levels are errors. A decoder checks the byte cap
first, parses strictly, validates, then requires the input to equal the
re-canonicalized value, so whitespace, key order and escaped equivalents are
refused rather than repaired. Hashes are SHA-256 of these exact bytes. A
payload refused for privacy is never hashed or stored.

Every object is closed: an unknown property is `INVALID_SCHEMA`, and if its name
looks like an identifier, content, credential, URL, host, SQL, environment,
process id, command name, boot id or free-text reason it also sets
`PRIVACY_REJECTED`. A refusal returns flags only, never the offending value.

## Objects

Byte caps are canonical UTF-8 bytes. Descriptor, record and seal: 4096.
Incident: 1024. Manifest: 32768. Key: 128.

**Run descriptor** (`b2proc.run.v1`, immutable): `contract_sha256`, `mode`
(`offline` or `staging`), `run_alias`, `fixture_alias`, `writer_alias`,
`reader_alias` (the two principals must differ), `T_utc` and
`T_evidence_sha256` (the receipt is null while T is unset; a run arms only when
both are present), the literal `schedule`
(`7200`, `60`, `120`, `14400`), the literal `fixture_bounds` (900 s, 20
expectations, 60 receipts, full 60 is truncated), nine nullable `authority`
hashes, `initial_identity`, `logical_budget_bytes` (1048576) and `created_utc`.

**Identity** (frozen in the descriptor, repeated in every record): nine
provenance fields (`bot_source_sha`, `worker_source_sha`, `bot_build_id`,
`container_image_sha256`, `worker_version_id`, `process_alias`,
`process_start_ticks`, `startup_observed_utc`, `startup_mono_ns`), plus
`clock_kind` (`linux_clock_monotonic` or `unavailable`),
`cpu_counter_width_bits`, `cpu_ticks_per_second`, `allocated_vcpu_milli` and
`limits_receipt_sha256`. A value is never guessed from deployment config.

**Observation**: wall `utc_start`/`utc_end`, monotonic
`mono_start_ns`/`mono_end_ns` (end not before start), nullable
`cpu_anchor_mono_ns` (inside the monotonic bracket), `rss_bytes`,
`cpu_user_ticks`, `cpu_system_ticks`, and `source` (`linux_proc_self_v1`, or
`unavailable` with every quantity null). `rss_bytes` is the process resident
set from supplied status text, kB times 1024 in checked integers; cgroup,
container and allocator figures are not this source.

**Record** (`b2proc.record.v1`, one per slot 0..119): aliases, `i`,
`scheduled_offset_seconds` (7200 + 60 i), `scheduled_utc` (T + offset when T is
qualified, else null), `writer_received_utc`, `write_seq` (arrival order of
committed slots), `identity`, nullable `observation`, `cpu_interval` and
`flags_mask`. Slot 0 carries `CPU_BASELINE_ONLY` and no interval. A null
observation is a tombstone and carries `SOURCE_UNAVAILABLE`.

**CPU interval**: `previous_i`, `start_anchor_mono_ns`, `end_anchor_mono_ns`,
`delta_mono_ns`, `delta_cpu_ticks`, `one_core_micropercent`,
`allocated_micropercent`. Either all null, or the five delta fields set with
the rates null (tick frequency unknown), or one-core set with the allocated rate
null (allocation unknown), or all set. Rates are millionths of a percentage
point: 100% of one core is 100000000.

**Incident** (`b2proc.incident.v1`, ordinals 0..31): fixed `kind` vocabulary,
no free text. Ordinals 0..30 are ordinary; ordinal 31 is reserved for the
terminal stop (`stop`, or `budget` when ordinary capacity ran out).
`candidate_sha256` is the digest of the canonical `{identity, observation}` of
the submitted candidate (null when nothing sanitized exists);
`persisted_sha256` is the digest of the persisted record bytes.

**Seal** (`b2proc.writer-seal.v1` / `b2proc.reader-seal.v1`): `last_i`
(the highest committed slot) with `last_committed_record_sha256` (that slot's
hash), `outcome` (`finished`, `stopped`, `unknown`), `stopped_utc`,
`flags_mask`, and `receipt_sha256` (null offline). A writer seal may say
`finished` only at slot 119.

**Manifest** (`b2proc.manifest.v1`, writer claim or independent readback):
exactly 120 entries sorted by index, each
`{i, record_sha256, write_status, read_status, flags_mask}`, recomputed counts,
a logical byte budget (`physical_storage_bytes` is always null), `coverage`,
`cpu_interval_boundary` = `baseline_0_adjacent_1_to_119`, and
`acceptance` = `UNDECIDED`. A writer claim never verifies itself and can never
be `complete_structural`.

## CPU arithmetic

For slot i and exactly slot i-1, both usable (all nine provenance fields known,
Linux monotonic clock, 64-bit counter width backed by a limits receipt, counters
present, anchor above zero) with equal identity:

- `delta_mono_ns` = anchor(i) - anchor(i-1), which must be positive
- `delta_cpu_ticks` = (user(i) - user(i-1)) + (system(i) - system(i-1)), each
  difference non-negative, the sum a u64
- `one_core_micropercent` = floor(100000000 x delta_cpu_ticks x 1000000000 /
  (hz x delta_mono_ns))
- `allocated_micropercent` = floor(100000000 x delta_cpu_ticks x 1000000000 x
  1000 / (hz x delta_mono_ns x allocated_vcpu_milli)), from the full numerator,
  not from the rounded one-core value

All arithmetic is checked wide-integer (`BigInt`). There is no Number
conversion, no hard-coded 100, no nominal 60 seconds, no clamp (two busy cores
exceed 100000000), no wrap or modulo repair, no reset-to-zero stitching and no
nearest-prior interval. A rate that does not fit a u64 is left null and the
record is flagged `INVALID_SCHEMA`; it is never saturated. An overflowing
allocated rate does not remove a valid one-core rate; an overflowing one-core
rate leaves both null, because an allocated rate is only recorded beside the
one-core rate. A decreasing counter,
a non-increasing anchor, a changed identity or an unusable neighbour yields no
interval and the matching flag. Missing slot i-1 yields `NONADJACENT_CPU`; slot
i-2 is never used.

Worked vector: hz 100, 3000 ticks over 60000000000 ns is 50000000 (50% of one
core); with 500 allocated milli-vCPU it is 100000000.

## Schedule

Slot i is due at T + 7200 s + 60 s x i, so all 120 fall in [T+2h, T+4h). With T
unset there are no scheduled times and nothing runs. CPU uses observed
monotonic anchors, never the nominal step. A bracket outside the half-open
60-second slot is `OUTSIDE_SLOT`; a start that is not exactly due is
`JITTER_UNACCEPTED` (no jitter or skew tolerance is defined, so none is
invented). No catch-up sample is ever taken.

## Failure vocabulary

Thirty-two bits; multiple causes survive.

| Bit | Name | Set when |
|---:|---|---|
| 0 | `MISSING` | Reader: the exact key is absent. Unreadable is `FAILED_READ`, not this. |
| 1 | `DUPLICATE` | Any second submission of an index, including an identical retry. |
| 2 | `OUT_OF_ORDER` | First arrival is not slot 0, or an arrival is below the previous one. |
| 3 | `CPU_BASELINE_ONLY` | Slot 0. |
| 4 | `COUNTER_RESET_OR_WRAP` | A counter decreased or the delta sum is not a u64. |
| 5 | `IDENTITY_UNKNOWN` | Any provenance field is null. |
| 6 | `IDENTITY_CHANGE` | Identity differs from the frozen descriptor or the adjacent record. Stops the run. |
| 7 | `CLOCK_UNKNOWN` | Clock kind unavailable, or anchor null or zero. |
| 8 | `CLOCK_RESET` | Adjacent anchors not increasing, or anchor before process startup. Stops the run. |
| 9 | `CLOCK_DISAGREEMENT` | Wall end before start, or wall start earlier than the previous slot's. |
| 10 | `OUTSIDE_SLOT` | Bracket not inside [due, due + 60 s). |
| 11 | `JITTER_UNACCEPTED` | Wall start is not exactly due. |
| 12 | `SOURCE_UNAVAILABLE` | Observation null, `unavailable` or incomplete. Stops the run. |
| 13 | `FAILED_WRITE` | Both bounded attempts were definitively rejected, or a permission denial. |
| 14 | `WRITE_OUTCOME_UNKNOWN` | Acknowledgement absent and not resolved by the one allowed read. |
| 15 | `FAILED_READ` | A read errored, timed out or was refused. |
| 16 | `READ_MISMATCH` | Persisted bytes, hash, index, run or claim disagree. |
| 17 | `TRUNCATED` | Reader stopped early, or a fixture receipt set reached its cap. |
| 18 | `COLLECTOR_STOP` | Stopped before the remaining slots. |
| 19 | `BYTE_BUDGET` | A key, value, input or total byte bound was exceeded. |
| 20 | `ROW_BUDGET` | A key or index outside the fixed key set. |
| 21 | `EVENT_BUDGET` | Ordinary incident capacity (31) is used up. |
| 22 | `CONCURRENT_WRITER` | Another writer or run owns the scope. No takeover. |
| 23 | `INVALID_SCHEMA` | Bad type, unit, range, unknown key, canonical form or overflow. |
| 24 | `PRIVACY_REJECTED` | Forbidden or identifying material. |
| 25 | `CPU_HZ_UNKNOWN` | Tick frequency or the limits receipt is null. |
| 26 | `CPU_WIDTH_UNKNOWN` | Counter width is not a receipt-backed 64. |
| 27 | `VCPU_UNKNOWN` | Allocation or the limits receipt is null. |
| 28 | `NONADJACENT_CPU` | Slot i-1 is absent or unusable. |
| 29 | `DEADLINE` | An observation reaches T+4h, or the reader budget runs out. |
| 30 | `AUTHORITY_MISSING` | T or a required authority reference is missing, or a permission denial. |
| 31 | `OPEN_OR_UNSEALED` | The terminal seal or writer manifest is unavailable. |

## Writer

One run, one fixed key space, one serial writer. Keys are
`two-bot:b2proc:v1:<run_alias>:` followed by `descriptor`, `record:000`..`119`,
`event:00`..`31`, `writer-seal`, `manifest:writer`, `manifest:reader` or
`reader-seal`: 157 keys, no listing, no range scan.

1. Validate the descriptor, T and (for `staging`) all nine authority references
   before any storage access. An unarmed or unauthorized run makes **zero**
   calls and yields a manifest of 120 not-attempted entries.
2. Validate each slot as a closed object, derive flags and the CPU interval from
   the committed neighbour only, canonicalize, then create the key
   (create-if-absent; never overwrite).
3. At most two attempts per key. A first permission denial is **absorbing**: no
   retry, no alternate credential, and **no newly initiated I/O afterwards**,
   including terminal incident, seal and manifest writes. Already-started work
   settles inside its own budget. After an ambiguous timeout, one same-key
   reconciliation read is allowed only when read authority is separately
   granted: identical bytes confirm one commit plus a `duplicate_retry` incident;
   different bytes stop the run; anything else stays `WRITE_OUTCOME_UNKNOWN`.
4. A retry with the same measurement is a duplicate-retry incident and never a
   second sample. A changed payload for a committed index is a conflict: the
   original bytes stay and the run stops.
5. Stop triggers: any denial, missing authority, schema or privacy violation,
   budget, duplicate conflict, concurrent writer, identity or clock reset,
   source failure, observation reaching T+4h, failed or unknown write, or an
   explicit stop. A stopped writer refuses every later slot without I/O, even
   one requested before it opened. There is no resume, restart, reset, takeover
   or catch-up. Once slot 119 is committed there is nothing left to stop; a
   later submission is only a duplicate retry or conflict.
6. After a non-denial stop or after slot 119, write the reserved terminal
   incident (ordinal 31, only when stopped), the writer seal and the writer
   manifest. `finish` runs once; a second call returns the first result and
   touches nothing. A crashed writer leaves no seal; the reader reports that.

## Independent reader

The reader receives only a store of persisted bytes (a serialized snapshot of
them), the run alias and its own alias, which must be the descriptor's
`reader_alias`. It reads the descriptor, writer seal, writer manifest, 120
record keys and 32 incident keys: one attempt per key, each at most 2 seconds,
300 seconds overall. A denial or the time budget ends reading at once and
leaves the rest `not_attempted` with `TRUNCATED`. After a read denial the
reader writes neither its manifest nor its seal. A write denied for the
manifest leaves no seal; a write denied for the seal leaves a committed
manifest without one.

It recomputes each record hash and rechecks aliases, index, schedule, intrinsic
flags and (where a persisted interval exists) the CPU arithmetic from the two
persisted neighbours. A neighbour that is missing or unverified marks the
dependent slot `NONADJACENT_CPU` and removes it from the qualified count. It
also compares the writer's claims and seal against what it read: a record that
contradicts its claimed hash or status, a seal that names a different last
record, or a claimed incident count that differs from the incident keys read is
`READ_MISMATCH`, and adverse writer facts in the claim (duplicate, failed or
unknown write, stop, budget) are carried into the reader's flags so a lost
incident cannot make a flagged run look clean. A persisted record may not omit
a flag its own adjacent pair requires, except when the record arrived before
its predecessor (persisted arrival order shows that), because the writer then
held no pair to judge. The reader never uses
the writer's memory, hashes alone, a re-scrape or sorted estimates. With no
readable descriptor there is nothing to bind a manifest to, so none is
produced. The reader writes its seal only after this call commits its manifest. A
second reader's manifest write finds the key taken, so it writes no seal and
leaves both stored outputs untouched. A manifest that fails, stays unknown
after a timeout, or is denied also leaves no seal; an unknown manifest may
still have landed.

**Coverage.** `unknown` without authority (T unset or references missing), for a
writer claim with every slot committed, or for a run that is merely open (no
seal and no claim, or no claim) with nothing else wrong. A seal removed while
the claim still names it is a `READ_MISMATCH`, so that is `incomplete`.
`incomplete` for any loss, mismatch, unverified entry or blocking flag.
`complete_structural` only for an independently verified, sealed, fully present
run whose flags are limited to `CPU_BASELINE_ONLY`, `JITTER_UNACCEPTED`,
`CPU_HZ_UNKNOWN`, `CPU_WIDTH_UNKNOWN` and `VCPU_UNKNOWN`. Even then it says
nothing about memory, CPU health, errors, jitter acceptance or the live bot.

## Bounds

| Class | Keys | Max value bytes each | Total |
|---|---:|---:|---:|
| Descriptor | 1 | 4096 | 4096 |
| Records | 120 | 4096 | 491520 |
| Incidents (31 ordinary + 1 terminal) | 32 | 1024 | 32768 |
| Writer seal | 1 | 4096 | 4096 |
| Manifests (writer, reader) | 2 | 32768 | 65536 |
| Reader seal | 1 | 4096 | 4096 |
| **Total** | **157** | | **602112** |

Key bytes are at most 157 x 128 = 20096, so the accounted logical maximum is
622208 bytes, below the 1048576-byte run budget (426368 spare). This is a
**logical** bound; physical overhead of any real backend is unknown and
`physical_storage_bytes` stays null. Other limits: proc status input 8192 bytes,
proc stat input 4096, fixture file 262144, aggregate parser and serializer
scratch 131072 (the modelled working set is 58880). One run, one fixture, one
writer, one reader; no second namespace, spill, paging or takeover; no delete
or overwrite exists in the model.

## Retention

A sealed run keeps a 14-day inspection lease after which it is frozen: no new
collection or read without a separate disposition. Nothing is deleted at the end
of the lease, a retained run blocks another run, and restoring eligibility never
fills missed slots or resumes a stopped run.

## Interpretation choices

Where the contract allowed more than one reading, the model takes the
conservative one:

- A usable CPU pair needs all nine provenance fields, a receipt-backed 64-bit
  width and the Linux monotonic clock. A missing limits receipt therefore also
  leaves tick frequency, width and allocation unqualified.
- Arrival order is slot 0 first, then strictly increasing. A first arrival that
  is not slot 0, or an arrival below the previous one, is `OUT_OF_ORDER`. A
  skipped slot is a gap, not disorder; the slot after it has no interval.
- An unavailable source stops the run after writing its tombstone.
- `NONADJACENT_CPU` is decided by the persisted bytes at read time, so deleting
  slot k removes the deltas of both k and k+1.

## Proof catalogue

`wrangler/test/process-history-contract.test.ts` generates each indexed family
across its whole domain (no first, middle or last sampling of a named family;
a few extra reader-binding probes use slots 0, 59 and 119 on top of that): missing,
duplicate (identical and conflicting), out-of-order (slot k first, and a late
arrival of each slot e), skipped slot, counter reset (user,
system, wrap) for slots 1..119, identity change for every frozen field at slots
1..119, stop before each slot, failed and ambiguous writes, failed and
mismatched reads, and denial at every slot. It also covers null provenance
field by field, unit and parser vectors, exact arithmetic, clock and slot
cases, byte caps at the limit and one above, the 157-key and 622208-byte
budgets, event capacity, concurrency, privacy, all 32 flag bits (a ledger
requires each bit to have been produced by a scenario), restoration and a
no-import, no-live-call scan. A distinct reader is built from serialized
persisted bytes after the writer is discarded.
