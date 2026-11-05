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
  A closed single-item ASCII RSS fixture grammar now compares all four selected
  item fields directly with the body, including an explicit UTC `pubDate`.
  The entire document must match the fixed declaration/root/channel/item field
  order; duplicate/missing/extra fields or items, attribute shadowing, namespace
  variants, DTD/entities, CDATA, comments, processing instructions, fallback
  identifiers and noncanonical XML refuse without invoking an XML engine. This
  is deliberately narrower than C, not permission to run arbitrary XML. The
  frozen C and implementation-base `crates/core/src/feeds.rs` share blob
  `f5148aaa0faf7767474ca8da344006f262122e92`. A shared three-case golden corpus
  is consumed by Python claim tests and a Rust parser/planner test in Actions;
  that is a hermetic comparison of the shipped parser, not a served observation.
  **This is not authenticated source or transport/selection evidence.** Actual
  served-C fetch/parse, immutable hosting, externally enforced request/response
  caps and full stored-case semantics remain mandatory. No URL is fetched, no
  general XML is parsed/executed, no source is authenticated and no public
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
  Business value semantics, actual setting persistence, setup/cleanup phase
  assignment, separately scoped recovery authority, actual desired restoration postconditions and
  queue/drain evidence remain mandatory. The synthetic fixture intentionally
  leaves every effect at one instant, not a successful lifecycle. Runtime
  admission remains unsupported and no SQL or live adapter is introduced.
- `scripts/announcement_runtime_settings.py` composes the transition and scheduler
  comparisons with two closed restricted setting-plan/value-projection sidecars.
  The input-plan digest binds four exact setting operation IDs and six distinct
  before/intermediate/final value projections, each tied to the literal K key.
  Absent or explicit JSON string-bit (`"0"`/`"1"`) prior values are the only
  supported baseline projections; null, numeric/boolean and other prior values
  refuse, never become boot fallback or a guessed restoration target. Present
  state value digests must equal the canonical JSON **string** value digest.
  The baseline, enable and disable values must be exactly zero/one/zero; the
  restored and separately observed final value/presence must equal the original
  projection without resetting a present row's advancing version. All four
  mutation IDs/methods/order and the latest committed state/version at each
  off/on/replay/final-off reader must agree. A value change before the claimed
  poll finishes cannot borrow an earlier snapshot as suppression evidence.
  **This is not persistence, full-row restoration, actual SQL CAS or stored-case
  proof.** Positive row versions here describe legacy row-version claims, not C's
  distinct negative database-owned `cas_version` tokens. Settings audit/revision
  trigger effects, actual SQL/reader snapshots, mutation-phase assignment,
  separately scoped cleanup/recovery authority and full business/lifecycle
  acceptance remain mandatory. Only setting writes are spread across the new
  fabricated timeline; other effects still occupy one instant and the baseline
  shares rollout's timestamp. No successful lifecycle is claimed. No value
  projection is a trust root or runtime permission; admission stays unsupported.
- `scripts/announcement_runtime_cases.py` composes scheduler/input and telemetry
  comparisons for the exact four feed cases plus the finite declared interactions.
  Expected/actual operation IDs must resolve, agree and be disjoint across cases;
  each wire/DB attempt and applicable platform readback must fit its case window,
  candidate served reference and exact resource set. The window end is exclusive.
  Off/replay/final-off feed cases allow scoped reads only, not a business post or
  delivery write. The on case needs source reads followed by claim → post →
  completion in full-window sequence order. Off/negative interactions require
  exactly one ACK and no business effect; on-RSVP requires ACK → membership/event
  pre-reads → atomic RSVP/audit → post-reads → original-response edit. Required
  business and announcement transport operations cannot disappear outside cases
  even if global totals match. **These are case-membership/order comparisons,
  not stored-data acceptance or request/response authentication.** Payload bodies,
  ephemeral visibility, exact interaction/platform IDs, stored business values,
  source parser selection and live suppression/delivery remain unproved. The
  fabricated fixture deliberately retains setup/cleanup at one instant and
  placeholder transition state/authority refs; it is not full-lifecycle proof.
  Synthetic/setup/cleanup operations still need their own exact semantic checks.
  No schema limit, live adapter, canonical gate or runtime refusal is changed.
