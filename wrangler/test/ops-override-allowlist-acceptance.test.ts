/**
 * TOG-36209 acceptance (Slice 0a): reach/identity keys leave the PUT
 * /ops/config allowlist. Builder TOG-36216 must not edit this file.
 *
 * Contract (from the TOG-35922 spec, section 3.1 mismatch decision + B5):
 * `SETTING_CLASSES` marks the keys below EnvOnly, yet all nine are PUT-able
 * today because the allowlist is exactly `FORWARDED_FLAGS`. The builder
 * narrows the allowlist so every PUT naming one is refused (fail-closed,
 * like unknown keys). Forwarding itself MUST NOT change: these keys stay in
 * `FORWARDED_FLAGS` because the container's env-only reach bounds must still
 * arrive via env (`container-env.ts` header). Removing one from forwarding
 * to satisfy these pins is the wrong fix and breaks the stability pins.
 *
 * The Rust runtime mirrors these pins read-side in
 * `crates/bot/src/ops_overrides_acceptance_tests.rs`: even a pre-narrowing
 * DO record carrying a reach key must never surface at runtime.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-override-allowlist-acceptance.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { parseConfigPut } from "../src/ops-control.ts";
import { FORWARDED_FLAGS } from "../src/container-env.ts";

const ACTOR = "ops-allowlist-test";

function put(overrides: Record<string, string>) {
  return parseConfigPut({ overrides, expectedEpoch: 0, actor: ACTOR });
}

/** EnvOnly in settings.rs, forwarded, and PUT-able today: must become refused. */
const REACH_KEYS = [
  "TWO_MODERATION",
  "TWO_MODERATION_PROTECTED_ROLE_IDS",
  "TWO_OWEN_USER_ID",
  "TWO_ONBOARDING_MODE",
  "TWO_ANTI_NUKE_PROTECTED_USER_IDS",
  "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
  "TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS",
  "TWO_ASSISTANT_ENDPOINT",
  "TWO_ASSISTANT_MODEL",
] as const;

/** Already refused today (never forwarded): must stay refused. */
const INTERNAL_KEYS = ["TWO_INTERNAL_ALLOW_SETTINGS", "TWO_INTERNAL_FUTURE_GATE"] as const;

/** Behavioral flags that must stay PUT-able: guards against over-narrowing. */
const BEHAVIORAL_KEYS = [
  "TWO_AUTOMOD_MENTION_LIMIT",
  "TWO_RAID_JOIN_THRESHOLD",
  "TWO_FEED_POLL_SECONDS",
  "TWO_ONBOARDING_DRY_RUN",
] as const;

for (const key of REACH_KEYS) {
  test(`PUT /ops/config refuses ${key} (env-only reach key)`, () => {
    assert.equal(put({ [key]: "1" }), null);
  });
}

test("PUT /ops/config refuses a record mixing a behavioral flag with a reach key", () => {
  assert.equal(put({ TWO_AUTOMOD_MENTION_LIMIT: "9", TWO_MODERATION: "1" }), null);
});

for (const key of INTERNAL_KEYS) {
  test(`PUT /ops/config keeps refusing ${key}`, () => {
    assert.equal(put({ [key]: "1" }), null);
  });
}

test("PUT /ops/config keeps refusing secrets", () => {
  assert.equal(put({ TWO_MODERATION_AUDIT_SECRET: "synthetic" }), null);
});

for (const key of BEHAVIORAL_KEYS) {
  test(`PUT /ops/config still accepts ${key}`, () => {
    const change = put({ [key]: "9" });
    assert.notEqual(change, null);
    assert.deepEqual(change?.overrides, { [key]: "9" });
  });
}

for (const key of REACH_KEYS) {
  test(`${key} stays forwarded to the container (env-only reach bound)`, () => {
    assert.ok(
      (FORWARDED_FLAGS as readonly string[]).includes(key),
      `${key} must arrive via env; narrow the PUT allowlist, not forwarding`,
    );
  });
}
