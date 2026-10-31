# Intended staging rollout acceptance

`deploy-staging.yml` runs `scripts/staging_rollout.py` before and after a
successful Wrangler deployment (the `deploy` job was re-enabled by
[TOG-12856](/TOG/issues/TOG-12856)). Order: ownership-control `preflight`,
`prepare`, Wrangler deploy, `receipt`, ownership-control `deployment-takeover`,
`verify`. `preflight` and `deployment-takeover` live outside
`staging_rollout.py`. Readiness cannot be proven before the takeover, because a
fenced or non-owner singleton refuses `/readyz`; the Wrangler receipt is local
and is therefore accepted before ownership moves. Any failed step after a
successful deploy triggers a conditional automatic
rollback attempt by the workflow: `deploy-staging.yml` runs
`scripts/worker_container_rollback.py rollback` when both the `deploy`
and `snapshot` steps succeeded and a later step fails (receipt validation,
ownership takeover, `verify`, or the error-rate gate);
`scripts/staging_rollout.py` itself performs
no rollback. On success the pre-deploy Worker version **and** container image
are restored (with takeover, which releases an intentional fence onto the
restored version after a fenced takeover refusal), and the run still stays red — the restore
recovers service, it never certifies the failed build. Without a successful
deploy and snapshot there is nothing to restore, so no rollback is attempted.
The gate does not
accept a health response from an old singleton. This is a **staging-only** gate,
not production authorization or a migration tool. Operators can preview the
ordered restore plan for a receipt without any mutation via
`scripts/worker_container_rollback.py rollback ... --dry-run`, which validates
the receipt identically to the live path and exits 0.

## Step order and why

1. `preflight` -> `prepare` -> `wrangler deploy` -> `receipt` ->
   `deployment-takeover` -> `verify`.
2. Once `wrangler deploy` succeeds the new Worker version is already at 100%
   traffic. Ownership transfer does not choose what serves; it lets that version's
   singleton run the bot.
3. `receipt` (steps 3-4 below) needs only Wrangler's output file, local Docker and
   the Worker versions API, so it runs **before** ownership moves. A malformed,
   stale or wrong-environment receipt therefore never hands ownership to an
   unverified version ([TOG-12939](/TOG/issues/TOG-12939): the first real run
   failed here, after takeover, because the receipt gate had never seen real
   Wrangler output).
4. `verify` (steps 5-7) needs an owning singleton: a parked singleton answers
   `/readyz` 503 by design, so rollout convergence and runtime readiness cannot be
   checked before takeover. `verify` repeats the receipt checks first.
5. If any step after a successful deploy with a
   successful snapshot fails (receipt validation, ownership takeover,
   `verify`, or the error-rate gate), the workflow attempts an automatic rollback to the pre-deploy
   snapshot (Worker version **and** container image, with takeover) via
   `scripts/worker_container_rollback.py`; `scripts/staging_rollout.py` only reports
   the failure. After a fenced takeover refusal the rollback's own takeover
   releases the fence onto the restored version. A restored deployment still leaves the job red on `main`: the rollback
   restores service, it never certifies the failed build. There is deliberately **no
   automatic permanent fence**: a successful restore ends with takeover (`active`), so
   the next push can hand off again without a manual `release_fence` dispatch. When
   the `deploy` or `snapshot` step did not succeed there is nothing to restore, so no
   rollback is attempted and staging stays owned by whatever version holds it. When the
   rollback step itself fails or its stable gate never verifies, treat staging as
   unverified — it may still serve the failed build or sit fenced. A red downstream
   `voice-synthetic` job is separate: it runs after the deploy job and never triggers
   a rollback. Before re-running the deploy (or dispatching it), inspect the
   `auto-rollback-evidence.json` in the `staging-rollout-evidence` artifact plus the
   current Worker identity (`/health` version header), `/readyz` readiness and
   ownership status, then fix the cause; production deploys are separate,
   manual and cutover-owned.

## Provenance chain

1. Before deployment, snapshot the existing application named
   `two-bot-next-twobotcontainer-staging`, its Durable Object namespace, and rollout
   IDs. An absent/ambiguous application or an in-flight prior rollout fails closed.