- `scripts/announcement_runtime_interactions.py` adds one closed restricted
  aggregate exchange record, content-bound through the interaction/input/resource
  and authority plans. The token-free request projection binds interaction,
  application, guild, channel, consenting actor, command and closed options; its
  canonical JSON digest must match the interaction's `request_sha256`. This
  digest is explicitly **not a hash of a raw token-bearing Discord envelope**.
  Every declared interaction has exactly one exchange; its ACK/edit operation
  IDs must match the case. Claimed route classes bind callback POST versus
  original-response PATCH at the fixed Discord origin without storing a URL
  containing an interaction token. Exact body-byte digests must match the effect
  ledger. Duplicate/unknown JSON fields, followups, non-ephemeral flags, mentions,
  extra payloads and borrowed operations refuse. The narrow profile supports the
  eight off-command refusals, final-off refusals, and one first RSVP with a raw
  numeric event ID/status: deferred ephemeral ACK then the source-backed saved
  response text in an original-response edit. Negative/replay/pasted-link or
  changed-response profiles remain unsupported. Readback projections bind the
  exact application/channel/content/ephemeral flag/platform ID; ACK and edit must
  concern the same original message. The initial deferred readback requires
  `EPHEMERAL | LOADING` (192), while the outbound callback, completed edit and
  immediate refusals require only `EPHEMERAL` (64). Request-received time and
  observer must fit the case/full-window observer claims and precede the ACK.
  Claimed initial ACK attempts three or more whole seconds after receipt refuse;
  original-response edits do not inherit that deadline. This coarse necessary
  comparison is not proof of actual Discord deadline compliance.
  Each exchange also carries a closed finite eligibility-read list: empty for
  off refusals; exactly member/event pre-read then member/event post-read for the
  first RSVP. Exact GET origin/path/fixture IDs and empty request-body digests
  must match the operation ledger. Minimal response JSON permits only
  `user.id` for membership and `id`, `guild_id`, integer `status` for events;
  wrong echoes, cancelled/unknown events, non-200 responses, malformed/duplicate
  or extra fields refuse. Statuses 1 through 3 match C's predicate (including
  completed events); no invented future-event-only rule is imposed. Each read's
  independent-observer claim and response time must precede the next dependent
  read/write/success edit. All four reads bind distinct same-case operations;
  off cases cannot borrow them. These restricted minimal bytes are content-bound
  claims, not proof of actual HTTP response bytes or faithful projection of a
  normal richer Discord response. Arbitrary member profiles/event metadata and
  alternate positive/negative profiles remain unsupported.
  **These remain unauthenticated projections, not proof of real requests,
  transport routing, ephemeral visibility or persistence.** Other request kinds,
  complete raw-envelope-to-projection validation, actual observers, callback
  secret enforcement, stored business values and source-parser equivalence of
  every serialization are still missing. No token or arbitrary raw Discord
  response may be serialized into these records. No restricted record may be
  serialized into public artifacts. Unsupported serializers
  refuse rather than gain an extension object. Runtime remains HOLD.
