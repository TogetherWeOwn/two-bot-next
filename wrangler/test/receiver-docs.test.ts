/** Offline runbook conformance: real ingress outputs and DO startup env, no network. */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { TwoBotContainer, type Env } from "../src/index.ts";
import { NOT_FORWARDED } from "../src/container-env.ts";
import {
  ACTIONS_PATH, BODY_TIMEOUT_MS, CONTENT_TYPE_MAX_BYTES, IDEMPOTENCY_HEADER, REQUEST_TIMEOUT_MS, SIGNING_HEADERS,
  allowlistedHeaders, handleActionsIngress, resetIngressLimits,
} from "../src/internal-actions.ts";
import { REQUEST_ID_HEADER, REQUEST_ID_LEN, isRequestId, newRequestId } from "../src/request-id.ts";
import { RELEASE_TOKEN, normalizeRelease } from "../src/error-events.ts";
import { LOG_QUERY_URL } from "../src/alert-rules.ts";

const doc = readFileSync(new URL("../../docs/internal-actions-receiver.md", import.meta.url), "utf8");
const id = newRequestId();
const headers = {
  "content-type": "application/json",
  "x-two-key-id": "fixture",
  "x-two-timestamp": "1790000000",
  "x-two-nonce": "synthetic-nonce",
  "x-two-signature": `sha256=${"a".repeat(64)}`,
  "idempotency-key": `intent-${randomBytes(6).toString("hex")}`,
  [REQUEST_ID_HEADER]: id,
};
const blocked = Object.entries(NOT_FORWARDED).filter(([key, reason]) =>
  (key.startsWith("TWO_INTERNAL_ALLOW_") || key === "TWO_INTERNAL_ROLE_KEYS") && !reason.includes("test-only"))
  .map(([key]) => key);
const turn = () => new Promise<void>((resolve) => setImmediate(resolve));

function post(body: BodyInit): Request {
  const init: RequestInit & { duplex: "half" } = { method: "POST", headers, body, duplex: "half" };
  return new Request(`https://worker.invalid${ACTIONS_PATH}`, init);
}

function startupEnv(): Record<string, string> {
  const database = new DatabaseSync(":memory:");
  try {
    const ctx = {
      container: { running: false },
      storage: { kv: { get: () => undefined }, sql: { exec: (query: string) => database.prepare(query).all() } },
      blockConcurrencyWhile: () => Promise.resolve(),
    };
    const env = {
      TWO_INTERNAL_ACTIONS: "1", TWO_INTERNAL_CALLERS: "fixture:website",
      TWO_INTERNAL_CHANNEL_KEYS: "fixture:123456789012345678",
      TWO_INTERNAL_KEYS: `fixture:${randomBytes(32).toString("hex")}`,
      ...Object.fromEntries(blocked.map((key) => [key, key.endsWith("ROLE_KEYS") ? "member:123456789012345678" : "1"])),
    };
    return new TwoBotContainer(ctx as unknown as DurableObjectState, env as unknown as Env).envVars;
  } finally {
    database.close();
  }
}

function table(markdown: string, heading: string): string[][] {
  const lines = markdown.split("\n");
  const start = lines.findIndex((line) => line.startsWith(`| ${heading} |`));
  assert.notEqual(start, -1, `missing ${heading} table`);
  const rows: string[][] = [];
  for (const line of lines.slice(start + 2)) {
    if (!line.startsWith("|")) break;
    rows.push(line.split("|").slice(1, -1).map((cell) => cell.trim().replaceAll("`", "")));
  }
  return rows;
}

interface Refusal { status: number; code: string; retryable: boolean }
interface Evidence {
  intake: Refusal;
  forwarding: Refusal;
  forwardedHeaders: string[];
  log: Record<string, unknown>;
  env: Record<string, string>;
}

async function refusal(response: Response): Promise<Refusal> {
  const body = await response.json() as { request_id: string; error: { code: string; message: string; retryable: boolean } };
  assert.equal(body.request_id, id);
  assert.equal(body.error.message, "request refused");
  return { status: response.status, code: body.error.code, retryable: body.error.retryable };
}

