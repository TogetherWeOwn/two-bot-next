import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const read = (file: string) => readFileSync(new URL(`../../${file}`, import.meta.url), "utf8");

function maxKeepaliveSeconds(toml: string): number {
  const values = [...toml.matchAll(/KEEPALIVE_SECONDS\s*=\s*"(\d+)"/g)].map((m) => Number(m[1]));
  assert.ok(values.length > 0, "wrangler.toml must define KEEPALIVE_SECONDS");
  return Math.max(...values);
}

function maxProbeTimeoutSeconds(indexTs: string): number {
  const values = [...indexTs.matchAll(/AbortSignal\.timeout\((\d+)\)/g)].map((m) => Number(m[1]) / 1000);
  assert.ok(values.length > 0, "index.ts must bound its keepalive probes");
  return Math.max(...values);
}

function lingerSeconds(shutdownRs: string, name: string): number {
  const match = new RegExp(`pub\\(crate\\) const ${name}: Duration = Duration::from_secs\\((\\d+)\\)`).exec(shutdownRs);
  assert.ok(match, `shutdown.rs must define ${name}`);
  return Number(match[1]);
}

test("runtime linger outlasts every keepalive tick plus both probe timeouts", () => {
  const keepalive = maxKeepaliveSeconds(read("wrangler/wrangler.toml"));
  const probe = maxProbeTimeoutSeconds(read("wrangler/src/index.ts"));
  const shutdownRs = read("crates/bot/src/shutdown.rs");
  const runtime = lingerSeconds(shutdownRs, "RUNTIME_FAILURE_LINGER");
  const startup = lingerSeconds(shutdownRs, "FAILURE_LINGER");
  assert.ok(
    runtime > keepalive + 2 * probe,
    `RUNTIME_FAILURE_LINGER (${runtime}s) must outlast KEEPALIVE_SECONDS (${keepalive}s) plus two probe timeouts (${probe}s each)`,
  );
  assert.ok(runtime < 900, "runtime linger must stay far below the container grace period");
  assert.ok(startup < runtime, "startup linger must stay shorter than the runtime linger");
});
