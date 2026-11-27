import assert from "node:assert/strict";
import test from "node:test";
import { bindStagingAutomodReadbackProvider } from "../scripts/staging-automod-readback-provider.ts";
import { readStagingAutomodJobInputs } from "../src/staging-automod-inputs.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";

const deploymentId = "12345678-abcd-abcd-abcd-123456789012";
const sourceSha = "a".repeat(40);
const pins = { deploymentId, sourceSha, ownerEpoch: 7, runId: "42", runAttempt: 1 } as const;
const stagingControlBearer = "fixture-staging-control-".repeat(2);
const githubReadToken = "fixture-actions-read";
const oidc = "fixture.oidc.token";
const credentials = { stagingControlBearer, githubReadToken };
const scope = { environment: "staging", deploymentId, sha: sourceSha, runId: "42", runAttempt: 1, target } as const;
const acquiredAt = Date.now() - 1_000;
const issuedLease = { environment: scope.environment, deploymentId, sha: sourceSha, runId: pins.runId, runAttempt: 1,
  purpose: AUTOMOD_STAGING_PURPOSE, epoch: 7,
  phase: "active", acquiredAt, expiresAt: acquiredAt + 60_000 } as const;
const repository = { id: 1396224242 };
const run = { id: 42, head_sha: sourceSha, run_attempt: 1, status: "in_progress", event: "workflow_dispatch",
  head_branch: "main", path: ".github/workflows/staging-automod-preview.yml@main", repository, head_repository: repository };
const status = { deploymentId, sha: sourceSha, running: false, voiceMoveEnabled: false,
  owner: { deploymentId, phase: "active", epoch: 7 }, use: issuedLease };
const runUrl = "https://api.github.com/repos/TogetherWeOwn/two-bot-next/actions/runs/42";
const bridgeUrl = "https://two-bot-next-staging.5150.workers.dev/internal/staging-automod";
const statusUrl = "https://two-bot-next-staging.5150.workers.dev/internal/staging-use";

type Transport = Parameters<typeof bindStagingAutomodReadbackProvider>[2];
type Call = { url: string; init: RequestInit };
type Respond = (call: Call) => Response | Promise<Response> | undefined;

function fixture(respond?: Respond, mint?: Transport["mintOidcToken"]) {
  const calls: Call[] = [];
  const mints: { audience: string; signal: AbortSignal }[] = [];
  const transport: Transport = {
    fetch: async (url, init) => {
      calls.push({ url, init });
      const reply = respond?.({ url, init });
      if (reply) return reply;
      if (url === runUrl) return Response.json(run);
      if (url === statusUrl) return Response.json(status);
      assert.equal(url, bridgeUrl);
      const operation = JSON.parse(String(init.body));
      return Response.json({ value: operation.op === "read-lease"
        ? issuedLease : { action: operation.action, issuedLease, target } });
    },
    mintOidcToken: (audience, signal) => {
      mints.push({ audience, signal });
      return mint ? mint(audience, signal) : Promise.resolve(oidc);
    },
  };
  const provider = bindStagingAutomodReadbackProvider(pins, credentials, transport);
  assert.ok(provider);
  return { ...provider, calls, mints, transport };
}

const signal = () => new AbortController().signal;

test("reads the exact run, issued lease and independent approvals with separated credentials", async () => {
  const provider = fixture();
  assert.deepEqual(await provider.readRun("42", signal()), run);
  const inputs = await readStagingAutomodJobInputs(scope, {
    readIssuedLease: context => provider.authority.readIssuedLease(context, signal()),
    readApproval: (action, lease) => provider.authority.readApproval(action, lease, signal()),
  });
  assert.deepEqual(inputs?.issuedLease, issuedLease);
  assert.deepEqual(inputs?.approvals, {
    start: { action: "start", issuedLease, target }, recover: { action: "recover", issuedLease, target },
  });
  assert.deepEqual(provider.calls.map(({ url }) => url).sort(),
    [...Array(4).fill(runUrl), ...Array(3).fill(bridgeUrl), ...Array(3).fill(statusUrl)].sort());
  for (const { url, init } of provider.calls) {
    const headers = new Headers(init.headers);
    assert.equal(init.redirect, "error");
    assert.equal(init.method, url === bridgeUrl ? "POST" : "GET");
    assert.equal(headers.get("authorization"), `Bearer ${url === runUrl ? githubReadToken : stagingControlBearer}`);
    assert.equal(headers.get("x-two-actions-oidc"), url === bridgeUrl ? oidc : null);
    assert.ok(!JSON.stringify(init.body ?? "").includes(stagingControlBearer));
    assert.ok(!JSON.stringify(init.body ?? "").includes(githubReadToken));
  }
  for (const { audience, signal: shared } of provider.mints) {
    assert.equal(audience, "two-bot-staging-automod");
    assert.deepEqual(provider.calls.filter(({ init }) => init.signal === shared).map(({ url }) => url),
      [runUrl, bridgeUrl, statusUrl]);
  }
  assert.deepEqual(JSON.parse(String(provider.calls.find(({ url }) => url === bridgeUrl)?.init.body)),
    { op: "read-lease", context: scope });
});