2. Generate a temporary JSON Wrangler config from the checked-in TOML. Keep the
   staging Container/DO wiring, use absolute source/build paths, and pass three
   non-secret Docker build arguments as `image_vars`: `BOT_BUILD_REVISION` (the
   exact `GITHUB_SHA`), `BOT_BUILD_ID` (the unique
   `GITHUB_RUN_ID-GITHUB_RUN_ATTEMPT`), and `BOT_DEPLOY_ID` (the
   `YYYY.MM.DD.N+sha7` deploy ID derived in-lane by the compute step). The Rust
   `/readyz` response compiles these values into the binary as
   `build_revision`/`build_id`/`deploy_id`; runtime environment
   overrides cannot substitute them. The `org.opencontainers.image.revision`,
   `com.togetherweown.build-id` and `com.togetherweown.deploy-id` OCI labels
   repeat them for narrowly scoped image inspection. A redeploy of the same SHA
   therefore still produces a new image identity. Prepare also requires the
   `CF_VERSION_METADATA` version-metadata binding in the staging config and sets
   `rollout_kind: full_auto`. After the gates judge, the record step stores the
   deploy as a GitHub Deployment with `environment` set to `staging` carrying
   the deploy ID — never a tag or Release — after the auto-rollback, so its
   failure fails the run red with the service already restored. Both ID steps
   run reviewed tooling from the workflow's own
   commit, so a pinned drill deploys a commit whose checkout may predate the
   script while the gate scripts stay on the deployed checkout.
3. Read the **fresh** `WRANGLER_OUTPUT_FILE_PATH` NDJSON only after Wrangler exits
   successfully. Every Wrangler invocation appends its own `wrangler-session`
   record, and `wrangler-action` runs `wrangler --version` before deploying, so
   the file holds several sessions. Require at least one session, all pinned to
   4.152.0 with record version 1, exactly one `deploy` invocation and every other
   session exactly `--version` or `-v`, and exactly one staging deploy record
   with the expected Worker name, fresh timestamp and concrete Worker version.
   The `receipt` step runs this check **before** `deployment-takeover`, so
   ownership never moves to a deploy whose receipt is invalid; `verify` repeats
   it. Raw session arguments/log paths are never published. A failure prints,
   after the fixed gate code, only an allowlisted diagnostic: record-type counts,
   session class counts (deploy / probe / other), each session's record/Wrangler
   version, and which deploy-record checks failed. A verify failure after the
   first control-plane poll also prints the last rollout/readiness observation
   (fixed vocabulary only).
4. Resolve that exact Worker version's `TWO_BOT`/`TwoBotContainer` namespace through
   the versions API and match it to the application. Find the unique local
   Cloudflare registry tag for the expected application and Worker UUID prefix;
   inspect **only** its two provenance labels and RepoDigests. Labels must match
   the build receipt, and the image must be an immutable `@sha256:` digest.
   Registry account namespaces are opaque; they are not assumed to equal the
   Cloudflare account ID. A missing/ambiguous tag or digest fails closed.
5. Select exactly one rollout created after the baseline, not present in its IDs,
   with that exact target digest. Never select `result[0]` or infer newest-first
   ordering. Pin the rollout ID for subsequent polling. Competing/multiple new
   rollouts, replaced/reverted states, unknown schemas and identity drift fail.
6. Require the pinned `full_auto` / `rolling` rollout to complete with finished
   steps, converged instance progress, and one active/healthy singleton with no
   failed/starting/scheduling instances. The rollout's `current_*` fields describe
   the **before** configuration, not an acknowledgement of the target. Verify the
   application's actual configuration digest separately. If the completed rollout
   is otherwise exact but the control-plane `active` counter still reads 0 while
   `healthy` reads 1, `verify` accepts it only after two consecutive full passes:
   each pass still requires the exact Worker at 100% traffic, `/readyz` 200 with
   all components ready and the exact compiled revision/build ID, `/health` 200
   with the exact Worker version, and a control-plane recheck that accepts either
   completed counter shape (converged or lag) with identity unchanged. Any
   non-passing poll resets the streak. The success evidence records
   `"active_lag": true` on this path. The application listing is read separately
   from the rollout record and can briefly trail it. A target digest in the listing
   is accepted normally; a stale listing is accepted only when its digest matches the
   completed rollout's verified previous-image digest, the listing has never shown
   the target during this verification, a final rollout-list read finds no newer
   rollout, and the exact Worker passes the runtime probes. On the active-lag counter
   shape, that listing-lag acceptance still requires two consecutive full passes.
   Successful stale-listing acceptance records `"application_listing_lag": true`.

   A stale listing that is not verified as the rollout's previous image fails as
   `application_image_drift`. For a verified previous-image listing that has not
   yet passed the runtime probes, `verify` keeps polling (last observation
   `application_image=stale`) up to twenty-four polls or until the 300s verify
   deadline, whichever comes first. The stale-poll cap does not interrupt an
   in-progress active-lag confirmation; it can still complete its required second
   full pass. If the deadline ends while the last known listing is still stale—
   including when a deadline-bounded control-plane read times out after it—the gate
   fails closed as `application_image_drift` (the deadline usually ends it first,
   since the polls only count once the rollout completes). A gateway failure named in
   the last observation keeps `rollout_timeout`, because the rollback triggers key on it.
   A drift failure (also raised if the listing stops matching on the final re-read)
   prints a fixed-vocabulary diagnostic: `listing_image=target|baseline|other` (where
   the listing points, `baseline` being the pre-deploy image), `app_version=behind|equal|ahead`
   when the listing reports a version, and `later_rollouts=<n>` with
   `latest_later=<status>:<target|baseline|other>` when a rollout created after the
   pinned one exists. `listing_image=baseline` with a later `completed:baseline`
   rollout means the control plane reverted the application after the pinned rollout
   completed. A baseline listing is accepted only if the gate never saw the target
   during this verification and the final rollout-list read found no newer rollout;
   `later_rollouts=0` alone does not establish that history. Digests, configurations
   and ids are never printed.