async function observe(t: TestContext): Promise<Evidence> {
  resetIngressLimits();
  t.after(() => resetIngressLimits());
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const signal = t.mock.method(AbortSignal, "timeout", () => new AbortController().signal);
  const logs: Record<string, unknown>[] = [];
  t.mock.method(console, "warn", (line: string) => { logs.push(JSON.parse(line)); });
  let calls = 0;
  const intakePending = handleActionsIngress(post(new ReadableStream()), "doc-intake", async () => {
    calls++;
    return Response.json({});
  });
  await turn();
  t.mock.timers.tick(BODY_TIMEOUT_MS);
  const intake = await refusal(await intakePending);
  assert.equal(calls, 0, "intake expires before forwarding");
  assert.equal(signal.mock.calls.length, 0, "forwarding budget does not start during intake");

  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const body = new ReadableStream<Uint8Array>({ start(value) { controller = value; } });
  let finish!: (response: Response) => void;
  let settled = false;
  const forwardingPending = handleActionsIngress(post(body), "doc-forward", () => {
    calls++;
    return new Promise<Response>((resolve) => { finish = resolve; });
  }, "not-a-sha").then((response) => { settled = true; return response; });
  await turn();
  t.mock.timers.tick(BODY_TIMEOUT_MS - 1);
  assert.equal(signal.mock.calls.length, 0);
  controller.enqueue(new TextEncoder().encode("{}"));
  controller.close();
  await turn();
  assert.equal(calls, 1);
  assert.deepEqual(signal.mock.calls.map((call) => call.arguments), [[REQUEST_TIMEOUT_MS]]);
  // At 20 seconds since intake began, forwarding still has its own remaining budget.
  t.mock.timers.tick(REQUEST_TIMEOUT_MS - (BODY_TIMEOUT_MS - 1));
  await turn();
  assert.equal(settled, false, "not a whole-request twenty-second deadline");
  t.mock.timers.tick(BODY_TIMEOUT_MS - 1);
  const forwarding = await refusal(await forwardingPending);
  finish(Response.json({}));
  await turn();

  const checked = allowlistedHeaders(new Headers({ ...headers, authorization: "synthetic-private", cookie: "synthetic-private", "x-two-bot-deployment-id": "caller-value" }));
  assert.ok("headers" in checked);
  assert.equal(checked.headers.get(REQUEST_ID_HEADER), id);
  for (const inbound of [null, "Bearer synthetic-private", "bad,id"]) {
    const source = new Headers(headers);
    if (inbound === null) source.delete(REQUEST_ID_HEADER);
    else source.set(REQUEST_ID_HEADER, inbound);
    const minted = allowlistedHeaders(source);
    assert.ok("headers" in minted);
    assert.ok(isRequestId(minted.headers.get(REQUEST_ID_HEADER)!));
    assert.notEqual(minted.headers.get(REQUEST_ID_HEADER), inbound);
  }
  const log = logs.at(-1)!;
  assert.equal(log["release"], normalizeRelease("not-a-sha"));
  assert.equal(log["log_url"], LOG_QUERY_URL);
  assert.equal(log["request_id"], id);
  return { intake, forwarding, forwardedHeaders: [...checked.headers.keys()], log, env: startupEnv() };
}

function conforms(markdown: string, evidence: Evidence): void {
  const phases = table(markdown, "Phase");
  assert.deepEqual(phases.map((row) => row.slice(0, 7)), [
    ["Intake", String(BODY_TIMEOUT_MS), "Before forwarding", String(evidence.intake.status), evidence.intake.code, String(evidence.intake.retryable), "Retry same Idempotency-Key; fresh nonce/signature"],
    ["Forwarding", String(REQUEST_TIMEOUT_MS), "After body read", String(evidence.forwarding.status), evidence.forwarding.code, String(evidence.forwarding.retryable), "Unknown outcome: retry same Idempotency-Key; reconcile needs_reconciliation first"],
  ]);
  assert.doesNotMatch(markdown, /whole request within 20 s/i, "stale whole-request promise");
  const forwarded = table(markdown, "Forwarded header");
  assert.deepEqual(forwarded.map((row) => row[0]!.toLowerCase()).sort(), evidence.forwardedHeaders.sort());
  for (const [name, cap] of SIGNING_HEADERS) {
    assert.match(forwarded.find((row) => row[0] === name)?.[1] ?? "", new RegExp(`at most ${cap} bytes`));
  }
  assert.match(forwarded.find((row) => row[0] === IDEMPOTENCY_HEADER[0])?.[1] ?? "", new RegExp(`at most ${IDEMPOTENCY_HEADER[1]} bytes`));
  assert.match(forwarded.find((row) => row[0] === "content-type")?.[1] ?? "", new RegExp(`at most ${CONTENT_TYPE_MAX_BYTES} bytes`));
  const requestId = forwarded.find((row) => row[0] === REQUEST_ID_HEADER)!;
  // Digit boundary: doc mutant "126-character" must not satisfy a 26 pin.
  assert.match(requestId[1]!, new RegExp(`validated or minted.*(?<!\\d)${REQUEST_ID_LEN}-character Crockford.*not identity`, "i"));
  assert.doesNotMatch(markdown, /Only .*X-TWO-Key-Id.*reach the container/);
  const fields = table(markdown, "Log field");
  assert.deepEqual(fields.map((row) => row[0]).sort(), Object.keys(evidence.log).sort());
  assert.equal(fields.find((row) => row[0] === "log_url")?.[1], evidence.log["log_url"]);
  const releaseCell = fields.find((row) => row[0] === "release")?.[1] ?? "";
  assert.match(releaseCell, /normalized.*unknown/i);
  const releaseRange = RELEASE_TOKEN.source.match(/\{(\d+),(\d+)\}/);
  assert.ok(releaseRange, "release token carries explicit bounds");
  const [releaseLow, releaseHigh] = [Number(releaseRange[1]), Number(releaseRange[2])];
  // Digit boundaries: doc mutants "7–400" and "17–40" must not satisfy a 7–40 pin.
  assert.match(releaseCell, new RegExp(`(?<!\\d)${releaseLow}[–-]${releaseHigh}(?!\\d).*lowercase hex`, "i"));
  // Lowercase-only class, pinned from source and behavior: an uppercase-accepting
  // RELEASE_TOKEN mutant must fail even though the doc wording still says lowercase.
  assert.ok(RELEASE_TOKEN.source.includes("[0-9a-f]"), "release token is lowercase hex");
  assert.ok(!RELEASE_TOKEN.source.includes("A-F"), "release token must not accept uppercase");
  assert.equal(RELEASE_TOKEN.test("a".repeat(releaseLow)), true);
  assert.equal(RELEASE_TOKEN.test("a".repeat(releaseLow - 1)), false);
  assert.equal(RELEASE_TOKEN.test("a".repeat(releaseHigh)), true);
  assert.equal(RELEASE_TOKEN.test("a".repeat(releaseHigh + 1)), false);
  assert.equal(RELEASE_TOKEN.test("A".repeat(releaseLow)), false, "release token rejects uppercase");
  assert.doesNotMatch(markdown, /status.*and.*error_class.*only/i, "stale log-field promise");
  const wiring = table(markdown, "Name").map((row) => row[0]!);
  assert.deepEqual(wiring.sort(), Object.keys(evidence.env).filter((key) => key.startsWith("TWO_INTERNAL_")).sort());
  assert.deepEqual(table(markdown, "Not forwarded").map((row) => row[0]).sort(), blocked.sort());
  for (const key of blocked) assert.equal(evidence.env[key], undefined, key);
  const gate = table(markdown, "Layer").find((row) => row[0] === "Gate")?.[1] ?? "";
  assert.match(gate, /INTERNAL_ACTIONS_INGRESS\s*=\s*"1"/, "missing staging var gate");
  assert.match(gate, /TWO_INTERNAL_ACTIONS.*exactly 1(?!\d)/, "missing Operator secret gate");
  const enable = markdown.split("### Enable order and rollback")[1]!.split("## Authentication")[0]!;
  assert.doesNotMatch(enable, /stages[^\n]*TWO_INTERNAL_ROLE_KEYS/, "ineffective role-key staging instruction");
  assert.match(markdown, /Adding secrets cannot enable unforwarded role\/capability gates/);
  assert.match(markdown, /Never[^\n]*header values, nonce, signature, body or query/);
}

