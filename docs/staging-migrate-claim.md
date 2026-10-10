# Staging migration apply-claim contract

This is non-secret request transport for the staging migration protection rule,
not an approval, a deployed-rule receipt or permission to apply migrations.
Existing required reviewers, independent exact-source review, recovery/ACL
checks and written CEO GO remain required. Production has its own mirror:
`production-migrate.yml` with `scripts/ci/production_migrate_claim.py`, which
binds the same projection hash but requires the production workflow path,
environment, `migration_target` and `PRODUCTION_HOST`/`PRODUCTION_DATABASE`
pins; a staging claim never satisfies it and vice versa.

## Available before environment approval

`staging-migrate.yml` has a `claim` job named `staging-migrate (claim)`. It runs
only for an apply dispatch on `main`, after the read-only `plan` job succeeds.
It has no environment, no database binding and only `contents: read` permission.
It checks out `github.sha` (the workflow revision), downloads this dispatch's plan
artifact and runs `scripts/ci/staging_migrate_claim.py`. Inputs enter through
step environment variables, not shell interpolation. Refusal fails the job;
there is no continue-on-error or upload-on-failure path.

The protected `apply` job needs **both** `plan` and `claim`. This means the claim
exists when the deployment-protection rule is asked to decide, unlike a step
inside apply, which cannot execute until approval. GitHub documents that
[protection rules gate jobs referencing the environment](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments).

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
| `target` | object with bare non-secret `host` and `database` |
| `expected_pending` | strictly ascending array of canonical positive i64 **decimal strings** |
| `recovery_evidence_ref`, `acl_plan_ref` | bare evidence references, not credential-bearing URLs |

The publisher compares the request's source, target, pending set and approval
references against the **current apply dispatch's read-only plan**, recomputes
its projection and requires embedded == computed == requested digest. This does
not authenticate the separately referenced prior plan run. That is mandatory
consumer work, not a promise inferred from the artifact name or a numeric ID.

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

## Required independent consumer checks

The deployment-protection rule must fail closed unless fresh, attempt-scoped
GitHub reads establish all of the following:

- The request is for the configured repository, workflow, main ref, dispatch
  event and `staging-migrate-apply` environment, with the expected pending gate.
- The claim belongs to that apply run/attempt and to its successful unprotected
  `staging-migrate (claim)` prerequisite; its workflow-head SHA agrees with the
  fresh run identity. Artifact absence, expiry, ambiguity or API failure refuses.
- The prior plan is a **different**, successful `staging-migrate.yml`
  `workflow_dispatch` on `main`, with a successful plan job and plan-mode
  evidence (not an already-approved apply run relabelled as a plan).
- Its authenticated `staging-migrate-manifest` artifact is read-only plan mode
  and binds the exact reviewed source, target, pending set, recovery/ACL refs.
  Its projection must be recomputed losslessly and equal embedded == claimed.
- Independent CEO GO approves this exact verified binding; hash equality or
  a completed adverse security review is not approval.

The runner still downloads the producing run's manifest and repeats its
pending/hash/provenance checks **before DDL**, after the environment gate.
The pre-approval claim supplements that check; it neither replaces it nor
weakens RO/apply credential separation, main-only protections or bash/pipefail.
