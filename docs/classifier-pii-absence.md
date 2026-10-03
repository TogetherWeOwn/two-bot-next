# Classifier PII-absence acceptance

Contract (`docs/audit-core.md`): classifier output carries IDs, role lists,
counts and flags — never message bodies, nicknames, usernames or free-form
moderation reasons. `AuditEvent.metadata_json` carries small classifier
context only.

## What pins it

`crates/core/tests/classifier_pii_absence.rs` uses the existing public
classify API only — no new production code, no DB, no Discord:

- `classify_member_update` + `MemberDelta::diff` / `change_digest`
- `classify_voice_boundary` + `VoiceBoundary::classify`
- `classify_raw_message` + `RawDispatch`
- `classify_moderation_audit` + `RawAuditLogEntry`

## Exercised: hostile text through the inputs that accept it

Two classify inputs take free text. Each gets a hostile value (snowflake-looking
digits, JSON braces, `@everyone`, a URL with a fake secret query) that must not
surface in any row surface: every serialized field value (unescaped, so a
field added later is covered), the decoded `metadata_json` keys and values,
`serde_json::to_string(row)` and the Discord mirror text from
`format_audit_event`. The check matches the whole string and each fragment, so
JSON escaping, mention neutralization or truncation cannot hide a leak.

- message-body shape through `RawDispatch.edited_timestamp`: a non-date value
  takes the shard/sequence fallback identity and the observed instant; it
  must not land in `entry_id`, `occurred_at` or anywhere else.
- moderation-reason shape through `RawAuditLogEntry.reason`: correlated rows
  verify the `[two-audit:v1:…]` marker and take `action`/`actor_id` from it,
  dropping the human suffix; uncorrelated rows keep only the action table
  string.

## API-unreachable: pinned by exact metadata key sets

Nicknames, usernames and message bodies have no parameter at all — a
type-level guarantee: `MemberDelta::diff` takes a nickname change flag, voice
boundaries take channel IDs, and `RawDispatch` has no body field. Instead of
asserting the absence of strings that never enter the API, each row kind pins
its exact metadata key set, so a future name/body/reason key fails whatever
the input:

| Row kind | Metadata keys |
| --- | --- |
| member update | `nicknameChanged`, `addedRoleIds`, `removedRoleIds` |
| voice join / leave / move | `isBot` |
| raw message delete / edit | none (`{}`) |
| moderation, uncorrelated | `auditLogEntryId`, `count` |
| moderation, correlated | `auditLogEntryId`, `count`, `origin`, `outcome` (+ `affected` only when a count is present) |

## Ordering and vacuous rows

- Role ids sort lexicographically as strings (`"10"` before `"9"`, legacy
  `[...set].sort()`), with an order-stable `change_digest` (golden
  `eG0n08KYgOucLcMM`) that survives into the member-update entry id.
- Empty / no-change inputs yield `None`: identical role sets, `(None, None)`
  and same-channel voice frames, non-dispatch / unknown-type / DM / empty-guild
  packets, and unknown audit-log actions.

## Verify

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test classifier_pii_absence
```
