# Intended staging rollout acceptance

`deploy-staging.yml` runs `scripts/staging_rollout.py` before and after a
successful Wrangler deployment (the `deploy` job was re-enabled by
[TOG-12856](/TOG/issues/TOG-12856)). Order: ownership-control `preflight`,
`prepare`, Wrangler deploy, `receipt`, ownership-control `deployment-takeover`,
`verify`. `preflight` and `deployment-takeover` live outside
`staging_rollout.py`. Readiness cannot be proven before the takeover, because a
fenced or non-owner singleton refuses `/readyz`; the Wrangler receipt is local
and is therefore accepted before ownership moves. A red `verify` leaves the owner
`active` on a Worker whose receipt was already accepted but whose rollout was not
confirmed; the next push may hand off again. The gate does not
accept a health response from an old singleton. This is a **staging-only** gate,
not production authorization or a migration tool.

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
5. If `verify` goes red, staging stays owned by the already-serving version and the
   job is red on `main`. There is deliberately **no automatic fence**: a fence would
   park every later push deploy until a manual `release_fence` dispatch. Re-run the
   deploy (or dispatch it) after fixing the cause; production deploys are separate,
   manual and cutover-owned.

## Provenance chain

1. Before deployment, snapshot the existing application named
   `two-bot-next-twobotcontainer-staging`, its Durable Object namespace, and rollout
   IDs. An absent/ambiguous application or an in-flight prior rollout fails closed.
2. Generate a temporary JSON Wrangler config from the checked-in TOML. Keep the
   staging Container/DO wiring, use absolute source/build paths, and pass two
   non-secret Docker build arguments as `image_vars`: `BOT_BUILD_REVISION` (the
   exact `GITHUB_SHA`) and `BOT_BUILD_ID` (the unique
   `GITHUB_RUN_ID-GITHUB_RUN_ATTEMPT`). The Rust `/readyz` response compiles these
   values into the binary as `build_revision`/`build_id`; runtime environment
   overrides cannot substitute them. The `org.opencontainers.image.revision` and
   `com.togetherweown.build-id` OCI labels repeat them for narrowly scoped image
   inspection. A redeploy of the same SHA therefore still produces a new image
   identity. Prepare also requires the `CF_VERSION_METADATA` version-metadata
   binding in the staging config and sets `rollout_kind: full_auto`.
3. Read the **fresh** `WRANGLER_OUTPUT_FILE_PATH` NDJSON only after Wrangler exits
   successfully. Every Wrangler invocation appends its own `wrangler-session`
   record, and `wrangler-action` runs `wrangler --version` before deploying, so
   the file holds several sessions. Require at least one session, all pinned to
   4.143.1 with record version 1, exactly one `deploy` invocation and every other
   session exactly `--version` or `-v`, and exactly one staging deploy record
   with the expected Worker name, fresh timestamp and concrete Worker version.
   The `receipt` step runs this check **before** `deployment-takeover`, so
   ownership never moves to a deploy whose receipt is invalid; `verify` repeats
   it. Raw session arguments/log paths are never published. A failure prints,
   after the fixed gate code, only an allowlisted diagnostic: record-type counts,
   session class counts (deploy / probe / other), each session's record/Wrangler
   version, and which deploy-record checks failed. A verify timeout separately
   prints the last rollout/readiness observation (fixed vocabulary only).
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
   application's actual configuration digest separately.
7. Require the intended Worker version at 100% traffic, `/readyz` **200** with all
   components ready and the exact compiled revision/build ID, and `/health` 200.
   Both responses carry `x-two-worker-version`, overwritten by the outer Worker
   from its `CF_VERSION_METADATA` staging version-metadata binding—not trusted
   from the container. Re-read rollout/Worker control-plane state after the
   runtime probes.

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
performed by this script.

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

- [ApplicationRollout](https://github.com/cloudflare/workers-sdk/blob/wrangler%404.143.1/packages/containers-shared/src/client/models/ApplicationRollout.ts)
- [RolloutsService](https://github.com/cloudflare/workers-sdk/blob/wrangler%404.143.1/packages/containers-shared/src/client/services/RolloutsService.ts)
- [Wrangler image build/push](https://github.com/cloudflare/workers-sdk/blob/wrangler%404.143.1/packages/containers-shared/src/build.ts)
- [Structured output](https://github.com/cloudflare/workers-sdk/blob/wrangler%404.143.1/packages/workers-utils/src/output.ts)
- [Worker version metadata](https://developers.cloudflare.com/workers/runtime-apis/bindings/version-metadata/)
