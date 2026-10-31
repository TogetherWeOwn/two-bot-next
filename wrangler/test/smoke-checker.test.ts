/**
 * Gradual-promote smoke checker contract (bin/smoke.mjs).
 *
 * Pins the caller contract the shared central gradual-promote action relies
 * on: `node bin/smoke.mjs "https://$DOMAIN" [--allow-indexable]
 * --version-override '<worker-name>="<lowercase-version-uuid>"'` at the 5%,
 * 50% and 100-override stages (pinned to the NEW version) and without the
 * override at the apex/live stage. The action retries 6x10s; this file does
 * one pass. Loopback stub only: no staging call, no database, no secret.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { createServer } from "node:http";
import { once } from "node:events";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  OVERRIDE_HEADER,
  VERSION_HEADER,
  VERSION_OVERRIDE_PATTERN,
  assertVersionOverride,
  parseArgs,
  smoke,
} from "../../bin/smoke.mjs";

const WORKER = "two-bot-next";
const NEW_VERSION = "22345678-1234-1234-1234-123456789abc";
const QUOTED = `${WORKER}="${NEW_VERSION}"`;

function healthyBodies(origin: string) {
  void origin;
  return {
    "/health": {
      status: 200,
      type: "application/json",
      body: JSON.stringify({ status: "ok" }),
    },
    "/readyz": {
      status: 200,
      type: "application/json",
      body: JSON.stringify({
        health: {
          components: [
            ["process", "ready"],
            ["gateway", "ready"],
            ["database", "ready"],
          ],
        },
        build_revision: "abc123def456abc123def456abc123def456abc1",
        build_id: "1-1",
      }),
    },
  };
}

interface Seen {
  path: string | undefined;
  override: string | string[] | undefined;
}

async function stub(
  t: { after: (fn: () => Promise<void>) => void },
  opts: {
    version?: string;
    readyzStatus?: number;
    healthBody?: string;
    omitVersion?: boolean;
  } = {},
) {
  const seen: Seen[] = [];
  const version = opts.version ?? NEW_VERSION;
  const server = createServer((request, response) => {
    seen.push({
      path: request.url,
      override: request.headers[OVERRIDE_HEADER.toLowerCase()],
    });
    const bodies = healthyBodies(`http://${request.headers.host}`);
    if (request.url === "/health" && opts.healthBody !== undefined) {
      response.writeHead(200, {
        "content-type": "application/json; charset=utf-8",
        "x-content-type-options": "nosniff",
        ...(opts.omitVersion ? {} : { [VERSION_HEADER]: version }),
      });
      response.end(opts.healthBody);
      return;
    }
    const fixture = (bodies as Record<string, { status: number; type: string; body: string }>)[
      request.url ?? ""
    ];
    if (!fixture) {
      response.writeHead(500).end("Unexpected request");
      return;
    }
    let status = fixture.status;
    let body = fixture.body;
    if (request.url === "/readyz" && opts.readyzStatus !== undefined) {
      status = opts.readyzStatus;
      if (status !== 200) body = JSON.stringify({ health: { components: [] } });
    }
    response.writeHead(status, {
      "content-type": `${fixture.type}; charset=utf-8`,
      "x-content-type-options": "nosniff",
      ...(opts.omitVersion ? {} : { [VERSION_HEADER]: version }),
    });
    response.end(body);
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(
    () =>
      new Promise<void>((resolve, reject) => {
        server.closeAllConnections();
        server.close((error) => (error ? reject(error) : resolve()));
      }),
  );
  const address = server.address();
  assert.ok(address !== null && typeof address === "object");
  return { host: `127.0.0.1:${(address as { port: number }).port}`, seen };
}

function quiet() {
  const lines: string[] = [];
  return { log: (line: string) => void lines.push(line), lines };
}

test("version-override accepts the Cloudflare dictionary form and rejects the review bug", () => {
  assert.equal(assertVersionOverride(QUOTED), QUOTED);
  assert.ok(VERSION_OVERRIDE_PATTERN.test(QUOTED));
  // The review bug this pins: the composite action once passed
  // --version-override "$WORKER_NAME=$NEW_VERSION" (unquoted) while the
  // checker only accepts worker-name="<uuid>". Every canary gate failed.
  for (const bad of [
    `${WORKER}=${NEW_VERSION}`,
    `${WORKER}='${NEW_VERSION}'`,
    `${WORKER}="${NEW_VERSION.toUpperCase()}"`,
    `"${NEW_VERSION}"`,
    `${WORKER}=""`,
    "",
  ]) {
    assert.throws(() => assertVersionOverride(bad), /version-override must look like/);
  }
});

test("parseArgs accepts the action's exact stage shapes", () => {
  // 5%, 50%, 100-override: pinned canary with the quoted override.
  for (const extra of [[], ["--allow-indexable"]]) {
    const parsed = parseArgs([
      "https://probe.invalid",
      ...extra,
      "--version-override",
      QUOTED,
    ]);
    assert.equal(parsed.baseUrl, "https://probe.invalid");
    assert.equal(parsed.versionOverride, QUOTED);
    assert.equal(parsed.allowIndexable, extra.length > 0);
  }
  // Apex/live: no override, with the indexable flag.
  const live = parseArgs(["https://probe.invalid", "--allow-indexable"]);
  assert.equal(live.versionOverride, undefined);
  assert.equal(live.allowIndexable, true);
  // Unquoted override is a usage shape the action must never send.
  assert.throws(
    () => parseArgs(["https://probe.invalid", "--version-override", `${WORKER}=${NEW_VERSION}`]),
    /version-override must look like/,
  );
  // Unknown flags and missing override values are usage errors.
  assert.throws(() => parseArgs(["https://probe.invalid", "--nope"]), /Usage/);
  assert.throws(() => parseArgs(["https://probe.invalid", "--version-override"]), /Usage/);
  assert.throws(() => parseArgs([]), /Usage/);
});

test("smoke passes pinned to the new version and sends the override header", async (t) => {
  const { host, seen } = await stub(t);
  const { log } = quiet();
  const ok = await smoke(`http://${host}`, {
    log,
    versionOverride: QUOTED,
    fetchImpl: fetch,
  });
  assert.equal(ok, true);
  assert.equal(seen.length, 2);
  for (const request of seen) {
    assert.equal(request.override, QUOTED);
  }
});

test("smoke passes live without an override and with --allow-indexable (no-op)", async (t) => {
  const { host, seen } = await stub(t);
  const { log } = quiet();
  const ok = await smoke(`http://${host}`, { log, allowIndexable: true, fetchImpl: fetch });
  assert.equal(ok, true);
  for (const request of seen) {
    assert.equal(request.override, undefined);
  }
});

test("smoke fails when the edge serves the old version under an override", async (t) => {
  const { host } = await stub(t, { version: "11111111-2222-3333-4444-555555555555" });
  const { log, lines } = quiet();
  const ok = await smoke(`http://${host}`, {
    log,
    versionOverride: QUOTED,
    fetchImpl: fetch,
  });
  assert.equal(ok, false);
  assert.ok(lines.some((line) => line.includes("pinned canary")));
});

test("smoke fails when the version header is missing", async (t) => {
  const { host } = await stub(t, { omitVersion: true });
  const { log } = quiet();
  const ok = await smoke(`http://${host}`, {
    log,
    versionOverride: QUOTED,
    fetchImpl: fetch,
  });
  assert.equal(ok, false);
});

test("smoke fails on /readyz 503 (gateway not ready)", async (t) => {
  const { host } = await stub(t, { readyzStatus: 503 });
  const { log } = quiet();
  const ok = await smoke(`http://${host}`, {
    log,
    versionOverride: QUOTED,
    fetchImpl: fetch,
  });
  assert.equal(ok, false);
});

test("smoke fails on a non-ok /health body", async (t) => {
  const { host } = await stub(t, { healthBody: JSON.stringify({ status: "down" }) });
  const { log } = quiet();
  const ok = await smoke(`http://${host}`, {
    log,
    versionOverride: QUOTED,
    fetchImpl: fetch,
  });
  assert.equal(ok, false);
});

test("smoke rejects non-origin base URLs", async () => {
  const { log } = quiet();
  for (const bad of [
    "https://probe.invalid/health",
    "https://probe.invalid?token=1",
    "https://user:pass@probe.invalid/",
    "ftp://probe.invalid/",
  ]) {
    await assert.rejects(
      smoke(bad, { log, fetchImpl: fetch }),
      /base-url must be an HTTP\(S\) origin/,
    );
  }
});

test("smoke CLI refuses an invalid URL through symlinks and spaced paths", (t) => {
  const root = mkdtempSync(join(tmpdir(), "smoke link "));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const real = fileURLToPath(new URL("../../bin/smoke.mjs", import.meta.url));
  const link = join(root, "smoke.mjs");
  const spacedDir = join(root, "spaced dir");
  const spaced = join(spacedDir, "smoke.mjs");
  mkdirSync(spacedDir);
  symlinkSync(real, link);
  symlinkSync(real, spaced);

  for (const argv1 of [real, link, spaced, relative(process.cwd(), link)]) {
    const executed = spawnSync(process.execPath, [argv1, "not-a-url"], {
      env: {},
      encoding: "utf8",
      timeout: 15_000,
    });
    assert.equal(executed.error, undefined, argv1);
    assert.equal(executed.signal, null, argv1);
    assert.equal(executed.status, 2, `${argv1}\n${executed.stderr}`);
    assert.equal(executed.stdout, "", argv1);
    assert.equal(executed.stderr, "smoke: Invalid URL\n", argv1);
  }

  const imported = spawnSync(
    process.execPath,
    ["--input-type=module", "--eval", `await import(${JSON.stringify(pathToFileURL(link).href)})`],
    { env: {}, encoding: "utf8", timeout: 15_000 },
  );
  assert.equal(imported.error, undefined);
  assert.equal(imported.signal, null);
  assert.equal(imported.status, 0);
  assert.equal(imported.stdout, "");
  assert.equal(imported.stderr, "");
});