test("refuses invalid pins, wrong input scope and credential reuse before dispatch", async () => {
  const provider = fixture();
  const { authority, transport } = provider;
  assert.equal(bindStagingAutomodReadbackProvider({ ...pins, runId: "42/../../" }, credentials, transport), undefined);
  assert.equal(bindStagingAutomodReadbackProvider(pins, { ...credentials, githubReadToken: stagingControlBearer }, transport),
    undefined);
  assert.equal(await provider.readRun("42?redirect=other", signal()), undefined);
  const wrongTarget = { ...target, name: "other" };
  assert.equal(await authority.readIssuedLease({ ...scope, target: wrongTarget }, signal()), undefined);
  assert.equal(await authority.readIssuedLease({ ...scope, runAttempt: 2 }, signal()), undefined);
  assert.equal(await authority.readApproval("start", { ...issuedLease, sha: "b".repeat(40) }, signal()), undefined);
  const aborted = AbortSignal.abort(new Error(stagingControlBearer));
  assert.equal(await authority.readIssuedLease(scope, aborted), undefined);
  assert.deepEqual(provider.calls, []);
  assert.deepEqual(provider.mints, []);

  for (const wrongToken of [githubReadToken, stagingControlBearer, "bad\r\nvalue"]) {
    const invalid = fixture(undefined, async () => wrongToken);
    assert.equal(await invalid.authority.readIssuedLease(scope, signal()), undefined);
    assert.deepEqual(invalid.calls.map(({ url }) => url), [runUrl]);
  }
});

test("refuses mismatched GitHub run identity and staging lease or active ownership", async () => {
  for (const change of [{ id: 43 }, { run_attempt: 2 }, { head_sha: "b".repeat(40) }, { repository: { id: 1 } },
    { head_repository: { id: 1 } }, { path: "other.yml" }, { status: "completed" }]) {
    const provider = fixture(({ url }) => url === runUrl ? Response.json({ ...run, ...change }) : undefined);
    assert.equal(await provider.authority.readIssuedLease(scope, signal()), undefined, JSON.stringify(change));
    assert.deepEqual(provider.calls.map(({ url }) => url), [runUrl]);
  }
  for (const change of [{ deploymentId: "wrong" }, { sha: "b".repeat(40) }, { epoch: 8 },
    { runId: "43" }, { runAttempt: 2 }, { phase: "released" }, { purpose: "ordinary" }]) {
    const provider = fixture(({ url }) => url === bridgeUrl
      ? Response.json({ value: { ...issuedLease, ...change } }) : undefined);
    assert.equal(await provider.authority.readIssuedLease(scope, signal()), undefined, JSON.stringify(change));
    assert.deepEqual(provider.calls.map(({ url }) => url), [runUrl, bridgeUrl]);
  }
  for (const change of [{ deploymentId: "wrong" }, { sha: "b".repeat(40) }, { owner: null },
    { owner: { ...status.owner, deploymentId: "wrong" } }, { owner: { ...status.owner, phase: "fenced" } },
    { owner: { ...status.owner, epoch: 8 } }, { use: { ...issuedLease, acquiredAt: acquiredAt + 1 } }]) {
    const provider = fixture(({ url }) => url === statusUrl ? Response.json({ ...status, ...change }) : undefined);
    assert.equal(await provider.authority.readIssuedLease(scope, signal()), undefined, JSON.stringify(change));
  }
});

test("refuses a mismatched approval without granting or withholding the other action", async () => {
  for (const change of [{ action: "recover" }, { target: { ...target, guildId: "other" } },
    { issuedLease: { ...issuedLease, runAttempt: 2 } }, { issuedLease: { ...issuedLease, expiresAt: acquiredAt + 1 } }]) {
    const provider = fixture(({ url, init }) => url === bridgeUrl && JSON.parse(String(init.body)).action === "start"
      ? Response.json({ value: { action: "start", issuedLease, target, ...change } }) : undefined);
    assert.equal(await provider.authority.readApproval("start", issuedLease, signal()), undefined, JSON.stringify(change));
    assert.deepEqual(await provider.authority.readApproval("recover", issuedLease, signal()),
      { action: "recover", issuedLease, target });
  }
});

