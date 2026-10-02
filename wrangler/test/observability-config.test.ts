/**
 * Per-environment Workers Logs regressions (TOG-12193). Parses wrangler.toml and
 * fixture copies with Wrangler's own config loader: no deploy, no API calls.
 */
import { after, test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { experimental_readRawConfig, unstable_readConfig } from "wrangler";

const CONFIG = fileURLToPath(new URL("../wrangler.toml", import.meta.url));
// staging: deploy-staging.yml; production: B4 (TOG-9699). Any other [env.*] is checked too.
const REQUIRED_ENVS = ["staging", "production"];

const MISSING = "missing (an inherited top-level block does not count)";

type Toggle = { enabled?: unknown; head_sampling_rate?: unknown; invocation_logs?: unknown };
type Observability = Toggle & { logs?: Toggle; traces?: Toggle };
type RawConfig = { env?: Record<string, { observability?: Observability }> };

function observabilityErrors(raw: RawConfig): string[] {
  const errors: string[] = [];
  const envs = raw.env ?? {};
  for (const name of new Set([...REQUIRED_ENVS, ...Object.keys(envs)])) {
    const prefix = `[env.${name}.observability]`;
    const o = envs[name]?.observability;
    // Observability is inheritable, so a top-level block would hide this gap.
    if (!o) {
      errors.push(`${prefix}: ${MISSING}`);
      continue;
    }
    if (o.enabled !== true) errors.push(`${prefix}: enabled must be true`);
    const rate = o.head_sampling_rate;
    if (typeof rate !== "number" || !(rate > 0 && rate <= 1)) {
      errors.push(`${prefix}: head_sampling_rate must be a number in (0, 1]`);
    }
    // Head sampling drops whole invocations, error and rejection logs included.
    if ((o.logs?.head_sampling_rate ?? rate) !== 1) {
      errors.push(`${prefix}: logs must not be head-sampled (rate 1)`);
    }
    if (o.logs?.enabled === false || o.logs?.invocation_logs === false) {
      errors.push(`${prefix}: logs and invocation logs must stay on`);
    }
    // Fetch spans record url.full: the OPS_ALERT_WEBHOOK_URL token.
    if (o.traces?.enabled === true) errors.push(`${prefix}: traces must stay off`);
  }
  return errors;
}

const fixtureDir = mkdtempSync(join(tmpdir(), "two-bot-observability-"));
after(() => rmSync(fixtureDir, { recursive: true, force: true }));
let fixtureCount = 0;

function parseFixture(toml: string): RawConfig {
  const path = join(fixtureDir, `wrangler-${fixtureCount++}.toml`);
  writeFileSync(path, toml);
  return experimental_readRawConfig({ config: path }).rawConfig as RawConfig;
}

test("every named env declares unsampled Workers Logs", () => {
  const raw = experimental_readRawConfig({ config: CONFIG }).rawConfig as RawConfig;
  assert.deepEqual(observabilityErrors(raw), []);
  // What wrangler deploys per env is exactly the declared block.
  for (const env of REQUIRED_ENVS) {
    assert.deepEqual(unstable_readConfig({ config: CONFIG, env }).observability, raw.env?.[env]?.observability, env);
  }
});

test("removing or disabling production observability in wrangler.toml fails", () => {
  const real = readFileSync(CONFIG, "utf8");
  const block = /^\[env\.production\.observability\]\n(?:[^[\n].*\n|\n)*/m;
  assert.match(real, block);
  const mutations = {
    removed: real.replace(/^\[env\.production\.observability(?:\.\w+)?\]\n(?:[^[\n].*\n|\n)*/gm, ""),
    removedMainBlock: real.replace(block, ""),
    disabled: real.replace(block, (b) => b.replace(/^enabled = true$/m, "enabled = false")),
    sampled: real.replace(block, (b) => b.replace(/^head_sampling_rate = 1$/m, "head_sampling_rate = 0.5")),
  };
  for (const [name, toml] of Object.entries(mutations)) {
    assert.notEqual(toml, real, name);
    const errors = observabilityErrors(parseFixture(toml));
    assert.ok(errors.length > 0, name);
    assert.ok(errors.every((e) => e.startsWith("[env.production.observability]")), `${name}: ${errors}`);
  }
});

test("fixture TOML strings reject each unsafe per-env shape", () => {
  const staging = "[env.staging.observability]\nenabled = true\nhead_sampling_rate = 1\n";
  const production = (body: string) => `name = "fixture"\n${staging}[env.production.vars]\nBOT_PORT = "8080"\n${body}`;
  assert.deepEqual(observabilityErrors(parseFixture(production("[env.production.observability]\nenabled = true\nhead_sampling_rate = 1\n"))), []);
  for (const [body, error] of [
    ["", MISSING],
    ["[observability]\nenabled = true\nhead_sampling_rate = 1\n", MISSING],
    ["[env.production.observability]\nhead_sampling_rate = 1\n", "enabled must be true"],
    ["[env.production.observability]\nenabled = false\nhead_sampling_rate = 1\n", "enabled must be true"],
    ['[env.production.observability]\nenabled = "true"\nhead_sampling_rate = 1\n', "enabled must be true"],
    ["[env.production.observability]\nenabled = true\n", "head_sampling_rate must be a number in (0, 1]"],
    ['[env.production.observability]\nenabled = true\nhead_sampling_rate = "1"\n', "head_sampling_rate must be a number in (0, 1]"],
    ["[env.production.observability]\nenabled = true\nhead_sampling_rate = 0\n", "head_sampling_rate must be a number in (0, 1]"],
    ["[env.production.observability]\nenabled = true\nhead_sampling_rate = 1.5\n", "head_sampling_rate must be a number in (0, 1]"],
    ["[env.production.observability]\nenabled = true\nhead_sampling_rate = 0.1\n", "logs must not be head-sampled (rate 1)"],
    ["[env.production.observability]\nenabled = true\nhead_sampling_rate = 1\nlogs = { head_sampling_rate = 0.1 }\n", "logs must not be head-sampled (rate 1)"],
    ["[env.production.observability]\nenabled = true\nhead_sampling_rate = 1\nlogs = { invocation_logs = false }\n", "logs and invocation logs must stay on"],
    ["[env.production.observability]\nenabled = true\nhead_sampling_rate = 1\ntraces = { enabled = true }\n", "traces must stay off"],
  ]) {
    assert.ok(
      observabilityErrors(parseFixture(production(body!))).includes(`[env.production.observability]: ${error}`),
      JSON.stringify(body),
    );
  }
  // A future named env is held to the same rule without editing REQUIRED_ENVS.
  const canary = parseFixture(production("[env.production.observability]\nenabled = true\nhead_sampling_rate = 1\n[env.canary.vars]\nBOT_PORT = \"8080\"\n"));
  assert.deepEqual(observabilityErrors(canary), [`[env.canary.observability]: ${MISSING}`]);
});