test("receiver runbook matches observed budgets, headers, logs and container wiring", async (t) => {
  const evidence = await observe(t);
  conforms(doc, evidence);
  const stale = [
    ["whole-request budget", doc.replace("separate budgets below", "whole request within 20 s (504)")],
    ["missing request id", doc.replace(/^\| x-request-id \|.*\n/m, "")],
    ["old logs only", doc.replace(/^\| Logs \|.*$/m, "| Logs | Scalar internal_actions_ingress events: status and error_class only. |")],
    ["missing release", doc.replace(/^\| release \|.*\n/m, "")],
    ["forwarded role keys", doc.replace("| `TWO_INTERNAL_KEYS` |", "| `TWO_INTERNAL_ROLE_KEYS` | Worker secret |\n| `TWO_INTERNAL_KEYS` |")],
    ["stage role keys", doc.replace("2. Operator stages", "2. Operator stages `TWO_INTERNAL_ROLE_KEYS`, then stages")],
    ["wrong timeout code", doc.replace("| `upstream_timeout` | true |", "| `request_timeout` | true |")],
    ["stale idempotency cap", doc.replace(/^\| idempotency-key \|.*$/m, "| idempotency-key | Optional at ingress; at most 999 bytes. Receiver action rules still apply |")],
    ["stale content-type cap", doc.replace(/^\| content-type \|.*$/m, "| content-type | Required accepted JSON media type; at most 999 bytes |")],
    ["stale release range", doc.replace("7–40", "7–99")],
    ["release range suffix mutant", doc.replace("7–40", "7–400")],
    ["release range prefix mutant", doc.replace("7–40", "17–40")],
    ["gate count prefix mutant", doc.replace("`TWO_INTERNAL_ACTIONS` exactly `1`", "`TWO_INTERNAL_ACTIONS` exactly `12`")],
    ["request-id length prefix mutant", doc.replace(`${REQUEST_ID_LEN}-character`, `1${REQUEST_ID_LEN}-character`)],
    ["single-gate enablement", doc.replace("INTERNAL_ACTIONS_INGRESS", "REMOVED_GATE")],
  ];
  for (const [name, mutant] of stale) {
    await t.test(`rejects ${name}`, () => {
      assert.notEqual(mutant, doc, "negative fixture must change the document");
      assert.throws(() => conforms(mutant!, evidence), { name: "AssertionError" });
    });
  }
});
