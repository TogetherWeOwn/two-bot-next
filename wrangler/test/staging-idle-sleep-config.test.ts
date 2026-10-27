/**
 * Staging idle sleep config: only [env.staging.vars] turns the keepalive off.
 * Parses wrangler.toml with Wrangler's own config loader (no deploy, no API
 * calls) and feeds each environment's resolved vars to the same helpers the
 * Durable Object uses, so production provably keeps the always-on keepalive
 * and the 30m sleepAfter while staging sleeps after its idle window.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { experimental_readRawConfig, unstable_readConfig } from "wrangler";
import {
  DEFAULT_IDLE_SLEEP_AFTER,
  KEEPALIVE_SLEEP_AFTER,
  keepaliveEnabled,
  sleepAfterFor,
  type Env,
} from "../src/index.ts";

const CONFIG = fileURLToPath(new URL("../wrangler.toml", import.meta.url));
const IDLE_VARS = ["KEEPALIVE_ENABLED", "IDLE_SLEEP_AFTER"];

type RawConfig = {
  vars?: Record<string, unknown>;
  env?: Record<string, { vars?: Record<string, unknown> }>;
};

function deployedVars(env?: string): Partial<Env> {
  return unstable_readConfig({ config: CONFIG, env }).vars as Partial<Env>;
}

test("production and the top level carry no idle-sleep var: keepalive on, sleepAfter 30m", () => {
  const raw = experimental_readRawConfig({ config: CONFIG }).rawConfig as RawConfig;
  for (const [name, vars] of [["top-level", raw.vars], ["production", raw.env?.production?.vars]] as const) {
    assert.ok(vars, name);
    for (const key of IDLE_VARS) assert.equal(key in vars, false, `${name} must not set ${key}`);
  }
  for (const env of [undefined, "production"]) {
    const vars = deployedVars(env);
    assert.equal(keepaliveEnabled(vars), true, env ?? "top-level");
    assert.equal(sleepAfterFor(vars), "30m", env ?? "top-level");
    assert.equal(vars.KEEPALIVE_SECONDS, "60", env ?? "top-level");
  }
  assert.equal(KEEPALIVE_SLEEP_AFTER, "30m", "the always-on belt is unchanged");
});

test("staging turns the keepalive off and sleeps after 15 idle minutes", () => {
  const vars = deployedVars("staging");
  assert.equal(vars.KEEPALIVE_ENABLED, "0");
  assert.equal(vars.IDLE_SLEEP_AFTER, "15m");
  assert.equal(keepaliveEnabled(vars), false);
  assert.equal(sleepAfterFor(vars), "15m");
  assert.equal(DEFAULT_IDLE_SLEEP_AFTER, "15m");
});

test("every other named environment keeps the keepalive on", () => {
  const raw = experimental_readRawConfig({ config: CONFIG }).rawConfig as RawConfig;
  for (const name of Object.keys(raw.env ?? {})) {
    if (name === "staging") continue;
    assert.equal(keepaliveEnabled(deployedVars(name)), true, name);
    assert.equal(sleepAfterFor(deployedVars(name)), "30m", name);
  }
});
