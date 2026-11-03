/**
 * Slice 0a builder pins: the /ops record fetch URL and bearer reach the
 * container on explicit lines (never flag-forwarded), and the token travels
 * only while the URL is set.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-container-env.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { TwoBotContainer, type Env } from "../src/index.ts";
import { FORWARDED_FLAGS, NOT_FORWARDED } from "../src/container-env.ts";

/** Construct the real DO class far enough to read its startup envVars. */
function containerEnv(env: Partial<Env>): Record<string, string> {
  const database = new DatabaseSync(":memory:");
  try {
    const ctx = {
      container: { running: false },
      storage: {
        kv: { get: () => undefined },
        sql: { exec: (query: string) => database.prepare(query).all() },
      },
      blockConcurrencyWhile: () => Promise.resolve(),
    };
    const bot = new TwoBotContainer(ctx as unknown as DurableObjectState, env as Env);
    return bot.envVars as Record<string, string>;
  } finally {
    database.close();
  }
}

const OPS_URL = "https://ops.example.invalid/ops/config";
const TOKEN = "x".repeat(32);

test("ops fetch URL is forwarded verbatim when set", () => {
  const vars = containerEnv({ TWO_OPS_CONFIG_URL: OPS_URL });
  assert.equal(vars["TWO_OPS_CONFIG_URL"], OPS_URL);
});

test("ops fetch inputs stay absent when unset", () => {
  const vars = containerEnv({});
  assert.equal(vars["TWO_OPS_CONFIG_URL"], undefined);
  assert.equal(vars["TWO_OPS_CONFIG_TOKEN"], undefined);
});

test("ops fetch bearer travels only while the URL is set", () => {
  assert.equal(
    containerEnv({ TWO_OPS_CONFIG_URL: OPS_URL, OPS_CONTROL_TOKEN: TOKEN })["TWO_OPS_CONFIG_TOKEN"],
    TOKEN,
  );
  assert.equal(containerEnv({ OPS_CONTROL_TOKEN: TOKEN })["TWO_OPS_CONFIG_TOKEN"], undefined);
  assert.equal(containerEnv({ TWO_OPS_CONFIG_URL: OPS_URL })["TWO_OPS_CONFIG_TOKEN"], undefined);
});

test("ops fetch inputs are classified, never flag-forwarded", () => {
  for (const name of ["TWO_OPS_CONFIG_URL", "TWO_OPS_CONFIG_TOKEN"]) {
    assert.ok(Object.hasOwn(NOT_FORWARDED, name), `${name} classified`);
    assert.ok(!(FORWARDED_FLAGS as readonly string[]).includes(name), `${name} never a flag`);
  }
});

const RECEIVER = {
  TWO_INTERNAL_ACTIONS: "1",
  TWO_INTERNAL_CALLERS: "fixture:website",
  TWO_INTERNAL_CHANNEL_KEYS: "fixture:123456789012345678",
  TWO_INTERNAL_KEYS: `fixture:${"a".repeat(64)}`,
};

test("voice move gate is forwarded as exactly 1 while the receiver is forwarded", () => {
  const vars = containerEnv({ ...RECEIVER, TWO_INTERNAL_ALLOW_VOICE_MOVE: "1" });
  assert.equal(vars["TWO_INTERNAL_ALLOW_VOICE_MOVE"], "1");
});

test("voice move gate stays absent when unset", () => {
  assert.equal(containerEnv(RECEIVER)["TWO_INTERNAL_ALLOW_VOICE_MOVE"], undefined);
});

test("voice move gate stays absent for disabled or non-exact values", () => {
  for (const value of ["0", "", "true", "yes", " 1", "1 ", "01"]) {
    const vars = containerEnv({ ...RECEIVER, TWO_INTERNAL_ALLOW_VOICE_MOVE: value });
    assert.equal(vars["TWO_INTERNAL_ALLOW_VOICE_MOVE"], undefined, JSON.stringify(value));
  }
});

test("voice move gate never travels while the receiver is dark", () => {
  assert.equal(containerEnv({ TWO_INTERNAL_ALLOW_VOICE_MOVE: "1" })["TWO_INTERNAL_ALLOW_VOICE_MOVE"], undefined);
  const vars = containerEnv({ ...RECEIVER, TWO_INTERNAL_ACTIONS: "true", TWO_INTERNAL_ALLOW_VOICE_MOVE: "1" });
  assert.equal(vars["TWO_INTERNAL_ALLOW_VOICE_MOVE"], undefined);
});

test("voice move gate is classified, never flag-forwarded, and siblings stay unforwarded", () => {
  assert.ok(Object.hasOwn(NOT_FORWARDED, "TWO_INTERNAL_ALLOW_VOICE_MOVE"));
  assert.ok(!(FORWARDED_FLAGS as readonly string[]).includes("TWO_INTERNAL_ALLOW_VOICE_MOVE"));
  const vars = containerEnv({
    ...RECEIVER,
    TWO_INTERNAL_ALLOW_VOICE_MOVE: "1",
    TWO_INTERNAL_ALLOW_VOICE_SNAPSHOT: "1",
    TWO_INTERNAL_ALLOW_MODERATION: "1",
  } as Partial<Env>);
  assert.equal(vars["TWO_INTERNAL_ALLOW_VOICE_SNAPSHOT"], undefined);
  assert.equal(vars["TWO_INTERNAL_ALLOW_MODERATION"], undefined);
});
