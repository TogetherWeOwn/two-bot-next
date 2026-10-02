# Classifier PII-absence acceptance

Contract (`docs/audit-core.md`): classifier output carries IDs, role lists,
counts and flags — never message bodies, nicknames, usernames or free-form
moderation reasons. `AuditEvent.metadata_json` carries small classifier
context only.

## What pins it

`crates/core/tests/classifier_pii_absence.rs` exercises the existing public
classify API only — no new production code, no DB, no Discord:

- `classify_member_update` + `MemberDelta::diff` / `change_digest`
- `classify_voice_boundary` + `VoiceBoundary::classify`
- `classify_raw_message` + `RawDispatch`
- `classify_moderation_audit` + `RawAuditLogEntry`

## Hostile inputs held aside

Each PII shape is built once and asserted absent verbatim from every row's
`metadata_json`, `identity()` and `entry_id`:

- display-name shape: snowflake-looking digits, JSON braces, `@everyone`, URL
  with a fake code query
- message-body shape: same family with a fake password query (the raw-message
  API never takes a body at all)
- moderation-reason shape: same family with a fake token query (correlated
  rows verify the `[two-audit:v1:…]` marker but drop the human suffix;
  uncorrelated rows keep only the action table string)
- username shape: same family (voice rows only take channel/member IDs)

A hostile non-date `edited_timestamp` takes the shard/sequence fallback
identity — the hostile text itself must not land in the entry id.

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
