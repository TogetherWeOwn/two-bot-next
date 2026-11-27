import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import {
  bindStagingAutomodInstanceClient, STAGING_AUTOMOD_BRIDGE_URL,
} from "../scripts/staging-automod-instance-client.ts";
import {
  STAGING_AUTOMOD_AUDIENCE, STAGING_AUTOMOD_BODY_BYTES, STAGING_AUTOMOD_OIDC_HEADER,
} from "../src/staging-automod-bridge.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

const env = {
  ACTIONS_ID_TOKEN_REQUEST_URL: "https://pipelines.actions.githubusercontent.com/token?audience=old&fixture=1",
  ACTIONS_ID_TOKEN_REQUEST_TOKEN: "synthetic-oidc-request",
  GITHUB_TOKEN: "synthetic-github-read",
  OWNERSHIP_CONTROL_TOKEN: "synthetic-control-" + "c".repeat(32),
};
const serving = { deploymentId: lease.deploymentId, sha: lease.sha };
const minted = "synthetic-minted-oidc";
const runUrl = `https://api.github.com/repos/TogetherWeOwn/two-bot-next/actions/runs/${lease.runId}`;
const signal = new AbortController().signal;
const maxRunBytes = 1_048_576;

function unexpectedFetch(): never {
  return assert.fail("unexpected fetch");
}

beforeEach(t => {
  t.mock.method(globalThis, "fetch", unexpectedFetch);
});

test("the instance client isolates credentials across the three request boundaries", async t => {
  const fetcher = t.mock.fn(async (url: string | URL, _init: RequestInit) => {
    if (String(url) === runUrl) return Response.json({ id: Number(lease.runId) });
    if (url instanceof URL && url.origin === "https://pipelines.actions.githubusercontent.com") {
      assert.equal(url.pathname, "/token");
      assert.deepEqual(url.searchParams.getAll("audience"), [STAGING_AUTOMOD_AUDIENCE]);
      assert.equal(url.searchParams.get("fixture"), "1");
      return Response.json({ value: minted });
    }
    if (url === STAGING_AUTOMOD_BRIDGE_URL) return Response.json({ value: lease });
    return unexpectedFetch();
  });
  const client = bindStagingAutomodInstanceClient(env, serving, fetcher);
  assert.ok(client);
  assert.deepEqual(await client.host.readRun(lease.runId, signal), { id: Number(lease.runId) });
  assert.deepEqual(await client.host.authority.readIssuedLease(lease, signal), lease);

  assert.deepEqual(fetcher.mock.calls.map(({ arguments: [url, init] }) => ({
    url: String(url), method: init.method, redirect: init.redirect, headers: Object.fromEntries(new Headers(init.headers)),
  })), [
    { url: runUrl, method: "GET", redirect: "error", headers: {
      accept: "application/vnd.github+json", authorization: `Bearer ${env.GITHUB_TOKEN}`,
      "x-github-api-version": "2022-11-28",
    } },
    { url: `https://pipelines.actions.githubusercontent.com/token?audience=${encodeURIComponent(STAGING_AUTOMOD_AUDIENCE)}&fixture=1`,
      method: "GET", redirect: "error", headers: {
        accept: "application/json", authorization: `Bearer ${env.ACTIONS_ID_TOKEN_REQUEST_TOKEN}`,
      } },
    { url: STAGING_AUTOMOD_BRIDGE_URL, method: "POST", redirect: "error", headers: {
      authorization: `Bearer ${env.OWNERSHIP_CONTROL_TOKEN}`, "content-type": "application/json",
      [STAGING_AUTOMOD_OIDC_HEADER]: minted,
    } },
  ]);
});

test("Actions run reads accept an omitted token and both valid token length limits", async () => {
  const { GITHUB_TOKEN: _, ...withoutGitHubToken } = env;
  for (const credentials of [withoutGitHubToken, { ...env, GITHUB_TOKEN: "!" }, { ...env, GITHUB_TOKEN: "~".repeat(4096) }]) {
    const token = "GITHUB_TOKEN" in credentials ? credentials.GITHUB_TOKEN : undefined;
    const client = bindStagingAutomodInstanceClient(credentials, serving, async (url, init) => {
      assert.equal(url, runUrl);
      assert.ok(new Headers(init.headers).get("authorization") === (token === undefined ? null : `Bearer ${token}`));
      return Response.json({ id: Number(lease.runId) });
    });
    assert.ok(client);
    assert.deepEqual(await client.host.readRun(lease.runId, signal), { id: Number(lease.runId) });
  }
});

