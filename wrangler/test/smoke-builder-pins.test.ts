/**
 * Builder-owned pins for the bounded smoke probe (bin/smoke.mjs).
 *
 * These cases pin behaviors the protected acceptance suite
 * (smoke-bounded-intake-acceptance.test.ts) does not execute: the
 * fetch-rejection transport path, the JSON-mode --allow-indexable
 * suppression, and fragment-level no-echo of hostile values. The builder
 * must not edit the protected file, so the pins live here instead.
 *
 * Offline only: rejecting/injected fetchImpl fakes plus loopback 127.0.0.1
 * stubs for the CLI spawn. No staging, production, database, Discord or
 * secret is contacted.
 * Run: npm run test:smoke (from wrangler/).
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { VERSION_HEADER, smoke } from "../../bin/smoke.mjs";

const WORKER = "two-bot-next";
const NEW_VERSION = "22345678-1234-1234-1234-123456789abc";
const QUOTED = `${WORKER}="${NEW_VERSION}"`;
const HEALTH_SMALL = JSON.stringify({ status: "ok" });
const READYZ_SMALL = JSON.stringify({ build_revision: "smoke-pin", ok: true });

// Hostile no-echo fixtures, runtime-built so no credential-shaped literal
// appears in source (secret scanners flag such literals even in tests).
const SECRET_TOKEN = ["synthetic", "pin-token", "must-not-leak", "0123456789abcdef"].join("-");
const SECRET_KEY = ["synthetic", "pin-key", "must-not-leak", "0123456789"].join("-");
const OVERLONG = "a".repeat(5000);
const CONTROL_NEWLINE = "evil-version\ninjected-header: yes";
const CONTROL_CRLF = "bad\r\nContent-Length: 9999";
const CONTROL_NUL = "nul-\u0000-ctrl-\u0001-end";
const EVIL_URL = "https://evil.example/exfil?token=abc123";
const EVIL_EMAIL = "secret-leak@example.com";

const HOSTILE_VERSIONS = [
  CONTROL_NEWLINE,
  CONTROL_CRLF,
  CONTROL_NUL,
  OVERLONG,
  SECRET_TOKEN,
  SECRET_KEY,
  EVIL_URL,
  EVIL_EMAIL,
];

function quiet() {
  const lines: string[] = [];
  return { log: (line: string) => void lines.push(line), lines };
}

function healthyFetch() {
  const fetchImpl = async (input: unknown) => {
    const url = input instanceof URL ? input : new URL(String(input));
    const body = url.pathname === "/health" ? HEALTH_SMALL : READYZ_SMALL;
    return new Response(body, {
      status: 200,
      headers: {
        "content-type": "application/json; charset=utf-8",
        "x-content-type-options": "nosniff",
        [VERSION_HEADER]: NEW_VERSION,
      },
    });
  };
  return fetchImpl as typeof fetch;
}

/** Bypass Headers validation so hostile/control values reach the checker. */
function hostileFetch(handler: (url: URL) => { version?: string; security?: string; contentType?: string }) {
  const fetchImpl = async (input: unknown) => {
    const url = input instanceof URL ? input : new URL(String(input));
    const opts = handler(url);
    const bodyText = url.pathname === "/health" ? HEALTH_SMALL : READYZ_SMALL;
    const bytes = new TextEncoder().encode(bodyText);
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(bytes);
        controller.close();
      },
    });
    const map: Record<string, string> = {};
    if (opts.version !== undefined) map[VERSION_HEADER] = opts.version;
    if (opts.security !== undefined) map["x-content-type-options"] = opts.security;
    if (opts.contentType !== undefined) map["content-type"] = opts.contentType;
    return {
      status: 200,
      headers: { get: (name: string) => map[name.toLowerCase()] ?? null },
      body: stream,
    } as unknown as Response;
  };
  return fetchImpl as typeof fetch;
}