test("rejects redirects and non-200 replies on every endpoint without reading or forwarding them", async () => {
  for (const endpoint of [runUrl, bridgeUrl, statusUrl]) {
    for (const code of [201, 302, 403]) {
      let cancelled = false;
      const provider = fixture(({ url }) => url === endpoint ? new Response(new ReadableStream({
        cancel() { cancelled = true; },
      }), { status: code, headers: { location: "https://other.invalid/" } }) : undefined);
      assert.equal(await provider.authority.readIssuedLease(scope, signal()), undefined, `${endpoint} ${code}`);
      assert.ok(cancelled);
      assert.equal(provider.calls.at(-1)?.url, endpoint);
    }
  }
  for (const override of [{ redirected: true }, { url: "https://other.invalid/" }]) {
    const response = Response.json(run);
    for (const [key, value] of Object.entries(override)) Object.defineProperty(response, key, { value });
    const redirected = fixture(({ url }) => url === runUrl ? response : undefined);
    assert.equal(await redirected.authority.readIssuedLease(scope, signal()), undefined);
    assert.deepEqual(redirected.calls.map(({ url }) => url), [runUrl]);
  }
});

test("bounds bytes and refuses duplicate keys, malformed UTF-8 and reflected transport errors", async () => {
  const cases: [string, () => Response][] = [
    [runUrl, () => new Response(" ".repeat(1_048_576) + JSON.stringify(run))],
    [runUrl, () => new Response(JSON.stringify(run).replace('"id":42', '"id":42,"\\u0069d":42'))],
    [bridgeUrl, () => new Response(`{"value":null,"value":${JSON.stringify(issuedLease)}}`)],
    [statusUrl, () => new Response(JSON.stringify(status).replace('"epoch":7', '"epoch":7,"epoch":7'))],
    [statusUrl, () => new Response(new Uint8Array([0xc0, 0xaf]))],
    [bridgeUrl, () => new Response(" ".repeat(4_096) + JSON.stringify({ value: issuedLease }))],
    [statusUrl, () => Response.json({ ...status, owner: { ...status.owner, actor: "é".repeat(2_048) } })],
    [runUrl, () => { throw new Error(`reflected ${githubReadToken} ${stagingControlBearer}`); }],
  ];
  for (const [endpoint, response] of cases) {
    const provider = fixture(({ url }) => url === endpoint ? response() : undefined);
    assert.equal(await provider.authority.readIssuedLease(scope, signal()), undefined);
    assert.equal(provider.calls.at(-1)?.url, endpoint);
  }
});

test("parent cancellation releases an ignored body read and stops the transaction", async () => {
  const parent = new AbortController();
  let cancelled = false;
  const provider = fixture(({ url }) => url === bridgeUrl ? new Response(new ReadableStream({
    pull() { parent.abort(new Error(stagingControlBearer)); return new Promise<void>(() => {}); },
    cancel() { cancelled = true; return new Promise<void>(() => {}); },
  })) : undefined);
  assert.equal(await provider.authority.readIssuedLease(scope, parent.signal), undefined);
  assert.ok(cancelled);
  assert.deepEqual(provider.calls.map(({ url }) => url), [runUrl, bridgeUrl]);
});

test("one deadline covers mint, fetch and body reads even when the transport ignores abort", async t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let release: (response: Response) => void = () => {};
  const provider = fixture(({ url }) => url === bridgeUrl
    ? new Promise<Response>(resolve => { release = resolve; }) : undefined, async () => {
    t.mock.timers.tick(1_999);
    return oidc;
  });
  const pending = provider.authority.readIssuedLease(scope, signal());
  await new Promise<void>(resolve => setImmediate(resolve));
  assert.equal(provider.calls.at(-1)?.url, bridgeUrl);
  t.mock.timers.tick(1);
  assert.equal(await pending, undefined);
  assert.ok(provider.calls.at(-1)?.init.signal?.aborted);

  let cancelled = false;
  release(new Response(new ReadableStream({ cancel() { cancelled = true; } })));
  await new Promise<void>(resolve => setImmediate(resolve));
  assert.ok(cancelled);
});
