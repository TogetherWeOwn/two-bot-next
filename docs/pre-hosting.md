# Hosting decision for the archived attendance predecessor

Keep live PRE qualification closed for [#2614](https://github.com/TogetherWeOwn/two-bot-next/issues/2614).
Keep hosted caller wiring and the existing selectors off. Use the isolated
Docker fixture for synthetic Container evidence. Cloudflare
Workers and Containers is the candidate for the exact hosted pair when an
already-approved isolated account can prove private creation and enforce the
Container launch policy independently of the archived Worker. The available
evidence leaves both controls open. Separate source PRs track the
[archived upload guard (#2622)](https://github.com/TogetherWeOwn/two-bot-next/pull/2622),
[hosted transport preflight (#2654)](https://github.com/TogetherWeOwn/two-bot-next/pull/2654)
and [dormant caller (#2650)](https://github.com/TogetherWeOwn/two-bot-next/pull/2650).

## Archived pair

The protected rehearsal packet pins first-attempt build `45640646722/1`, artifact `15036560345`,
and source revision `b689bc3545777c801b8758bcb7160e72a0321724`. The archived
`index.js` SHA-256 is `709eaf3242b28d645cf163a3e28333b106f9dd35ab2426d0eef023a431326453`.
The Linux amd64 OCI manifest is `sha256:c2a855f56ac103323ec1055618e68e5716ad0432f9e0faf158594b8166766c1c`
and its config is `sha256:b4bba336bfeb30da8a03732687385c2f5a9baa6533f25c87de3e02d8140975f8`.
Verify the full archive with the protected materializer and preserve every
Worker module and OCI blob. Verify the reserved registry reference and pull by
digest for any paired rehearsal. [Producer artifact](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/45640646722/artifacts/15036560345)

The archived Worker forwards the database URL, token and guild ID. Its Container
environment omits `DISCORD_GATEWAY_URL` and `DISCORD_API_BASE`, so setting those
names on a Worker or caller leaves the Rust process without the overrides. The
gateway override requires a loopback WebSocket address. Both fixture listeners
must be reachable by loopback from the bot process. The archived Container SDK
defaults to `enableInternet: true`. Probe-triggered auto-start uses
`startAndWaitForPorts`, and the Worker's own keepalive can reach the launch path
again after the initiating runner exits. [Archived Worker source](https://github.com/TogetherWeOwn/two-bot-next/blob/b689bc3545777c801b8758bcb7160e72a0321724/wrangler/src/index.ts) ·
[Archived gateway source](https://github.com/TogetherWeOwn/two-bot-next/blob/b689bc3545777c801b8758bcb7160e72a0321724/crates/bot/src/main.rs) ·
[Container SDK 0.3.7](https://unpkg.com/@cloudflare/containers@0.3.7/dist/lib/container.js)

## Hosting options

| Boundary | What the available evidence supports | Gap for the exact rehearsal |
| --- | --- | --- |
| Existing Cloudflare Workers and Containers | Same-account HTTP Service Bindings call the named Worker and environment without a public URL. Prior read-only staging inventory joined the Worker's `TWO_BOT` binding, the exact worker/class namespace, and the Container app detail by Durable Object namespace ID. The upload and route fixtures check artifact identity and complete ingress readback. | The script upload and per-script subdomain APIs are separate. The provider documents `workers.dev` route assignment on creation. A later read with `workers.dev` and Version URLs disabled leaves first-create privacy unproved. A service binding names a service, so the caller must independently verify the serving version and app. Staging inventory covers identity correlation only. There is no evidenced isolated account default or provider-supported atomic create with every public ingress surface disabled. [Worker upload](https://developers.cloudflare.com/api/resources/workers/subresources/scripts/methods/update/) · [Subdomain API](https://developers.cloudflare.com/api/resources/workers/subresources/scripts/subresources/subdomain/methods/create/) · [workers.dev](https://developers.cloudflare.com/workers/configuration/routing/workers-dev/) · [Version URLs](https://developers.cloudflare.com/workers/configuration/previews/) · [Service Bindings](https://developers.cloudflare.com/workers/runtime-apis/bindings/service-bindings/) · [Container detail](https://developers.cloudflare.com/api/resources/containers/subresources/applications/methods/get/) |
| Cloudflare Container launch controls | The scheduler-backed application API exposes `configuration.environment_variables` and command/entrypoint settings. The separate Durable Object-managed application form delegates instance lifecycle to the Durable Object and exposes neither an application environment override nor a command override. The start API takes process `env`, `entrypoint` and `enableInternet`. | The scheduler-backed schema supplies no guarantee that application defaults win over the archived Worker's actual start configuration or that mocks can be installed in the process network namespace before each start. The documented egress controls require control of start options or Worker-side intercepts. HTTP(S) intercepts alone supply neither a loopback listener nor a private PostgreSQL transport. No documented control in this path enforces the required settings and default-deny policy across legacy restarts without changing the pair. [Application Create API](https://developers.cloudflare.com/api/resources/containers/subresources/applications/methods/create/) · [Container start](https://developers.cloudflare.com/containers/api/durable-object-container/#start) · [Outbound traffic](https://developers.cloudflare.com/containers/configuration/outbound-traffic/) |
| Disposable Docker fixture | A `--network=none` namespace is loopback-only. The disposable PostgreSQL, mock and bot share that namespace, and the Docker launch supplies both settings to the exact bot image. The retained replay used the exact image fixture changes from #2635 and the REST sync check from #2654. Both PRE shapes showed readiness 503 then 200, a gateway checkpoint, nonempty REST command sync and cleanup on synthetic data with existing capacity. | Scope is `direct-container-only`. There is no Worker service binding, Durable Object launch, hosted app UUID or proof against a legacy keepalive restart. [Docker none driver](https://docs.docker.com/engine/network/drivers/none/) · [Docker run](https://docs.docker.com/engine/containers/run/) · [Exact image fixture (#2635)](https://github.com/TogetherWeOwn/two-bot-next/pull/2635) · [REST sync verification (#2654)](https://github.com/TogetherWeOwn/two-bot-next/pull/2654) |
| GitHub-hosted fixture checks and Cloudflare local development | The existing Check workflow discovers the synthetic reaper fixture. The reusable reaper workflow has no wired caller. Cloudflare documents local Worker-driven Container launches using a Docker-compatible engine. This is a candidate for additional local paired evidence if its launch boundary can be enforced on already-approved isolated capacity. | Local dev alone leaves the archived launch options in control. The reviewed configuration surfaces provide no supported pre-start environment and shared-loopback listener injection with egress denial for this exact archived pair. Local evidence also cannot establish Cloudflare account ingress, hosted version/app identity or stop after runner loss. [Check workflow](../.github/workflows/check.yml) · [Synthetic reaper](../.github/workflows/staging-pre-reaper.yml) · [Cloudflare local development](https://developers.cloudflare.com/containers/local-dev/) |

Use the direct-container fixture route for zero-new-spend synthetic evidence.
The retained receipt applies to the replay inputs identified above. Cloudflare's
included usage is metered. The packet contains
no applicable allowance readback for an isolated hosted run. Keep hosted work
at the decision and evidence stage. [Cloudflare Container pricing](https://developers.cloudflare.com/containers/platform/pricing/)

## Acceptance evidence

The missing provider predicate is a supported, reservation-scoped pre-start
boundary outside the archived Worker. It must apply both effective process
settings, provide both same-network-namespace loopback listeners and enforce
default-deny external egress before the first process and every replacement
process can run. The same path must keep ingress private from the first provider
operation that could expose the Worker. Require the following evidence for one
exact reserved pair at a time.

1. **Private first creation and exact dispatch.** Retain the provider contract
   and authenticated operation-order evidence that public ingress was disabled
   before the first upload, version or deploy operation that could make it
   reachable. Read back the complete `workers.dev`, Version URL, route and
   custom-domain state. Join the protected caller's exact service target and
   environment to the serving Worker version UUID and annotation, the `TWO_BOT`
   binding and Worker/class namespace ID, and the independently pinned app UUID.
   Keep the service target reserved and revalidate the version on each bounded
   probe. A later unrouted snapshot or a matching service name in isolation
   supplies only part of the proof.
2. **Effective launch controls.** Record the effective process settings
   `DISCORD_GATEWAY_URL=ws://127.0.0.1:8081` and
   `DISCORD_API_BASE=http://127.0.0.1:8081` without dumping the environment.
   Correlate process and listener namespace identity, readiness before launch,
   policy generation and the running OCI digest for the cold start and for a
   forced restart through the archived keepalive path. Show an enforced
   pre-start deny policy for external Discord traffic, covering DNS, direct
   IPv4/IPv6 and HTTPS/WSS. Use controlled synthetic denial probes or policy
   observations, with zero traffic to public Discord. Establish that a missing
   policy or listener blocks process start on every launch path. Allow only the
   separately authenticated disposable PostgreSQL path the rehearsal needs.
3. **Paired readiness.** For separate reservations for `0473/0465` and retained
   `0475/0465`, invoke the unchanged Worker through the exact private binding.
   Record its response version stamp, runtime build/revision/deploy stamps,
   running digest, effective isolated database writer and PRE ledger including
   the retained timestamp. Require `/health` 200. Before releasing mock READY,
   require the pinned bot's `/readyz` 503 with matching runtime stamps, process
   and database ready, and gateway starting or down. After release, require
   the same pinned runtime's `/readyz` 200 with database and gateway ready.
   Correlate it with the synthetic gateway checkpoint and nonempty REST command
   sync.
4. **Independent stop and cleanup.** Arm the original cutoff before create.
   Show that runner loss, delayed create and legacy keepalive cannot defeat
   the independent fence. Retain exact-scope settlement, writer revocation,
   pre-delete zero active and assigned counts or affirmative never-created
   evidence, fresh absence and settled registry disposition before handback.
   [Existing rollout and cleanup contract](staging-rollout-gate.md#disposable-exact-pre-rehearsal)

Adopt a provider path only when its supported controls and authenticated
evidence cover the full sequence. Any future caller integration requires the
reviewed upload guard and hosted preflight on protected source. Until the
provider boundary is proved, keep live admission closed. The two synthetic PRE
receipts support the direct-container fallback and leave hosted qualification open.

## Replacement artifact with native scheduling

A replacement PRE artifact can use Cloudflare's `durable_object` scheduling
policy with a dedicated `PreQualificationContainer` class and namespace. The
[launch check](../scripts/pre_runtime_binding.py) reads the exact version and
source annotation, then matches its `TWO_BOT` namespace against the reserved
application's namespace and scheduling policy. The existing private-route
and writer observation and independent fence recheck follow these reads.
[Worker version API](https://developers.cloudflare.com/api/resources/workers/subresources/scripts/subresources/versions/methods/get/) · [Get Application API](https://developers.cloudflare.com/api/resources/containers/subresources/applications/methods/get/)

Resolve the verified immutable image from the exact Durable Object's `images`
map and pass the reference to `start`. Use `inspect` and readiness to verify
the running image after each admitted start. Wire the dedicated class to
reject starts outside the armed window and disable Internet access for every
launch. Put synthetic Discord and PostgreSQL fixtures in the same container
so they can bind loopback before the bot starts. Cloudflare's HTTP intercepts
cannot carry raw PostgreSQL with Internet access disabled.
[Native Container API](https://developers.cloudflare.com/containers/api/durable-object-container/) · [Non-HTTP egress](https://developers.cloudflare.com/containers/configuration/outbound-traffic/#non-http-traffic)

For native scheduling, Get Application reports observed `active` and
`starting` counts. The [cleanup projection](../scripts/pre_inventory.py)
requires `active` and `assigned`. A zero `starting` count cannot satisfy the
assigned count check. Keep admission closed until a provider-supported cleanup
adapter can prove zero active and assigned instances for the exact app before
deletion, including settlement of any in-flight starts. Preserve the existing
cleanup refusal while this proof is unavailable. The instance API reports a
latest known state and newly started instances can take time to appear. A
cleanup adapter could use positive terminal observations for the exact actors
after confirmed stop and settlement. An empty listing alone gives no proof.
[Get Application API](https://developers.cloudflare.com/api/resources/containers/subresources/applications/methods/get/) · [Instance API](https://developers.cloudflare.com/api/resources/containers/subresources/applications/subresources/instances/methods/list/) · [Listing delay](https://developers.cloudflare.com/sandbox/manage/list-sandboxes/#list-sandboxes-across-the-account)

Qualification requires protected evidence of private first create, the exact
fixture writer, version-bound takeover and readiness at both PRE endpoints,
and independent stop and cleanup. Use the new class with a separately reviewed
build. The archived upload continues to refuse before provisioning.
[Scheduling migration guide](https://developers.cloudflare.com/containers/guides/migrate-to-durable-object-scheduling-policy/)