7. Require the intended Worker version at 100% traffic, `/readyz` **200** with all
   components ready and the exact compiled revision/build ID, and `/health` 200.
   Both responses carry `x-two-worker-version`, overwritten by the outer Worker
   from its `CF_VERSION_METADATA` staging version-metadata binding—not trusted
   from the container. Re-read rollout/Worker control-plane state after the
   runtime probes.

## Auto-rollback image completion

The staging auto-rollback's `worker_container_rollback.py` pins the newly created
rollback rollout ID, target version and snapshot image digest. A completed rollout
with finished steps, one updated instance, and converged or already-supported
active-lag counters completes the image leg even if the separate application
listing still shows the previous image. Replaced/reverted rollouts and identity
or image drift still fail closed. Production retains its application-listing
check.

Image completion is not rollback success: ownership takeover and the existing
18 consecutive passing `/health` and `/readyz` polls still follow. Those polls
require the restored Worker version, expected build revision (subject to the
existing pre-stamp exception), and `gateway=ready`; any miss resets the streak.

If the image leg times out, the log and failure evidence name the unmet condition
as `waiting_for=target_version|rollout_completion|counters_or_progress|application_image`.
The evidence file records `failure: image_rollout_timeout` and `image_restore_wait`,
never a successful stable gate or a raw application configuration. `application_image`
is only a production wait condition. Failure evidence is not a successful drill.

## Rollback drill: injected gateway failure (staging only)

The one supported way to prove the auto-rollback end to end is a manual
staging dispatch with the drill switch on:

```sh
gh workflow run deploy-staging.yml --ref main -f inject_gateway_failure=true
```

`inject_gateway_failure` is a `workflow_dispatch`-only boolean (default
false; a push cannot set it, and `deploy-production` never reads it). The
prepare step turns it into `--inject-gateway-failure` for
`scripts/staging_rollout.py`, which writes
`TWO_GATEWAY_READINESS_FAIL_INJECTION = "1"` into the **generated**
per-deploy staging config only — never a committed `wrangler.toml` var
(`wrangler/scripts/check-env-bindings.py` rejects it in every vars table).
The Worker forwards the switch into the container only while its
`TWO_DEPLOY_ENV` marker is exactly `staging` (a staging-vars-only binding),
and the Rust readiness fence repeats both checks before reporting the
gateway `down` on the real `/readyz` path. The gateway itself keeps
running; only readiness fails, in the exact component shape the gate
already rejects. With the switch off, absent, misspelled, or on any
non-staging deployment, nothing changes.

Expected drill shape: deploy and ownership takeover succeed, `verify`
fails because the new build serves `/readyz` 503 with `gateway: down`,
and the auto-rollback step restores the pre-deploy Worker version **and**
container image (with takeover). The run stays red by design — the
rollback restores service, it never certifies the injected build. The
next ordinary staging deploy generates its config without the switch, so
nothing needs to be switched off afterwards.

Evidence to capture from the drill run:

- the run URL and the exact head SHA it deployed;
- the pre-deploy snapshot printed by the snapshot step (Worker version +
  container image) and the uploaded `staging-rollout-evidence` artifact,
  whose generated `staging-deploy.json` shows the injected staging var;
- the `auto-rollback-evidence.json` from the same artifact naming the
  restored Worker version and image;
- a post-rollback `/readyz` read: 200, all components ready, previous
  build revision.

Stop conditions: do not start the drill while the schema or roles gates
are red (fix drift first, or the drill proves nothing about readiness);
do not re-dispatch to "retry" a drill whose rollback step failed or whose
post-rollback `/readyz` is not 200 (staging may be fenced — diagnose from
the artifact first, never release the fence to force a rerun); never
dispatch `deploy-production` as part of the drill.

