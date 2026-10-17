/**
 * TOG-28739 acceptance: request-span container correlation.
 *
 * Span/dispatch lines (`internal_actions_ingress`, `eventsub_ingress`) must
 * carry the container Git SHA and a working log-query URL, mirroring what
 * alert lines already carry via TOG-26876 (`release` + `log_url` on
 * `container_gateway_failure` / `metrics_alert`). This is the test half of
 * the R4 pair: the builder (TOG-28743) implements against this file and
 * never edits it.
 *
 * Current status: RED. Both `refuse()` formatters
 * (`wrangler/src/internal-actions.ts:127`,
 * `wrangler/src/eventsub.ts:137`) log only
 * `{event, status, error_class, request_id}`. These tests fail until the
 * builder stamps `release` + `log_url` on those lines.
 *
 * NOTE to the builder: the exact-keys assertions in
 * `test/internal-actions.test.ts` ("logs carry only scalar event, status,
 * error class and request id") and `test/eventsub.test.ts` (same name) pin
 * the old 4-key shape. Update those two assertions to the new 6-key shape
 * when you wire the fields through; this file stays frozen.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/span-correlation.test.ts
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import {
  handleActionsIngress,
  resetIngressLimits,
} from "../src/internal-actions.ts";
import {
  handleEventsubIngress,
  resetEventsubIngressLimits,
} from "../src/eventsub.ts";
import { LOG_QUERY_URL, alertContextSuffix } from "../src/alert-rules.ts";
import { isRequestId } from "../src/request-id.ts";

/** Container SHA or the visible unknown bucket (mirrors error-events.ts). */
const SHA_OR_UNKNOWN = /^(?:[0-9a-f]{7,40}|unknown)$/;

const ACTIONS_SIGNED: Record<string, string> = {
  "content-type": "application/json",
  "x-two-key-id": "web-staging",
  "x-two-timestamp": "1790000000",
  "x-two-nonce": "synthetic-nonce-0001",
  "x-two-signature": `sha256=${"a".repeat(64)}`,
};

const EVENTSUB_DELIVERY: Record<string, string> = {
  "content-type": "application/json",
  "twitch-eventsub-message-id": "synthetic-delivery-0001",
  "twitch-eventsub-message-timestamp": "2026-10-17T00:00:00Z",
  "twitch-eventsub-message-type": "notification",
  "twitch-eventsub-message-signature": `sha256=${"b".repeat(64)}`,
};

function actionsRefusal(headers: Record<string, string>): Request {
  return new Request("https://worker.invalid/internal/actions", {
    method: "POST",
    headers,
    body: '{"action":"announcement.post"}',
  });
}

function eventsubRefusal(headers: Record<string, string>): Request {
  return new Request("https://worker.invalid/hooks/twitch", {
    method: "POST",
    headers,
    body: '{"subscription":{"type":"stream.online"}}',
  });
}

test("internal_actions_ingress span lines carry container SHA and log-query URL", async (t) => {
  resetIngressLimits();
  t.after(() => resetIngressLimits());
  const lines: string[] = [];
  t.mock.method(console, "warn", (...args: unknown[]) => {
    lines.push(args.map(String).join(" "));
  });
  const client = `span-correlation-${randomBytes(4).toString("hex")}`;
  const response = await handleActionsIngress(
    actionsRefusal({ ...ACTIONS_SIGNED, "content-type": "text/plain" }),
    client,
    async () => new Response("must not forward", { status: 500 }),
  );
  assert.equal(response.status, 415);
  const span = lines.filter((line) => line.includes("internal_actions_ingress"));
  assert.ok(span.length >= 1, `no span line logged: ${JSON.stringify(lines)}`);
  for (const line of span) {
    const record = JSON.parse(line) as Record<string, unknown>;
    assert.ok(
      typeof record["release"] === "string" &&
        SHA_OR_UNKNOWN.test(record["release"] as string),
      `span line lacks container SHA (release): ${line}`,
    );
    assert.equal(
      record["log_url"],
      LOG_QUERY_URL,
      `span line lacks log-query URL: ${line}`,
    );
    assert.ok(isRequestId(String(record["request_id"])), `bad id shape: ${line}`);
  }
});

test("eventsub_ingress span lines carry container SHA and log-query URL", async (t) => {
  resetEventsubIngressLimits();
  t.after(() => resetEventsubIngressLimits());
  const lines: string[] = [];
  t.mock.method(console, "warn", (...args: unknown[]) => {
    lines.push(args.map(String).join(" "));
  });
  const client = `span-correlation-${randomBytes(4).toString("hex")}`;
  const response = await handleEventsubIngress(
    eventsubRefusal({ ...EVENTSUB_DELIVERY, "content-type": "text/plain" }),
    client,
    async () => new Response("must not forward", { status: 500 }),
  );
  assert.equal(response.status, 415);
  const span = lines.filter((line) => line.includes("eventsub_ingress"));
  assert.ok(span.length >= 1, `no span line logged: ${JSON.stringify(lines)}`);
  for (const line of span) {
    const record = JSON.parse(line) as Record<string, unknown>;
    assert.ok(
      typeof record["release"] === "string" &&
        SHA_OR_UNKNOWN.test(record["release"] as string),
      `span line lacks container SHA (release): ${line}`,
    );
    assert.equal(
      record["log_url"],
      LOG_QUERY_URL,
      `span line lacks log-query URL: ${line}`,
    );
    assert.ok(isRequestId(String(record["request_id"])), `bad id shape: ${line}`);
  }
});

test("alert-line correlation shape stays frozen (TOG-26876)", () => {
  // The log-query URL stamped on span lines must be the same stable docs
  // URL the alerts carry: a dashboard-scoped link would leak account
  // context into public-safe log lines.
  assert.equal(
    LOG_QUERY_URL,
    "https://developers.cloudflare.com/workers/observability/logs/workers-logs/",
  );
  const suffix = alertContextSuffix({ buildRevision: "abc1234" });
  assert.ok(suffix.includes("Build: abc1234"), suffix);
  assert.ok(suffix.includes(`Logs: ${LOG_QUERY_URL}`), suffix);
});
