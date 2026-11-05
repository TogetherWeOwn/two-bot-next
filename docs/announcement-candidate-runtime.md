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
  layouts. Non-receipt evidence stays **unverified inert bytes** until a
  separate typed resolver is invoked; its filename or hash is not acceptance.
- `scripts/announcement_runtime_evidence.py` resolves closed, same-attempt
  provenance sidecars. Each has exactly `schema`, `ref`, `kind`, `run`, `data`;
  `schema` is `two.bot.announcement-evidence.v1`, the filename must match its
  reference, and each supported kind has a fixed field inventory. Document and
  manifest pins compare both revision and SHA-256. Job/step/check/producer,
  build/isolation, external-served/observer, gate-sample/assertion, installation
  mapping and artifact records have distinct types; references cannot alias
  another type. Unsupported kinds, missing references and unconsumed extra
  evidence refuse. Intake shares receipt JSON limits, permits at most 256
  sidecars and bounds their combined bytes to 32 MiB. Resolved records are
  isolated copies, not mutable caller objects. This is typed **claim**
  resolution, not authenticated root acquisition or a complete resource graph.
- `scripts/announcement_runtime_provenance.py` checks frozen source identity,
  run/control/attempt equality, manifest revision pins, installation mapping
  consistency, candidate build/image/Worker modules, credential-free build
  claims, rollout ordering, external served-pair claims and exact gate
  job/sample/assertion relationships. Candidate observations must be bracketed
  by matching candidate served samples. Served-before and restoration samples
  retain their own identities rather than being inferred from main or C;
  baseline/restoration may use S, and schema/role checks must precede rollout
  rather than falsely claim C is already served. The merge-base commit and tree
  are both frozen. C and its source PR cannot be the reviewed installation head
  or PR. Snapshot, candidate and restoration must concern the same application;
  each immutable Worker version must have one consistent package definition.
  The attempt must fit an unexpired
  at-most-60-minute lease and its observation window at most 20 minutes.
  These checks perform no service read and authenticate no installation,
  producer, isolation boundary, observer or manifest contents. The opaque
  observer provisioning reference remains an explicitly unsupported external
  requirement, not a root selected from a sidecar.
- `scripts/announcement_runtime_authority.py` adds offline typed authority-claim
  resolution. The content-bound scope compares exact source/control/code/policy,
  resource/identity/operation-plan pins, image/components, epochs and the full
  principal/source/consumer mapping. Seven distinct subjects have fixed role
  claims; runtime and installation decisions bind separate native stage and
  participant mappings, ordered Security→QA, exact scope and Security round.
  Both consumers must claim separate provisioning and matching complete
  effective decision histories at each mandatory checkpoint. Every effective
  decision must already exist at its read/history timestamp. History snapshots
  are contiguous, time-bound and prefix-consistent; later snapshots cannot
  backdate omitted events into an earlier complete interval or revive an
  invalidated approval after push/revert, revocation, dismissal or scope change. Reusable
  installation approval is separately scoped and rechecked at every checkpoint;
  its decisions cannot drift behind the epoch. The one-attempt execution and
  nonce-consumption claims bind the run, owner, epochs, time and exact scope.
  Credential/setup/rollout/transition/effect-batch/final checkpoints are required;
  each resolves an exact-phase external-atomic-fence claim with at most 60-second
  validity. Rollout starts and recorded effect attempts must fall inside their
  matching checkpoint windows; final runtime and installation reads are fresh
  against the verifier clock, not just the checkpoint time. These are consistency predicates, **not** actual native API reads,
  provisioning, nonce consumption, current revocation protection or enforcement.
  There is no authority endpoint, key, adapter or enable input. A coherent
  fabricated graph still returns no capability and cannot qualify runtime.
  The transition checker below adds scoped DB state/ownership/checkpoint claim
  binding; complete lifecycle mutation-to-checkpoint assignment remains unfinished.
  Composition with provenance/all-used accounting does not replace that or any
  remaining acceptance predicate.