## Rollback drill: rehearsal error-rate spike (staging only)

The error-rate gate proves its own fail-to-rollback chain the same way,
without live traffic and without a new secret:

```sh
gh workflow run deploy-staging.yml --ref main -f inject_error_rate_spike=true
```

`inject_error_rate_spike` is a `workflow_dispatch`-only boolean (default
false; a push cannot set it, and `deploy-production` never reads it). It
reaches only the `error_rate_gate` step's `INJECT_ERROR_RATE_SPIKE` env
through the pinned dispatch-only expression: the step passes
`--rehearsal-firing '["rest_5xx_rate"]'` to
`wrangler/scripts/promotion-error-gate.mjs`, which judges it through the
real `evaluatePromotionGate` helper and exits non-zero, so the existing
auto-rollback step restores the pre-deploy Worker version **and**
container image. The run stays red by design, exactly like the gateway
drill. Capture `error-rate-gate.json` (mode `rehearsal`, verdict `fail`)
from the `staging-rollout-evidence` artifact alongside the rollback
evidence above. This rehearsal proves the gate-to-rollback wiring; it is
not proof of live 5xx enforcement, which additionally needs the
`STAGING_METRICS_SCRAPE_TOKEN` secret (see
[runbook-observability](runbook-observability.md)).

## Bounds and fail-closed cases

The verification phase has a five-minute deadline, ten-second network socket
bounds, bounded response sizes and five-second convergence intervals. Docker
inspection has twenty-second subprocess bounds. Readiness 503 is a truthful
parked process, **never** successful deployment acceptance. SDK/API failures and
HTTP error bodies are not printed. Only fixed gate error classes are reported.
Credentials are read from the existing deployment environment; auth failures stop
without retrying with another credential or requesting extra grants.

The helper intentionally targets the pinned rolling API profile. A future
`new_instances` profile needs an explicit, fixture-tested implementation; absent
counters are not defaulted to zero. Rollout listing is bounded to 100 rows. The
pinned client documents a `last` input but does not establish a trustworthy
next-cursor/ordering contract, so a saturated page fails with
`rollout_snapshot_truncated` rather than pretending the baseline is complete.
First-deploy/bootstrap and no-new-rollout cases also fail; they are not evidence
that a new revision was rolled out. No automatic rollback or secret mutation is
performed by `scripts/staging_rollout.py` itself; rollback is a workflow step that
invokes `scripts/worker_container_rollback.py`. The local baseline receipt is
type- and shape-validated before `select_rollout`, `receipt` or `verify` acts
on it; any malformed member refuses with the single fixed token
`invalid_baseline_receipt`. Required members are `started`, `application_id`,
`namespace_id`, `revision`, `build_id`, `deploy_id` and `rollout_ids`. `deploy_id`
must be canonical `YYYY.MM.DD.N+sha7` (a real date, positive sequence without
leading zeros, lowercase seven-hex revision). `started` must be a finite positive timestamp
(the producer writes `time.time()`), and a decodable-but-non-object receipt
file (a JSON list, null, or string) refuses with the same token.

On success the job prints an allowlisted receipt: application/rollout/Worker
version, target application version, immutable image digest, build revision/ID and
HTTP statuses. Raw Cloudflare configurations can include env values and are never
saved as artifacts. Temporary baseline/config/NDJSON files stay in runner scratch.

## Verification boundaries

Offline standard-library fixtures exercise selection, schema/error handling,
build identity and runtime acceptance; Worker tests exercise the installed
Container SDK and spoof-resistant version header. Rust readiness tests check
compiled provenance. These fixtures do not prove a live Cloudflare rollout,
staging recovery, Discord reconnect or migration completion. A merged deployment
must pass the actual staging job; production cutover remains separately gated.

Pinned source contracts:

- [ApplicationRollout](https://github.com/cloudflare/workers-sdk/blob/dfd962fc07f61a3fbdd748d6bf073a49651efa75/packages/containers-shared/src/client/models/ApplicationRollout.ts)
- [RolloutsService](https://github.com/cloudflare/workers-sdk/blob/dfd962fc07f61a3fbdd748d6bf073a49651efa75/packages/containers-shared/src/client/services/RolloutsService.ts)
- [Wrangler image build/push](https://github.com/cloudflare/workers-sdk/blob/dfd962fc07f61a3fbdd748d6bf073a49651efa75/packages/containers-shared/src/build.ts)
- [Structured output](https://github.com/cloudflare/workers-sdk/blob/dfd962fc07f61a3fbdd748d6bf073a49651efa75/packages/workers-utils/src/output.ts)
- [Worker version metadata](https://developers.cloudflare.com/workers/runtime-apis/bindings/version-metadata/)
