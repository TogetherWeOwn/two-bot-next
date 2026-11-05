# Announcement candidate runtime controls

Status: **partial offline implementation; live execution HOLD**. The effective
runtime admission set is empty. No resources or live effects are authorized.
This tooling concerns the announcement candidate in [PR #1639](https://github.com/TogetherWeOwn/two-bot-next/pull/1639),
not canonical staging qualification or production promotion.

## Implemented boundaries

- `scripts/announcement_runtime_schema.py` defines the closed sixteen-key
  `two.bot.announcement-candidate-staging.v1` receipt syntax. It requires
  `purpose: premerge-evidence` and `promotable: false`, closed typed nested
  objects, eight gate names and eight command names. JSON intake rejects
  duplicate/unknown keys, invalid Unicode, impossible UTC timestamps, float/bool
  counts, unsupported enum values, unsafe module paths and unresolved sentinel
  values. Bounds are 1 MiB per receipt, depth 12, scalar text 4,096 UTF-8 bytes,
  reference lists 256 and individual effect/transition ledgers 512.
- `scripts/announcement_runtime_archive.py` inspects, but never extracts or
  executes, an exact inventory of `candidate-receipt.json` and bounded
  `evidence/<reference>.json` members. Compressed and expanded archive bounds
  are both 32 MiB. It rejects unlisted/missing/replaced/duplicate members,
  canonical-promotion filenames, traversal, links, executable modes, encrypted
  entries, alternate compression, comments and inconsistent local/central ZIP
  layouts. Non-receipt evidence stays **unverified inert bytes** for later
  typed resolution; its filename or hash is not acceptance.
- `scripts/announcement_runtime_policy.py` checks offline lease/freshness,
  ordered same-scope reviewer claims, operation multisets and coverage claims.
  The lifecycle accounting model requires the complete snapshot-through-verified
  sequence and treats abort as terminal for qualification.
- `scripts/announcement_runtime_source.py` observes complete local Git-object
  inventories without checkout, fetch, import, filter or build execution. It
  verifies raw commit/tree/blob hashes and validates raw canonical tree modes
  before Git can normalize them, records every regular source path as
  reversible hex bytes with mode, object ID, size and SHA-256, and computes
  unfiltered additions/deletions/content/mode deltas. Local collection refuses
  shallow/promisor/replaced/grafted history, symlinks, submodules, unsafe paths,
  missing objects and exhausted bounds. It separately labels M→C, historical
  B0→C, B0→T and served-S→C comparisons; unbound T/S are not inferred. Offline
  topology predicates require the frozen C/B0, singleton merge-base M, all
  required M ancestries, same-repository open PR and base/main equality to T.
  Source inventories are limited to 16,384 regular entries, 32 MiB per object,
  256 MiB per tree and unique object bytes per collector, 4,096 bytes per raw
  path, 30 seconds per child and 120 seconds per collector. Object sizes are
  checked before body reads. The Linux collector uses existing `/usr/bin/git`
  and `/usr/bin/prlimit`; each Git child has a 512 MiB address-space ceiling
  and 30 CPU-second limit. Missing tools or limits fail closed; no packages
  are installed. These are local intake limits, not a change to
  receipt/archive limits or runtime budgets. A complete
  local inventory is not authenticated remote identity, reviewed packaging,
  build isolation, installed T or evidence of actual served S.
- `runtime_preflight()` always raises `Refusal("unsupported")`, before any
  credentials or effects. It has no caller adapter, enable flag, root URL,
  allowlist or credential parameter. No mutation, deployment, recovery or
  certificate-emission implementation is installed by these modules.

**Syntax is not truth.** A structurally valid document can contain fabricated
claims, repeated reference IDs, impossible cross-object relationships or an
unsupported profile. A successful consistency predicate is not authenticated
independent authority, external containment or an observation. These modules
must not be called as a substitute for any canonical admission control.

## Capability gaps and accountable roles

The code's fixed `CAPABILITY_GAPS` ledger records these absent capabilities:

| Capability | Accountable role |
|---|---|
| Authenticated complete controller authority and independent verifier root | Engineering Lead |
| Atomic current authority/resource epoch fence and trusted one-use nonce | Engineering Lead |
| Credential and all-path egress mediation outside candidate code | Engineering Lead |
| Database key/operation/trigger boundary below candidate code | Engineering Lead |
| Complete transitive startup, job, retry and queue inventory | Engineering Lead |
| Independent full-window telemetry and exact literal resource/synthetic plan | Engineering Lead |
| Full-window lifecycle fence honored by every staging writer | Engineering Lead |
| Legitimately reviewed/installed tooling and bootstrap | Existing installer owner |
| Candidate-compatible readiness clearance | Existing readiness owner |

The ledger is not configurable and clearing a task or passing a unit test does
not remove a gap. No new proxy, service, trust root, identity, credential, resource
allocation or installation workflow is introduced. Application flags, ordinary
bot permissions and broad shared database credentials cannot replace the missing
external enforcement.

## Remaining implementation before exact-final review

This checkpoint is not the complete runtime-candidate lane. It still needs:

1. Independently authenticated source/tooling/component manifest bindings,
   approved T/actual served-S provenance, build isolation and packaging checks.
   The local full-tree/delta collector and offline topology predicates do not
   establish independent review or any of those external facts.
2. Closed typed resolution of every evidence reference, full cross-object and
   same-attempt identity comparisons, exact resource/profile budgets, stored
   off/on/replay/final-off semantics, atomic RSVP/audit evidence, owned CAS
   cleanup and independently verified restoration.
3. Strict receipt production and independently provisioned verification,
   including authenticated current service artifact/job/producer inventories,
   post-upload immutable artifact binding and final authority/identity rereads.
   Neither a caller inventory nor a copied receipt may select the verifier root.
4. Existing-lane integration without widening the separate docs/CI-only
   installer and complete finite refusal-matrix coverage, including independent
   candidate-verifier identity/provenance adversaries. The canonical-admission
   rejection tests below do not implement candidate verification. Readiness and
   full-window exclusion semantics must stay unchanged.
5. Complete exact-head implementation manifests, source-to-control traceability,
   independent Security then QA review, CI and applicable release/installation
   approvals. None is supplied by this checkpoint.

Unsupported live adapters must continue to refuse. Completing offline code does
not itself authorize bootstrap, candidate admission, credentials, binding changes,
resource setup, probes, sends, database operations, staging execution or merge.

## Hermetic checks

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_announcement_runtime_*.py' -v
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_immutable_bot_promotion.py -v
```

The new tests run in the existing `scripts/ci/test_*.py` discovery lane. Fixtures
are synthetic syntax/accounting claims, explicitly not live observations or
approval evidence. The canonical staging/production workflows, promotion tool,
main-ref/main-ancestor requirements and main-only environment policies remain
unchanged. `test_announcement_runtime_nonpromotable.py` passes candidate bytes
through the real canonical admission command using the existing offline service
fixture. Canonical naming/producer metadata, mixed packets, changed envelope
literals, wrong candidate claims and a hypothetical postmerge main identity all
refuse before image copy or admitted-file output. These tests prove rejection
by canonical admission, not positive acceptance by a candidate verifier.
Candidate packets remain non-promotable, including after source merge.

REA angle: reuse the existing candidate tooling boundary, immutable-promotion
intake conventions and source-backed feed/RSVP semantics. Do not create another
installer or infer containment from application configuration.
