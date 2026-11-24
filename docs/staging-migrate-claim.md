# Staging migration apply-claim contract

The claim is non-secret evidence of the staging apply request, not an approval
or permission to apply migrations. The staging gate combines the `main`-only
Environment branch rule and native reviewer approval. See the
[staging migration runbook](runbook.md#staging-only-migration-runner) for recovery
and ACL prerequisites. Production has its own approval and protection rules:
`production-migrate.yml` with `scripts/ci/production_migrate_claim.py`, which
binds the same projection hash but requires the production workflow path,
environment, `migration_target` and
`PRODUCTION_HOST`/`PRODUCTION_DATABASE`/`PRODUCTION_BRANCH_ID` pins (a
PlanetScale plan without the pinned branch id, or a request pin that does not
match the plan, refuses); a staging claim never satisfies it and vice versa.
Both gates require the explicit `migration_target` the producer emits
(`staging` here, `production` there); a field-less or wrong-target manifest
refuses with fixed vocabulary, and the marker sits outside the projection
hash. The host denylists are intentionally mirror-image rather than
identical: the staging gate refuses prod-like hosts and databases, while the
production gate refuses staging-like hosts (staging label, Neon, test
database, pooler). Each gate therefore refuses the other's label-bearing
canonical test host; label-free targets such as identical PlanetScale hosts
are distinguished by the explicit marker, workflow identity and pin match,
not the denylist. The explicit-target acceptance tests pin both
directions.

## Required before the apply job

`staging-migrate.yml` has a `claim` job named `staging-migrate (claim)`. It runs
only for an apply dispatch on `main`, after the read-only `plan` job succeeds.
It has no environment or database binding. It uses `actions: read` for plan provenance.
It checks out `github.sha` (the workflow revision), downloads this dispatch's plan
artifact and runs `scripts/ci/staging_migrate_claim.py`. Inputs enter through
step environment variables, not shell interpolation. Refusal fails the job;
there is no continue-on-error or upload-on-failure path.

The `apply` job needs **both** `plan` and `claim`. In `staging-migrate-apply`,
allow `main` only, name independent maintainers with read access as **Required
reviewers**, enable **Prevent self-review** and disable administrator bypass.
Keep direct owner approval until the reviewed workflow and protections are active.

## Exact artifact names and v1 document

- Plan artifact: `staging-migrate-manifest`; file: `staging-migrate-manifest.json`.
- Apply claim artifact: `staging-migrate-apply-claim`; file:
  `staging-migrate-apply-claim.json`.
- Both uploads use `compression-level: 0` and 14-day retention. The ZIP reader
  must bound its input and validate ZIP metadata, including data descriptors;
  no compression does not guarantee the absence of streaming descriptors.
  [Upload-artifact documents the compression input](https://github.com/actions/upload-artifact#altering-compressions-level-speed-v-size).

The JSON object carries these fields, with no database URL or credentials:

| Field | Value / type |
| --- | --- |
| `schema_version` | integer `1` |
| `kind` | `staging-migrate-apply-claim` |
| `repository` | `TogetherWeOwn/two-bot-next` |
| `workflow_path` | `.github/workflows/staging-migrate.yml` |
| `environment_name` | `staging-migrate-apply` |
| `apply_run_id`, `apply_run_attempt`, `plan_run_id` | whole positive decimal strings, 1–20 digits; no partial match or floating-point conversion |
| `workflow_head_sha` | `github.sha`, the workflow run's full lowercase 40-hex revision |
| `source_sha` | reviewed migration source, full lowercase 40-hex; can differ from workflow head |
| `plan_manifest_sha256` | lowercase 64-hex Rust projection digest |
| `store_plan_manifest_sha256` | lowercase 64-hex Rust store-chain projection digest |
| `target` | object with bare non-secret `host`, `database` and `branch_id` (empty for Neon) |
| `expected_pending` | strictly ascending array of canonical positive i64 **decimal strings** |
| `expected_store_pending` | strictly ascending array of canonical positive i64 **decimal strings** for the store chain |
| `recovery_evidence_ref`, `acl_plan_ref` | bare evidence references, not credential-bearing URLs |

The publisher compares the request's source, target, both pending sets and
recovery/ACL references against the **current apply dispatch's read-only plan**.
It recomputes both projections and requires embedded == computed == requested
for each digest. This does not authenticate the separately referenced prior plan
run; the reviewer must do that before dispatch.

Pending input follows the runner's trimming and integer parsing: surrounding
whitespace is ignored, whitespace-only input means no pending migrations, and
leading zeros or a leading `+` normalize to canonical positive i64 strings.
Duplicates, descending values, empty entries, overflow and manifest mismatches
still refuse. Database pins accept letters, digits, `_`, `-` and `.` within the
63-byte bound (for example `two-bot-staging`); production-like and URL-like names
still refuse. Evidence references use the runner's bare-reference rules: at most
200 UTF-8 bytes, no whitespace, `@` or `://`. Punctuation such as
`acl-review#decision(v2)` is preserved, not normalized; exact manifest/request
reference and target equality remain mandatory.

## Rust digest serialization

`crates/cutover/src/staging_migrate.rs::manifest_hash` hashes these UTF-8 bytes,
using LF newlines, including the final LF:

1. `source_sha + "\n"`.
2. Each `pending_before` i64 in order, decimal plus `"\n"`.
3. **All up source migrations**, including already-applied migrations, in source
   order: `version + ":" + description + ":" + sha384 + "\n"`.

Use `source_migrations[].sha384`, not `sha384_recomputed`. This is not JSON
serialization, not a digest of the enclosing self-describing manifest, and not
an upload-artifact ZIP digest. Target, role and approval references are outside
this projection, so comparing the hash alone cannot verify them.

The store chain (`crates/store/migrations/`, ledger
`public._two_bot_migrations`) binds through a second projection of the exact
same shape: `source_sha`, then `pending_store_before`, then **all up store
source migrations** as `version:description:sha384`, each plus LF. The claim
carries it as `store_plan_manifest_sha256` with `expected_store_pending`;
the runner recomputes both projections and proves both against the producing
run's manifest before any DDL. The cutover projection is unchanged.

The shared synthetic vectors are:

- `crates/cutover/tests/fixtures/staging-migrate-plan.json`: full runner-shaped
  read-only plan; two synthetic SQL migrations, UTF-8 description and an i64
  above JavaScript's safe-integer range. Rust verifies its migration fields and
  projection against the actual `source_manifest` / `manifest_hash` functions.
- `crates/cutover/tests/fixtures/staging-migrate-apply-claim.json`: exact publisher
  output, with an eleven-digit prior run ID and pending versions as strings.
  The Python CLI test regenerates it byte-for-byte, independently of the consumer.

These vectors are not a live successful plan, a verified target, or GO approval.
A JavaScript consumer must parse numeric i64 tokens losslessly before comparing
or hashing; `JSON.parse` into ordinary numbers silently rounds the vector's
`9007199254740993`. Do not repair a rounded number after parsing it.

## Verify the plan before dispatch and the claim after dispatch

Before dispatching apply, use fresh, attempt-scoped GitHub reads to verify a
successful plan-only `staging-migrate.yml` dispatch on `main`: the plan job
succeeded and the claim and apply jobs did not run. Authenticate that run's
`staging-migrate-manifest`; check the source, target, both pending sets and
recovery/ACL references. Recompute both projection digests losslessly and
compare them with the embedded digests. Do not dispatch if the artifact is
missing, expired, ambiguous or cannot be authenticated.

Before submitting `mode: apply` on `main`, compare the actual dispatch inputs
with the authenticated producing manifest and its run ID. Match the source SHA,
both pending lists, both digests, the pinned target (`staging_host`,
`staging_database`, `staging_branch_id`) and both evidence references
(`recovery_evidence_ref`, `acl_plan_ref`). The digests do not cover the target or
either reference. Do not dispatch if any value differs or a hold applies.

The designated maintainer checks the plan, recovery and ACL evidence against
the claim. In **Review deployments**, select `staging-migrate-apply` and enter
`approve-staging-migration sha256:<digest>` from the claim summary. The digest
binds the full claim and producing artifact ID and archive digest.

Apply checks approval within 23h30m of dispatch. Retry or change inputs with a new dispatch and approval.
Reuse a plan only when the apply dispatch's fresh read-only plan matches both ledgers and audit findings.

## Authorize one apply through a protected merge

In a reviewer-free Environment, make one new request file the sole change in a T0 PR.
Use `.github/staging-apply/<32-lowercase-hex>.json` with decimal string `plan_run_id`
and `sha256:` digests `plan_archive_digest` and `recovery_archive_digest`.
The authenticated plan binds the source, target, both ledgers and pending sets,
verified staging capture and ACL reference. CI validates the request fixture.

Obtain independent general and security T0 approvals on the exact request PR
head, including explicit approval of staging apply and its private recovery proof.
An independent operator verifies both reviews and uses the existing TogetherWeOwn
App with a token narrowed to repository Checks write to publish the approval.
Create a completed successful `staging-migration-approval` check run on that head,
with external ID `staging-apply:<PR number>`. Keep private evidence out of GitHub.
Squash-merge the pinned head after the App check and required CI pass. The workflow
authenticates the App approval, protected merge and exact request blob before apply.

The merge starts `staging-migrate-reviewed` once. It authenticates the producing
plan and capture, compares a fresh read-only plan, then uses the same scoped
staging apply Environment and honors any native reviewers configured there.
If it fails, reconcile both ledgers before preparing a new reviewed request.
GitHub reruns, expired requests and previously used request names refuse.