- `scripts/announcement_runtime_resources.py` compares content-bound resource,
  identity-map, ownership, state, isolation and per-method budget **claims**.
  Six new closed sidecar kinds carry those records and the feed-item binding.
  The fixed `runtime-resource-plan` and `runtime-identity-map` contents must hash
  to the exact document pins; entries/states must match the receipt without
  aliases, extra slots or unresolved resource references. Resource kinds have
  method ceilings, not a generic caller allowlist; per-method finite budgets
  must sum to the per-resource totals and at most 512 attempted operations.
  The prospective positive profile binds one feed post, one RSVP insertion and
  audit, one delivery claim/completion, and four separately counted setting
  mutations. Expected setting mutations cannot disappear behind larger maxima.
  Discord object types, one application/guild, DB table/key digests and a common
  DB endpoint/role/grants/trigger boundary are compared. Prior delivery/RSVP/audit
  claims must be absent; existing Discord/source objects must be present.
  Owner records bind the controller, attempt execution and nonce; state records
  bind their resource/identity/owner and before/after windows. Isolation claims
  must precede rollout, deny production memberships/raw credentials/application
  flags and use the resource epoch. Their observer claims bind the separate
  verifier provisioning/principal, same-attempt successful independent job and
  producer. Synthetic dependency objects must be finite and disjoint from the
  announcement fixtures. Both effect ledgers must have the exact method counts.
  **This is not containment, literal-key authentication or complete effect
  verification.** It performs no service read, authenticates no test IDs, and
  does not resolve consent, scheduler/interaction/synthetic-cleanup plans,
  transitive inventories, operation contents, wire/DB receipts, stored-case or
  full restoration semantics. Coherent fabricated graphs still return no
  capability. The fixtures explicitly omit those qualifications.
- `scripts/announcement_runtime_inputs.py` adds scoped **restricted** literal-key,
  consent and finite interaction-plan claim comparisons. Four closed sidecar
  kinds define table-specific primary keys, test consent, interaction plans and
  input-plan contents. The resource plan hashes the fixed `runtime-input-plan`;
  each referenced key/consent/interaction record is content-bound in that plan.
  Every modeled database resource must have one unique complete typed key whose
  table and digest match its identity. Delivery keys bind the relay/item; K binds
  the guild and exact `TWO_ANNOUNCEMENTS` key; RSVP binds the guild/event/actor,
  and its audit ID is the source-backed `rsvp:<interaction ID>`, not a check-in ID.
  Consent claims bind the exact actor/application/guild, execution/nonce, full
  attempt window and finite case set. Interaction claims bind the case/phase,
  consenting actor, application, control channel and event; live-off covers all
  eight command names. The scoped positive interaction is exactly one RSVP with
  one ephemeral ACK and one original-response edit. Live-off/negative/final-off
  interactions get one ephemeral ACK and no business edit; followups, excess wire
  retries, duplicate interaction IDs and positive feed CRUD are refused. Live
  interaction replay is explicitly unsupported, never assigned a fresh budget.
  These totals must match the resource plan's exact ACK/edit budgets. No literal
  key or consent record is authenticated, and no request payload or actual effect
  is proved. This code must **not** serialize restricted records into public
  artifacts; no public producer exists yet. Scheduler/transitive inventory,
  source URL/content, stored cases, actual wire/DB records and lifecycle remain
  mandatory separate checks. Consistent fabricated records return no capability.
- `scripts/announcement_runtime_scheduler.py` compares four closed, content-bound
  scheduler/activation/setting-reader/poll claim kinds. The scheduler plan hashes
  through the restricted input plan into resource/authority scope. Declared
  unchanged feed cadence is store-first (60–86,400 seconds) or an explicit absent
  interval row falling back to boot, not a boot-multiple approximation. The
  contiguous off/on/replay/final-off windows must cover the observation window,
  stay within 20 minutes and allow at least one normal poll interval each. A
  too-long cadence refuses; no interval mutation, grace or lease extension exists.
  Activation claims precede rollout and cover the attempt with the declared
  external fence/epoch and registered boot-enabled feed job. Stored announcement
  reads must be explicit strings, match row/version/snapshot/cadence and bind an
  eligible-item poll and candidate served pair. Null/absent rows cannot prove off.
  Changed values need advancing row/snapshot generations; replay can retain the
  unchanged snapshot. Cases must fit their phase's boundary and window. These
  comparisons do **not** authenticate an activation fence, stored read, normal
  cadence, eligible poll, send/suppression or any observation. Full source,
  transitive/synthetic inventory, operation telemetry and lifecycle resolution
  remain mandatory. Runtime admission is still unconditionally unsupported.
