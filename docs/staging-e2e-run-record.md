# Staging E2E run record

Every staging end-to-end run of two-bot-next is recorded in one template so
QA verdicts are comparable across runs: which revision was deployed, which
`deploy-staging` run put it there, which commands were exercised with
timings, pass/fail per command, failure-signature reference, tester
identity, cleanup result and the verdict. One run, one record. This is a
**staging-only** record: it never covers production.

## Files

- Template: copy this document's table (Human record) and fill every row.
- Machine-readable schema: `staging-e2e-run-record.schema.json` (same
  directory). The schema is normative when the two disagree.
- Validator: `python3 scripts/check_run_record.py --record <file.json>`
  (offline, standard library only). CI runs it against the schema plus the
  checked-in mock example on every push.
- Mock example: `scripts/fixtures/staging_e2e_run_record_mock.json`
  (clearly labelled `"mock": true`; it proves the shape, not staging).
- Generator for the read-mostly smoke: `python3 scripts/staging_smoke_run.py`
  writes a validated record (see
  [Live read-mostly smoke run](staging-slash-smoke.md#live-read-mostly-smoke-run)).

## What this template does not do

- It does **not** define failure signatures. A failing command row cites an
  opaque `failure_signature` ID into the separate failure-signature triage
  runbook; interpreting or triaging that signature is out of scope here.
- It does **not** parse evidence. Summarising gateway logs or soak evidence
  into verdicts belongs to the soak-evidence parser, not this record.
- It does **not** replace a QA verdict. The record is the evidence the
  verdict cites; the verdict itself is a `QA <head sha>: PASS` or
  `QA <head sha>: NEEDS WORK` comment with the record attached.

## Human record (copy and fill)

```markdown
# Staging E2E run: <run-id>

- Revision: <40-char commit SHA>
- Deploy-staging run: <numeric GitHub Actions run id>
- Worker version: <from the staging deployment receipt, when known>
- First /readyz 200 (UTC): <redeploy-gap measurement, when known>
- Guild: TWO Staging (staging guild id on file with the operator)
- Window (UTC): <start> to <end>
- Tester: <name, handle or role; never a token or credential>
- Plan: <which checklist rows or smoke scope this run covered>
- Mock: no (omit this line on real runs)

## Commands

| # | Command | Started (UTC) | Duration | Result | Expected | Actual | Failure signature | Evidence |
|---|---------|---------------|----------|--------|----------|--------|-------------------|----------|
| 1 | /rank | | | pass/fail/skipped | | | (fail only) | |
| 2 | | | | | | | | |

## Cleanup

- Result: restored / pending / not_applicable
- Notes: <what was restored through the normal UI/API, or what is pending and who owns it>

## Verdict

- Disposition: PASS / NEEDS WORK
- Summary: <one or two sentences: what passed, what failed, next step>
- Follow-ups: <public issue numbers or plain descriptions; no secrets>
```

## QA verdict work-product columns

The QA verdict work product for a staging run fills these columns. Every
column is required; write `unknown` only where the column explicitly allows
it:

| Column | What to fill | Example |
|---|---|---|
| `run_id` | Unique run label | `staging-e2e-2026-10-04-001` |
| `revision` | Exact deployed commit SHA (40 hex chars) | `c3efe26b…` (full SHA) |
| `deploy_staging_run_id` | Numeric `deploy-staging` workflow run id | `37154963396` |
| `command` | Command as invoked | `/rank` |
| `started_utc` | Invocation time, UTC ISO-8601 | `2026-10-04T00:12:03Z` |
| `duration_ms` | Invocation to observed reply, whole ms | `842` |
| `result` | `pass`, `fail` or `skipped` | `pass` |
| `expected` | One line: what the fixture should produce | `rank card for the fixture member` |
| `actual` | One line, sanitized: what was observed | `rank card shown; no other guild data` |
| `failure_signature` | Opaque runbook ID; required on `fail`, omit otherwise | `SIG-TIMEOUT-AUDIT-003` |
| `tester` | Who ran the commands (never a credential) | `QA & Release Engineer` |
| `cleanup` | `restored`, `pending` or `not_applicable` | `restored` |
| `verdict` | `PASS` or `NEEDS WORK` plus one-line summary | `PASS — 6/6 commands, fixtures restored` |

Rules the validator enforces: `revision` is 40 lowercase hex characters;
`deploy_staging_run_id` is digits; every command has a non-negative
`duration_ms`; every `fail` row carries a `failure_signature`; records with
`"mock": true` are rejected as real evidence (the mock example is the only
file allowed to carry it).

## Collection bounds

- Keys are opaque fixture aliases in the human record; raw member, channel
  and message IDs stay with the tester and never enter the record.
- No tokens, secrets, private URLs or internal tracker IDs in any record,
  filename or evidence pointer. Failed-auth details name the expected
  credential and the exact error only.
- A missing revision, missing deploy run id, missing timings or missing
  cleanup result is NEEDS WORK, not a silent waiver.
- A CI unit result alone does not prove a staging Discord effect; the record
  cites the staging run it describes.
