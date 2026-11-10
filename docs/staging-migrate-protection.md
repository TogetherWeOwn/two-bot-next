# Independent staging migration protection rule

Status: implementation candidate, disabled by default. Not registered or live.
A merged consumer alone does not close the environment's protection-rule gap.

The independent Worker in `wrangler/src/staging-protection/` consumes the
[apply-claim contract](staging-migrate-claim.md). It is **not** part of the bot
Worker entrypoint, cannot read a database binding, and never performs DDL.
Production migration policy and `production-migrate-apply` are out of scope.

## Decision policy and evidence

The staging-only decision of 2026-11-07 selects automated approval instead of a
human required reviewer/CEO GO. This rule does not manufacture a human receipt.
Independent source review is retained through reviewed `main`; the rule also
checks source ancestry through GitHub's compare API. Recovery and ACL evidence
must already be approved by their legitimate principals before dispatch: bare
references are compared exactly across the claim and both read-only manifests,
not interpreted as proof that the external evidence itself is adequate.
Security acceptance of the implementation is required before enabling it.

The receiver authenticates the exact raw webhook bytes with HMAC-SHA256 before
parsing. Repository and installation IDs are pinned in configuration. Only a
`deployment_protection_rule` / `requested` delivery for `workflow_dispatch` on
`refs/heads/main`, the configured repository and `staging-migrate-apply` can
select a run. The callback must be the exact GitHub endpoint; its URL is never
followed. The consumer constructs all authenticated API paths itself.

Fresh GitHub reads must establish:

1. The current run is the main-only staging workflow, with a pending staging
   environment gate. The webhook SHA matches the run. The current attempt's
   plan and unprotected claim jobs completed successfully.
2. Exactly one unexpired claim and one current plan artifact exist. Each
   artifact's repository-run/head identity, creation inside the corresponding
   attempt-scoped producer job's interval, API digest, strict ZIP and JSON all
   verify. An artifact from an earlier attempt cannot be substituted.
3. The claim matches the exact current run/attempt/head and the configured
   staging host, database **and branch ID**. Both current plan projections are
   recomputed and compared with the embedded and claimed digests.
4. The named prior run differs, finished successfully before the apply dispatch,
   and has the same repository, workflow, dispatch event and main branch. Its
   successful plan job includes the successful **Confirm read-only plan
   dispatch** step. Both its claim and apply jobs must be skipped. A successful
   apply run with a plan-shaped artifact is not plan-mode evidence. Older runs
   without this marker fail closed; create a new read-only plan after rollout.
5. The independently fetched prior manifest binds the same source, full target,
   both pending sets, both projection hashes and exact recovery/ACL references.
   Both ledgers are unchanged, counts are zero, role is read-only and source
   checksums agree. All integer tokens are parsed as bounded `bigint` values
   before comparison/hashing, including `9007199254740993` in the Rust vector.
6. Immediately before the callback, the current run/attempt/head and pending
   gate are re-read. Any change invalidates the decision.

Missing, ambiguous, stale, malformed or failed reads reject; an inability to
make a safe callback leaves the gate pending, never approved. No API error body,
private key, installation token, webhook body or signed download URL is logged.

## Resource, credential and replay limits

- `PROTECTION_ENABLED` must be exactly `1`; the checked-in value is `0`.
- Webhook bodies are at most 64 KiB. JSON/artifact content is at most 1 MiB,
  nesting 32, numeric tokens 20 digits. ZIP input is at most 1 MiB + 4096 bytes.
  Only one exact stored entry is accepted; CRC, local/central metadata, optional
  descriptors and archive boundaries must agree. Extra files/fields, encryption,
  compression and ZIP64 refuse. Unknown archive layouts are not auto-relaxed.
- Runs/plans must start within 24 hours; the claim producer must have completed
  within 30 minutes. Future timestamps refuse. Fourteen-day retention is not
  freshness. Lists are capped at 100 and must be complete; pagination refuses.
- Each evaluation has a 25-second network deadline and at most 24 authenticated
  calls, with no retries. Token revocation has its own five-second deadline.
  Worker configuration caps CPU at 50 ms and subrequests at 40. No new paid
  service/plan is selected by the implementation.
- App JWT lifetime is five minutes including clock skew. Installation tokens
  are minted at call time, scoped to one pinned repository with Actions read,
  Deployments write and Contents read. They are not cached, persisted, returned
  or logged. A `finally` block attempts revocation after every minted-token use;
  if GitHub is unavailable, its one-hour expiry is the remaining upper bound.
  A failed revocation is an audit signal, not a claim of successful teardown.
- A Durable Object per run serializes deliveries. A durable write-ahead record
  per attempt prevents repeating an uncertain callback after a crash. A duplicate
  returns `duplicate`, not `approved`. Records expire after 25 hours, beyond the
  accepted run lifetime. An uncertain result needs inspection, not blind retry.