- `scripts/announcement_runtime_source_inputs.py` resolves four closed restricted
  source-plan/stored-relay/body/selected-item claim kinds. The source plan hashes
  through the input/resource/authority scope. A deliberately narrower ASCII RSS
  profile requires a canonical HTTPS DNS origin and exact query-free path, no
  IDNA labels/credentials/ports/redirects/alternate encoding, and at most 4,096 UTF-8 response
  bytes. The origin and path identities hash their exact UTF-8 spelling; the body
  digest and byte count compare actual inert text, not equal arbitrary hashes.
  This bound fits the existing scalar limit; it does not raise C's fetch limits
  or silently authorize general feeds. Stored-relay claims bind exact row-key,
  guild/channel/source, enabled state, pre-rollout time and served-before pair;
  their observer/job/producer claims use the separate verifier identity. The
  selected-item claim has an exact content digest and C-derived item-key and
  delivery-nonce comparisons: SHA-256 of the nonempty ASCII raw key, then the
  first 24 hex characters of SHA-256 over relay ID, NUL and that **derived hash**.
  Narrow title/URL checks compare the resulting RSS post-content digest, including
  escaped title hyphens; unsupported trim/Unicode/markdown/URL forms refuse.
  Source wire-attempt ceilings must equal the exact finite method budget.
  **This is not a feed parser or a transport/selection observation.** A coherent
  claim that selects an item absent from the body does not prove parser output;
  exact C parser/source immutability, externally enforced request/response caps,
  actual source requests and full stored-case semantics remain mandatory. No URL
  is fetched, no XML is executed/parsed, no source is authenticated and no public
  serializer exposes these restricted bytes or literal nonce. Runtime admission
  stays unconditionally unsupported even for coherent fabricated records.
- `scripts/announcement_runtime_telemetry.py` composes provenance and resource
  checks with five closed restricted telemetry-window/wire/platform/transaction/
  transition sidecar kinds. One exact full-attempt coverage claim must match the
  run, enforcement epoch, inventory digest and ordered operation IDs. Subwindow
  or multiple coverage records refuse rather than imply complete coverage. The
  observer's matching successful external-observer job must span the entire
  attempt and its principal cannot be any declared writer/author/executor/caller
  or resource-owning controller, even if that owner has no producing job.
  Every non-DB operation has exactly one content-bound wire-attempt claim; every
  applicable message/ACK/edit/command-replace operation has matching platform
  identity/content/resource/nonce readback. DB operations each belong to one
  committed transaction; writes bind one exact owned key/count/method transition.
  The positive RSVP and audit inserts must share exactly one transaction; its
  additional budgeted reads are reconciled, while unrelated writes refuse. Missing,
  extra, aliased, stale, blocked, unknown or inconsistent records refuse; matching
  aggregate counts or a nonempty unrelated receipt list cannot substitute.
  Reference lists retain the 256-entry bound, so this narrower window profile
  refuses larger operation-ID lists rather than raising the contract's limits.
  **These are comparisons of claims, not authenticated telemetry, atomicity or
  all-path enforcement.** Exact request bodies/routes/ephemeral flags, literal
  platform identity semantics, transition state/ownership/authority evidence,
  source parser/transport, stored cases, triggers, lock/drain/cleanup/restoration
  and final all-used accounting remain mandatory separate checks. No observer,
  credential, DB transaction, external fence or public serializer is created.
  Consistent fabricated telemetry still cannot pass runtime admission.
