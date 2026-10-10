# Container release rollback one-pager

Reference for the production container release: where the last good
release is pinned, the backout order, and how rollback is verified and
acknowledged. This page is **reference only, not approval to execute**.
Authority and the full procedure live in the linked docs. Keep member
data, dumps, payloads and logs in the restricted evidence location,
never in public GitHub or issue comments.

- Full rollback procedure: [cutover.md](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy).
- Ordered checklist, decider, triggers and time bounds:
  [cutover-rollback-runbook.md](cutover-rollback-runbook.md).
- Deploy/rollback dispatch and 48-hour watch log:
  [production-deploy.md](production-deploy.md).
- Forward order and per-step rollback pointers:
  [cutover-sequence.md](cutover-sequence.md).
- Single-gateway ownership order:
  [cutover-guard-checklist.md](cutover-guard-checklist.md).

## 1. Identify the previous good release

Read the pins from the execution card manifest and the watch header,
recorded at `T_0` (first `/readyz` 200 on the production revision) and
kept for the whole watch:

- Deployed commit: full 40-hex SHA on `main` with green `check`
  (`fmt`, `clippy -D warnings`, tests, `cargo-deny`), `worker check`,
  `pr-lint`, `gitleaks`, plus a successful staging run for that SHA.
- Worker pair: new version ID and **previous version ID**. The previous
  ID comes from the dispatch run summary and is the rollback command's
  `<version-id>`.
- Container pair: Next digest plus reviewed head, and the pinned legacy
  image digest plus commit with its configuration. The legacy
  Coolify image, configuration and recovery points stay warm for the
  whole 48-hour watch and are never removed at watch close.
- The production workflow dispatch with `takeover: true`
  ([runbook §5](cutover-rollback-runbook.md#5-cloudflare-revert-no-dns-change))
  is the single production rollback method; coverage:
  [runbook §7](cutover-rollback-runbook.md#7-staging-rehearsal-log)
  is a dry-walk with no executed dispatch, and the staging drill differs
  (unforced deployment, immediate Durable Object update).
  A Worker-version rollback does not rebuild the container image or
  rewind data. A standalone full redeploy of a known-good pair is
  superseded as a production rollback path: when the Rust image is the
  fault, dispatch the same workflow in deploy mode with `takeover: true`
  and the prior good SHA under the same guard, takeover order and
  `/readyz` build-identity gate; that deploy-mode path has no production
  drill record. Never roll back to a pre-fence wrapper version: it
  ignores the persisted ownership record.

## 2. Backout sequence (document order)

1. Declare rollback with UTC time and reason. Freeze all Next and web
   writers, activate and verify the persisted ownership fence
   **before** stopping Next, pause health callers, drain admitted work
   to zero, stop Next, and verify terminal state. Record rollback
   freeze `T_r`, final watermarks, and a preserved Next snapshot.
2. Reconcile **every write since the freeze baseline** with the tested
   per-table and per-key mapping, not a bare timestamp filter.
   Record counts, hashes, conflicts and the applied watermark.
   Maximum accepted loss is zero acknowledged committed writes.
   Never down-migrate or restore an old snapshot over additive schema.
3. Classify applied Discord effects from delivery, audit and replay
   receipts. Completed messages, sanctions, role changes and callbacks
   are not replayed or undone by a restore. Reconcile uncertain
   effects explicitly with moderators.
4. Reconcile the frozen live registry snapshot and approved watch
   edits against pre-swap and post-swap receipts, approve a reconciled
   target (never an automatic reset to the old baseline), apply it,
   reapply permission overrides through the authorized path, and read
   back every affected guild.
5. Restore the pinned legacy image and configuration with the
   reconciled database binding and the existing application token.
   Pass read-only legacy preflight, confirm the Next fence is still
   active and Next is stopped, then start **one** legacy gateway with
   its tested fresh-session procedure. Record first READY.
6. Resume producers and consumers once, in the recorded order.

## 3. Verify and acknowledge

- `/health` 200 and truthful `/readyz` 200 with the compiled revision
  matching the restored release; real event continuity against the
  moderator record; pending jobs reconciled; registry read-back with
  zero unexplained mismatches; post-rollback watermarks compared.
- Watch the recovered service for at least the measured drill
  recovery window. Announce restored ownership and record incident,
  loss, gap and reconciliation evidence on the execution card. Keep
  Next evidence intact, its fence active, and monitors pointed at
  legacy. A missed budget is an incident, never permission to skip
  reconciliation. Incomplete capture keeps maintenance closed and
  escalates a decision brief.
- Execution follow-up (the live drill and any real rollback) stays
  with the cutover executor under the all-warm rehearsal gate in
  [cutover.md](cutover.md#preconditions-all-must-pass) and the
  rehearsal log in [rollback runbook §7](cutover-rollback-runbook.md#7-staging-rehearsal-log).
  This page executes nothing.