- `scripts/announcement_runtime_feed_exchange.py` adds one closed restricted
  `feed_post_exchange` sidecar, content-pinned by the source plan's mandatory
  `post_exchange` pin, for the single RSS feed post. It composes the source and
  case comparisons, then requires exactly one `feed_post` operation (no borrowed
  or unknown operation), a fixed `POST https://discord.com/channels/<feed
  channel>/messages` route and a raw JSON body whose bytes hash to the effect
  and platform payload digest. The body must be exactly source-derived content
  (C's hyphen-escaped bold title plus URL), the selection nonce, boolean
  `enforce_nonce`, and the complete empty `allowed_mentions` policy; duplicate,
  unknown, missing or extra fields and bool/int/string/null confusion refuse.
  The projected response must be HTTP 200 with the same message ID as the
  accepted platform claim, author equal to the application ID, the feed channel,
  the same content and nonce, zero flags and no mentions or attachments. Channel,
  application and projected message IDs must fit the nonzero `u64` range, not
  merely a 20-digit service-ID pattern; oversized mutation receipts are not
  successful C responses. This narrow projection supports HTTP 200 only, not
  every success status accepted by C. Its received time must equal the platform
  observation, lie between the post
  and the delivery completion CAS and before the case end, and its observer
  must be the coverage observer. These are unauthenticated projections: no
  request is sent, no message is read back, and response loss, ambiguous
  history, retry and repost remain unproved or unsupported. The profile covers
  only the validated one-item ASCII RSS grammar; components, embeds, polls,
  attachments, replies and other formats are unsupported. The checker is
  composed as the fifteenth explicit claim comparison, returns no capability and
  the source-only fixture keeps a placeholder pin, so partial packets still do
  not satisfy all-evidence-used accounting. Runtime remains HOLD.
- `scripts/announcement_runtime_lifecycle.py` compares the existing setup/cleanup
  DB ledgers to exact typed transition claims and operation sequence. Setup must
  follow the served-before snapshot and precede rollout; cleanup follows the
  observation window and precedes the restored sample. Every DB write outside
  the test window must belong to one ledger, including writes at the exclusive
  observation end. Final database presence/value projections must restore the
  prior projections and agree with the last observed transition's full state
  and commit timestamp. A new version may restore a present value; an unobserved version change cannot
  be accepted as an unchanged resource. Message cleanup is explicitly unsupported.
  **This is a scoped ledger comparison, not a complete lifecycle verifier.** It
  must be composed with the authority, transition, resource, telemetry and
  provenance checks. It does not consume opaque drain/recovery/postcondition
  references, prove queue quiescence, validate actual SQL/CAS tokens or acquire
  separately scoped recovery authority. A fabricated coherent ledger returns no
  capability; unresolved full-packet evidence remains adverse.
- `scripts/announcement_runtime_drain.py` resolves four closed work-inventory,
  snapshot, fence and drain claim kinds alongside the lifecycle-ledger comparison.
  The declared finite surface inventory is content-bound to the scoped inventory
  manifest, C/T and source manifest; every baseline, drained and final snapshot
  must enumerate exactly those surfaces with zero queued, in-flight, claimed and
  delayed work. Drain must finish before cleanup. A full-window external-observer
  job/producer, exact gap-free coverage claim and a closed-through-end external
  fence claim must agree on attempt, inventory and epochs. Observer subjects
  cannot overlap authors, callers, controller, resource owners or candidate/control
  writers. Missing/extra surfaces, nonzero work, late operations or unresolved
  outcomes refuse. This narrow post-cutoff projection supports only the DB writes
  already in the cleanup ledger: additional trusted recovery reads/wire operations
  remain unsupported until separately typed and scoped, not implicitly exempt.
  **These are declared quiescence comparisons, not proof of actual drain.** No
  complete source-to-surface inventory, authenticated observer/fence, actual
  queue/request resolution, cancellation or recovery authority is established.
  Recovery, postconditions and full-packet accounting still need independent
  semantics; coherent fabricated claims grant no capability. Nothing starts,
  stops, deletes or restores a process/resource, and runtime admission is unchanged.
- `scripts/announcement_runtime_policy.py` checks offline lease/freshness,
  ordered same-scope reviewer claims, operation multisets and coverage claims.
  The lifecycle accounting model requires the complete snapshot-through-verified
  sequence and treats abort as terminal for qualification.
- `scripts/announcement_runtime_delivery.py` adds a separate offline one-item
  accounting model for closed claim → wire-attempt → known-accepted-message →
  known-DB-completion → unchanged-replay declarations. Every event binds the same
  run/attempt/control, literal-key reference, nonce reference and claim; wire and
  acceptance events also pin the same operation. Message IDs must be canonical
  nonzero u64s. The model counts the declared first wire attempt before its
  outcome and a repeated in-scope attempt before refusing it. A timeout, unknown
  history/completion, pre-wire failure, abort, wrong scope/order/message or malformed
  event permanently makes this object adverse; catching a refusal never reopens
  it. Neither a history miss nor cleanup/reset/recovery input can authorize repost.
  Immutable snapshots retain known counts, including a known acceptance when DB
  completion is unknown. They stop at the first refusal and are **not full-window
  telemetry or final real-effect totals**. This is a narrower qualification model
  than C's recovery logic, not an implementation of that recovery. It has no
  authority, persistence, external fence or transport. Reconstructing a new object
  grants no new attempt or nonce use; successful completion never grants admission.
- `scripts/announcement_runtime_delivery_trace.py` connects that model to a closed,
  source-plan-content-bound restricted `feed_delivery_trace` sidecar, composed as
  the sixteenth explicit CLI comparison. It requires the same literal delivery-key
  reference, nonce and fresh-claim operation, and compares each model event with
  an actual-effect **claim** in the on/replay cases. Acceptance names the same
  post/response/message; completion and replay declare identical partial delivered
  row projections. Replay needs a separately budgeted delivery `db_read` operation,
  not merely an idle poll or history miss. Observer/time and full-window sequence
  comparisons reject substitutions and intervening delivery writes, including
  release/reclaim before replay. Up to sixteen closed events are parsed; only the
  exact five-event successful model sequence can complete. Extra/missing/reordered
  events refuse, as do unknown outcomes in the upstream ledger.
  These pending/delivered projections contain only state, literal nonce and (when
  delivered) message ID. They are **not complete stored rows or authenticated SQL
  results**; claim-token fencing, before/after value correspondence, actual replay
  reads/history/response-loss reconciliation and later cleanup/restoration remain
  mandatory independent work. No new read or mutation is performed or authorized.
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

## Offline claim-checking entry point

`scripts/announcement_runtime_check.py` composes bounded archive intake,
closed same-run typed sidecars, all sixteen existing public claim comparisons
and final all-used sidecar accounting. It is **not an independently provisioned
verifier, receipt producer or runtime admission path**. The local Git topology
collector is not invoked; authenticated source/service inventories, complete
business/lifecycle semantics and final authority/identity rereads remain missing.
Even complete sidecar consumption would not prove that every opaque receipt
reference resolves or that any claimed observation actually happened.

From the reviewed tooling checkout, credential-free offline inspection is:

```sh
python3 scripts/announcement_runtime_check.py --archive candidate.zip --inventory inventory.json
```

The separate inventory is a closed JSON object with exactly `run` and `members`.
`run` has exactly `run_id`, `attempt` and `control_sha` in the receipt's canonical
formats; `members` has 1–257 entries with exactly `path`, `size` and `sha256`,
matching the existing archive inventory. It binds all archived bytes and must
match the receipt's run. It is **caller-supplied unauthenticated data**, not
post-upload service inventory, approval or a verifier root. No endpoint, root,
credential, alternate clock, allowlist or enable flag is accepted.

Both inputs must be nonempty regular files: the archive is at most 32 MiB and
the inventory at most 1 MiB. Descriptor-based intake refuses symlinks, FIFOs,
devices and directories before reading; JSON/archive bounds still apply. No
member is extracted, imported or executed, and no artifact is written. Always
run the tool from reviewed tooling, never from an untrusted extraction directory.

Every normal inspection invocation emits only a fixed sanitized JSON HOLD report
and exits **1**, whether intake/claims refuse or all current comparisons agree.
The latter still calls the unchanged unconditional `runtime_preflight()` refusal;
even an accidental future preflight return cannot enable this offline CLI.
Unexpected checker failures also remain adverse without echoing paths, input
values or tracebacks. `--help` describes the offline interface, not admission.
No command here performs a live probe, build, setup, credential use, rollout,
effect, cleanup, restoration or promotion.

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
   binds restricted URL/body/item claims and a closed single-item RSS grammar,
   but still needs actual served-C parser/fetch observations, immutable hosting
   and independent transport evidence.
   The scheduler comparison is scoped offline consistency, not a stored-data proof.
   Resolve exact interaction request contents and actual ACK/edit wire records;
   live interaction replay remains explicitly unsupported. Implement full stored
   off/on/replay/final-off semantics and negatives. The scoped telemetry checker
   compares wire/platform/transaction correspondence and RSVP/audit grouping.
   The case checker adds exact operation membership, phase/resource/time/served
   comparisons and positive feed/RSVP operation order. Neither authenticates
   observation, exact interaction IDs, ephemeral wire bodies, real stored business
   values or SQL atomicity, nor resolves the full lifecycle. The transition checker
   now resolves DB state chains,
   exact ownership witness claims and each write's effect-batch authority reference.
   The setting checker compares a narrow value projection, mutation order and
   reader/poll state correspondence; neither establishes real CAS, full SQL row
   restoration or actual stored-data acceptance. The lifecycle-ledger comparison
   adds setup/cleanup operation assignment and prior-value projection checks,
   not independently verified restoration or separately scoped recovery authority.
   The feed-exchange comparison adds only the restricted post/response
   projection above; real transport, response-loss and repost reconciliation,
   authenticated Discord evidence and all-path enforcement are still missing.
   The drain comparison adds declared finite-surface/empty-snapshot and closed-fence
   correspondence, not a complete source-to-surface inventory or actual quiescence.
   Complete the remaining lifecycle phases and bind every actual mutation to its correct lifecycle-phase
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