- `scripts/announcement_runtime_serialization.py` composes provenance and resource
  comparisons with three closed serialization plan/participant/lock sidecar kinds.
  The resource-plan content digest binds the complete declared job graph, writer
  list, parent pointers, acquisition names and witness references. Every declared
  job has exactly one node. A single controller orchestrator holds `deploy-staging`
  for the entire attempt; writer paths include a distinct `staging-lifecycle`
  leaf, and observers/credential-free builds cannot acquire either lock. Cycles,
  missing parents/jobs/writers, same-lock ancestor acquisitions, parallel leaf
  reacquisition, interference, stale/mismatched fences and uncovered intervals
  refuse. Standalone synthetic writers cannot bypass the declared leaf path.
  The verifier-bound observer job must span the entire attempt; its subject cannot
  overlap any candidate/control producer, controller, author, executor or caller.
  Each participant interval must fit every ancestor and held lock, and each lock must match its
  acquiring controller job's entire interval. Multiple/reacquired leaves remain
  unsupported in this narrow model. **This is a comparison of declared graphs,
  not installed distributed locking, authenticated writer inventory or actual
  all-writer coverage.** The graph cannot establish that an omitted external
  writer honors the fence, map every operation to its actual producer, prove
  lifecycle phase completion, or resolve queues/drain/cleanup/restoration. Those
  remain mandatory. No workflow concurrency, canonical gate or installer changes
  are made and coherent fabricated witnesses grant no runtime capability.
- `scripts/announcement_runtime_transitions.py` composes authority and telemetry
  comparisons with a closed `ownership_match` sidecar and existing typed resource
  state records. Every DB write must extend the preceding complete state in exact
  operation sequence, including intermediate presence/version/value claims.
  Insert requires absence, CAS requires presence and an advancing version, and
  owned deletion requires presence. Intermediate state refs and compare-and-match
  witnesses cannot be reused. Each witness compares the exact operation, key,
  identity, owner, execution/nonce/epochs, method, snapshot hashes, independent
  full-window observer and timestamp. The transition must reference its own
  declared effect-batch checkpoint and fall inside that checkpoint's validated
  external-atomic fence; an unrelated fresh checkpoint is insufficient. Final
  separately observed DB states must match the chain's full terminal state,
  including version, not just its value digest. **These are claim comparisons,
  not actual CAS, authentication, phase completion or verified restoration.**
  Setting/business value semantics, setup/cleanup phase assignment, separately
  scoped recovery authority, actual desired restoration postconditions and
  queue/drain evidence remain mandatory. The synthetic fixture intentionally
  leaves every effect at one instant, not a successful lifecycle. Runtime
  admission remains unsupported and no SQL or live adapter is introduced.
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
2. Complete typed input, effect, telemetry and lifecycle resolution beyond the
   provenance/authority, resource and scoped restricted-input claim comparisons.
   Authenticate restricted literal keys, consent and identity maps; bind complete
   transitive inventory and synthetic-cleanup plans. The source-input comparison
   binds restricted URL/body/item claims but still needs exact C parser selection,
   actual source requests, immutable hosting and independent transport evidence.
   The scheduler comparison is scoped offline consistency, not a stored-data proof.
   Resolve exact interaction request contents and actual ACK/edit wire records;
   live interaction replay remains explicitly unsupported. Implement full stored
   off/on/replay/final-off semantics and negatives. The scoped telemetry checker
   compares wire/platform/transaction correspondence and RSVP/audit grouping,
   but does not authenticate observation, prove SQL atomicity or resolve the
   full lifecycle semantics. The transition checker now resolves DB state chains,
   exact ownership witness claims and each write's effect-batch authority reference,
   but does not establish real CAS or desired restoration. Implement setup/cleanup
   phase assignment, independently verified restoration and separately scoped
   recovery authority. Bind every actual mutation to its correct lifecycle-phase
   authority checkpoint. The scoped resource graph and
   method-count checks alone are not a complete receipt-verification result.
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

Full suites run on GitHub Actions in the existing discovery lane. The following
commands describe that CI coverage, not permission to mirror it on the shared
controller. Local execution is limited to one focused failing-test/crate
reproduction under the current build policy.

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