// Async CLI runner: spawnSync would block the event loop and starve the
// in-process stub server while the child waits for a response.
function runSmoker(
  args: string[],
  timeoutMs = 15_000,
): Promise<{ status: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, args, {});
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", reject);
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`smoker timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    child.on("close", (status) => {
      clearTimeout(timer);
      resolve({ status, stdout, stderr });
    });
  });
}

/**
 * Fragments a hostile value must never leak: the full value, its
 * JSON-escaped form, a fixed-length prefix, and every control-delimited
 * run of at least 12 characters, each checked once.
 */
function echoFragments(value: string): string[] {
  const escaped = JSON.stringify(value).slice(1, -1);
  const segments = value.split(/[\u0000-\u001f]+/).filter((part) => part.length >= 12);
  return [...new Set([value, escaped, value.slice(0, 32), ...segments])];
}

function assertNoEchoFragments(lines: string[], forbidden: string[], label: string) {
  for (const secret of forbidden) {
    for (const fragment of echoFragments(secret)) {
      assert.ok(
        lines.every((line) => !line.includes(fragment)),
        `${label}: diagnostics must never echo ${JSON.stringify(fragment.slice(0, 40))}`,
      );
    }
  }
}

test("pin: rejected fetch becomes a probe_error fail verdict, never a throw", async () => {
  const fetchImpl = (async () => {
    throw new TypeError("fetch failed");
  }) as unknown as typeof fetch;
  const { log, lines } = quiet();
  const ok = await smoke("http://probe.invalid", { log, fetchImpl, json: true } as never);
  assert.equal(ok, false, "rejected fetch fails the probe instead of throwing");
  assert.equal(lines.length, 1, "JSON mode still logs exactly one line on transport failure");
  const doc = JSON.parse(lines[0]) as Record<string, unknown>;
  assert.equal(doc["schema"], "smoke.v1");
  assert.equal(doc["verdict"], "fail");
  assert.equal(doc["reason_code"], "health_status_probe_error");
  assert.equal(doc["next_action"], "retry_probe");
  const routes = doc["routes"] as Array<Record<string, unknown>>;
  assert.deepEqual(routes.map((r) => r["id"]), ["health", "readyz"]);
  for (const route of routes) {
    assert.equal(route["ok"], false);
    for (const check of route["checks"] as Array<Record<string, unknown>>) {
      assert.equal(check["code"], "probe_error");
    }
  }
});

test("pin: CLI against a closed port exits 1 with one JSON fail verdict", async (t) => {
  // Reserve a loopback port, then close it: the probe below is guaranteed
  // connection-refused without depending on any fixed port being free.
  const holder = createServer();
  holder.listen(0, "127.0.0.1");
  await once(holder, "listening");
  const address = holder.address();
  assert.ok(address !== null && typeof address === "object");
  const port = (address as { port: number }).port;
  await new Promise<void>((resolve) => holder.close(() => resolve()));

  const smoker = fileURLToPath(new URL("../../bin/smoke.mjs", import.meta.url));
  const run = await runSmoker([smoker, `http://127.0.0.1:${port}`, "--json"]);
  assert.equal(run.status, 1, `closed-port --json exits 1, got ${run.status}\n${run.stderr}`);
  assert.equal(run.stderr, "", "a failed probe reports on stdout, not stderr");
  const stdoutLines = run.stdout.split("\n").filter((line) => line.length > 0);
  assert.equal(stdoutLines.length, 1, "closed-port --json logs exactly one line");
  const doc = JSON.parse(stdoutLines[0]) as Record<string, unknown>;
  assert.equal(doc["verdict"], "fail");
  assert.equal(doc["reason_code"], "health_status_probe_error");
  assert.equal(doc["next_action"], "retry_probe");
  t.after(() => undefined);
});

test("pin: --allow-indexable stays silent in JSON mode", async () => {
  const { log, lines } = quiet();
  const ok = await smoke("http://probe.invalid", {
    log,
    fetchImpl: healthyFetch(),
    json: true,
    allowIndexable: true,
  } as never);
  assert.equal(ok, true, "allow-indexable stays a passing no-op in JSON mode");
  assert.equal(lines.length, 1, "JSON mode logs exactly one line with --allow-indexable");
  const doc = JSON.parse(lines[0]) as Record<string, unknown>;
  assert.equal(doc["verdict"], "pass");
  assert.equal(doc["reason_code"], "ok");
});

test("pin: CLI --json --allow-indexable keeps stdout parseable", async (t) => {
  const server = createServer((request, response) => {
    const body = request.url === "/health" ? HEALTH_SMALL : READYZ_SMALL;
    response.writeHead(200, {
      "content-type": "application/json; charset=utf-8",
      "x-content-type-options": "nosniff",
      [VERSION_HEADER]: NEW_VERSION,
    });
    response.end(body);
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => new Promise<void>((resolve) => server.close(() => resolve())));
  const address = server.address();
  assert.ok(address !== null && typeof address === "object");
  const base = `http://127.0.0.1:${(address as { port: number }).port}`;

  const smoker = fileURLToPath(new URL("../../bin/smoke.mjs", import.meta.url));
  const run = await runSmoker([smoker, base, "--json", "--allow-indexable"]);
  assert.equal(run.status, 0, `--json --allow-indexable exits 0, got ${run.status}\n${run.stderr}`);
  const doc = JSON.parse(run.stdout) as Record<string, unknown>;
  assert.equal(doc["verdict"], "pass");
  assert.deepEqual(
    (doc["routes"] as Array<Record<string, unknown>>).map((r) => r["id"]),
    ["health", "readyz"],
  );
});

test("pin: hostile values never echo, even partially or escaped", async () => {
  const hostileContentType = `application/json; probe="${SECRET_TOKEN}"`;
  for (const override of [undefined, QUOTED] as const) {
    const label = override ? "override" : "live";
    for (const hostile of HOSTILE_VERSIONS) {
      const fetchImpl = hostileFetch(() => ({
        version: hostile,
        security: CONTROL_CRLF,
        contentType: hostileContentType,
      }));
      const { log, lines } = quiet();
      const ok = await smoke("http://probe.invalid", {
        log,
        fetchImpl,
        versionOverride: override,
      });
      assert.equal(ok, false, `${label}: hostile headers fail, never pass`);
      assertNoEchoFragments(
        lines,
        [hostile, CONTROL_CRLF, hostileContentType, SECRET_TOKEN],
        `${label} no-echo`,
      );
    }
  }
});