test("invalid OIDC endpoints and invalid bearers refuse the binding without fetching", t => {
  const fetcher = t.mock.fn(unexpectedFetch);
  for (const url of ["not a URL", "http://pipelines.actions.githubusercontent.com/token",
    "https://actions.githubusercontent.com/token", "https://pipelines.actions.githubusercontent.com.example.test/token",
    "https://pipelines.actions.githubusercontent.com@example.test/token"]) {
    assert.equal(bindStagingAutomodInstanceClient({ ...env, ACTIONS_ID_TOKEN_REQUEST_URL: url }, serving, fetcher), undefined);
  }
  assert.equal(bindStagingAutomodInstanceClient({ ...env, OWNERSHIP_CONTROL_TOKEN: "c".repeat(31) }, serving, fetcher), undefined);
  for (const token of ["", " leading", "trailing ", "internal space", "tab\tinside", "line\n", "cr\r\n",
    "nul\0byte", "del\x7f", "nonascii-é", "a".repeat(4097)]) {
    assert.equal(bindStagingAutomodInstanceClient({ ...env, GITHUB_TOKEN: token }, serving, fetcher), undefined);
  }
  assert.equal(fetcher.mock.callCount(), 0);
});

test("malformed Actions run IDs refuse before fetching", async t => {
  const fetcher = t.mock.fn(unexpectedFetch);
  const client = bindStagingAutomodInstanceClient(env, serving, fetcher);
  assert.ok(client);
  for (const id of ["", "0", "01", "-1", "1.2", "1e5", "1\n", "1?x=2", "1/../2", "１", "1".repeat(21)]) {
    assert.equal(await client.host.readRun(id, signal), undefined);
  }
  assert.equal(fetcher.mock.callCount(), 0);
});

test("non-200 replies refuse at each request boundary", async () => {
  for (const status of [201, 302, 403]) {
    const reply = () => Response.json({ value: minted }, { status });
    const client = bindStagingAutomodInstanceClient(env, serving, async (url) => {
      assert.notEqual(url, STAGING_AUTOMOD_BRIDGE_URL, "an OIDC refusal must prevent the bridge call");
      return reply();
    });
    assert.ok(client);
    assert.equal(await client.host.readRun(lease.runId, signal), undefined);
    await assert.rejects(client.host.authority.readIssuedLease(lease, signal), /Actions OIDC token unavailable/);

    const bridge = bindStagingAutomodInstanceClient(env, serving, async (url) =>
      url === STAGING_AUTOMOD_BRIDGE_URL ? reply() : Response.json({ value: minted }));
    assert.ok(bridge);
    await assert.rejects(bridge.host.authority.readIssuedLease(lease, signal), /AutoMod bridge refused/);
  }
});

test("duplicate JSON keys are rejected at each request boundary", async () => {
  for (const boundary of ["run", "oidc", "bridge"]) {
    const client = bindStagingAutomodInstanceClient(env, serving, async (url) => {
      if (url === runUrl && boundary === "run") return new Response('{"run":{"id":1,"id":2}}');
      if (url instanceof URL) {
        if (boundary === "oidc") return new Response('{"value":"one","value":"two"}');
        return Response.json({ value: minted });
      }
      if (url === STAGING_AUTOMOD_BRIDGE_URL && boundary === "bridge") return new Response('{"value":{},"value":null}');
      return unexpectedFetch();
    });
    assert.ok(client);
    const result = boundary === "run" ? client.host.readRun(lease.runId, signal) : client.host.authority.readIssuedLease(lease, signal);
    await assert.rejects(result, /duplicate_json_key/);
  }
});

test("Actions run reads accept 1 MiB and reject a reply one byte larger", async () => {
  const json = '{"id":12345}';
  let bytes = maxRunBytes;
  const client = bindStagingAutomodInstanceClient(env, serving, async (url) => {
    assert.equal(url, runUrl);
    return new Response(json.padEnd(bytes, " "));
  });
  assert.ok(client);
  assert.deepEqual(await client.host.readRun(lease.runId, signal), { id: 12345 });
  bytes++;
  await assert.rejects(client.host.readRun(lease.runId, signal), /AutoMod bridge response unavailable/);
});

test("oversized OIDC and bridge replies fail closed", async () => {
  const oidc = bindStagingAutomodInstanceClient(env, serving, async (url) => {
    assert.ok(url instanceof URL, "an oversized OIDC reply must prevent the bridge call");
    return new Response(JSON.stringify({ value: minted }).padEnd(16_385, " "));
  });
  assert.ok(oidc);
  await assert.rejects(oidc.host.authority.readIssuedLease(lease, signal), /AutoMod bridge response unavailable/);

  const bridge = bindStagingAutomodInstanceClient(env, serving, async (url) => url === STAGING_AUTOMOD_BRIDGE_URL
    ? new Response('{"value":null}'.padEnd(STAGING_AUTOMOD_BODY_BYTES + 1, " ")) : Response.json({ value: minted }));
  assert.ok(bridge);
  await assert.rejects(bridge.host.authority.readIssuedLease(lease, signal), /AutoMod bridge response unavailable/);
});
