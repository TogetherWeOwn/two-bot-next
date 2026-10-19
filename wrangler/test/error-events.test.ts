/**
 * Release-tagged error events grouped by event-specific fields.
 *
 * Workers Logs is the error tracker (no Sentry, no Gatus): the
 * `container_gateway_failure` and `metrics_alert` events carry `release` (the
 * container git SHA from /readyz `build_revision`) so a new error after a deploy
 * points at that release. Gateway failures group by `class`; metrics alerts
 * group by rule in `content`.
 * Traces stay disabled: fetch spans would record `url.full`, copying the ops
 * webhook token into trace storage.
 * Reads fixtures only; never touches a live Worker.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { normalizeRelease, parseRelease, releaseTag, workerVersionTag } from "../src/error-events.ts";

const SHA = "9f3a1c7e2b4d6a8c0e1f3a5b7c9d2e4f6a8b0c1d";

test("parseRelease extracts the container git SHA from a /readyz body", () => {
  const body = JSON.stringify({
    components: [["process", "ready"]],
    build_revision: SHA,
    build_id: "123-1",
  });
  assert.equal(parseRelease(body), SHA);
});

test("parseRelease accepts a short SHA", () => {
  assert.equal(parseRelease(JSON.stringify({ build_revision: "9f3a1c7" })), "9f3a1c7");
});

test("parseRelease drops everything that is not a hex SHA", () => {
  const hostile = [
    ["missing revision", JSON.stringify({ build_id: "123-1" })],
    ["unknown sentinel", JSON.stringify({ build_revision: "unknown" })],
    ["url as revision", JSON.stringify({ build_revision: "postgres://user:hunter2@db.internal/app" })],
    ["uppercase hex", JSON.stringify({ build_revision: SHA.toUpperCase() })],
    ["too short", JSON.stringify({ build_revision: "9f3a1c" })],
    ["too long", JSON.stringify({ build_revision: `${SHA}ff` })],
    ["numeric revision", JSON.stringify({ build_revision: 7 })],
    ["null body", "null"],
    ["array body", "[]"],
    ["not JSON", "durable_gateway:checkpoint_load_failed"],
  ] as const;
  for (const [name, body] of hostile) {
    assert.equal(parseRelease(body), null, name);
    assert.ok(!String(parseRelease(body) ?? "").includes("hunter2"), name);
  }
});

test("parseRelease never echoes hostile bodies", () => {
  const hostile = JSON.stringify({
    build_revision: "postgres://user:hunter2@db.internal/app",
    error: "token=hunter2",
  });
  assert.equal(parseRelease(hostile), null);
});

test("parseRelease rejects an oversize /readyz body without parsing it", () => {
  const marker = `synthetic-release-oversize-${"r".repeat(64)}`;
  const body = `${JSON.stringify({ build_revision: SHA, marker })}\n${"r 1\n".repeat(20000)}`;
  assert.ok(new TextEncoder().encode(body).byteLength > 64 * 1024);
  assert.equal(parseRelease(body), null);
});

test("releaseTag maps an unparsed release to the visible unknown bucket", () => {
  assert.equal(releaseTag(SHA), SHA);
  assert.equal(releaseTag(null), "unknown");
});

test("normalizeRelease keeps edge-supplied SHAs and buckets everything else", () => {
  assert.equal(normalizeRelease(SHA), SHA);
  assert.equal(normalizeRelease("9f3a1c7"), "9f3a1c7");
  assert.equal(normalizeRelease(undefined), "unknown");
  assert.equal(normalizeRelease(null), "unknown");
  assert.equal(normalizeRelease(""), "unknown");
  assert.equal(normalizeRelease("unknown"), "unknown");
  assert.equal(normalizeRelease(SHA.toUpperCase()), "unknown");
  assert.equal(normalizeRelease("postgres://user:hunter2@db.internal/app"), "unknown");
  assert.equal(normalizeRelease(7), "unknown");
});

test("workerVersionTag keeps platform version ids and drops anything else", () => {
  const version = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
  assert.equal(workerVersionTag(version), version);
  assert.equal(workerVersionTag(undefined), "unknown");
  assert.equal(workerVersionTag(null), "unknown");
  assert.equal(workerVersionTag(""), "unknown");
  assert.equal(workerVersionTag("https://evil.invalid/hunter2"), "unknown");
});