- Audit records contain only a fixed event/outcome, numeric run ID/attempt and
  `token_revoked`. Unsigned/refused ingress emits only a fixed refusal record.

## Threat model and residual trust

Assets: staging gate integrity, App signing material, scoped installation tokens,
webhook secret and bounded Worker resources. Boundaries: public ingress → HMAC;
verified delivery → exact repository/installation/environment; GitHub API →
attempt/job/artifact identity; artifact ZIP/JSON → lossless two-chain binding;
verified binding → staging-only callback. Inputs are untrusted at every step.

The rule trusts GitHub's signed deliveries/API, reviewed main workflow code,
operator-provided pins, the pre-dispatch recovery/ACL approvals and the approved
App's credential custody. Main write access or App key compromise remains outside
what artifact hashes can solve. Use the narrowest existing approved App; never
expand permissions silently. The rule cannot grant production approval, repair
missing database roles, override another rule, or attest to recovery quality.

A crash after the write-ahead record can leave a gate pending. This intentionally
favors denial over a second approval. Confirm approval history and logs, cancel
the affected apply dispatch if uncertain, and start a new dispatch with fresh
claim evidence. Do not edit Durable Object storage to erase the fence. Disabling
or rolling back the receiver must not remove the environment protection rule or
enable bypass; staging stays red if the consumer cannot decide safely.

## Deployment and registration (gated, not executed)

1. Record Security acceptance and exact-head Actions results. Verify the existing
   Cloudflare account includes this small separate Worker/SQLite Durable Object
   in its approved plan. If it needs a new paid service, stop for a spend decision.
2. Inspect existing approved GitHub Apps through the authorized App/broker path.
   Required permissions are Actions read, Deployments write, Contents read;
   subscribe to `deployment_protection_rule`. Prefer an existing App restricted
   to this repository. Creating/rotating a key or webhook secret needs the owner
   credential decision; approved minting/placement is an Operator step. No agent
   should fetch/copy broad credentials to perform registration.
3. The Operator configures `wrangler/wrangler.staging-protection.toml` with the
   exact approved App/installation/repository IDs and staging pins, keeping
   `PROTECTION_ENABLED=0`. Place `GITHUB_APP_PRIVATE_KEY` as a PKCS#8 PEM and
   `GITHUB_WEBHOOK_SECRET` through the approved secret-placement process, never
   command arguments, issue text or artifacts. Converting existing PEM encoding
   is not permission to mint another key. Keep these bindings out of bot envs.
4. Deploy this **separate** config from the reviewed commit, not `wrangler.toml`.
   No checked-in workflow automatically deploys it. Record Worker version and
   source SHA. Set the App webhook URL to the deployed Worker's
   `/hooks/staging-migration` path. Enable the approved installation's custom
   rule on **only** `staging-migrate-apply`; retain main-only branches and disabled
   bypass. Do not configure any production environment or human required reviewer.
5. Confirm that custom-rule-only gates appear in the pending-deployments API for
   this App and that the actual webhook ref/SHA shape matches the pinned parser.
   If not, keep the rule disabled and fix the evidence contract through review;
   do not infer pending status merely from a delivery. Capture a non-secret
   immutable artifact fixture to prove the ZIP/download layout and API digest
   match the supported envelope. This compatibility proof is required before
   enabling approvals; synthetic ZIP tests alone are not live-format proof.
6. Enable `PROTECTION_ENABLED=1` only after those prerequisites. Coordinate a
   fresh read-only plan followed by an authorized staging apply window. Record
   exact run/attempts, rule identity, pending-gate evidence, immutable artifact
   IDs/digests, decision/audit outcome and approval history. Exercise a
   deliberately mismatched request in a safe, authorized window: it must reject
   before apply starts. Existing staging database/migration authority still
   governs dispatch; registration is not permission to mutate shared data.
7. Only after the rule is installed and a real staging decision is verified may
   the owning task record that the runbook's required-reviewer gap is closed.

## Verification and references

The existing `worker check` Actions job runs `npm ci`, `npm run typecheck` and
`npm test` for `wrangler/**`. No new dependency or CI lane is needed. Synthetic
fixtures cover decoding, both projection chains, independent prior-plan checks,
API/download refusal, ingress authentication and repository/environment fencing.
Durable Object crash/replay and live-format registration evidence must also be
recorded before the candidate is called ready; they are not inferred from these
unit fixtures. CI is GitHub Actions, not a local full-suite transcript.

REA angle: reuse GitHub's supported
[custom-rule protocol](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/create-custom-protection-rules),
[review API](https://docs.github.com/en/rest/actions/workflow-runs#review-custom-deployment-protection-rules-for-a-workflow-run),
[webhook event](https://docs.github.com/en/webhooks/webhook-events-and-payloads#deployment_protection_rule)
and the existing Rust/Python vectors, rather than a custom approval surrogate.
