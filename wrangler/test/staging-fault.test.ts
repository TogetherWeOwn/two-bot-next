/**
 * Staging-only fault injector for the guarded auto-remediation soak.
 *
 * The Worker cannot stall itself, so the soak arms synthetic trigger input
 * through the generated staging deploy config only: held creator-map gauges
 * (generation 1, age past the two-interval bound) and/or an advancing
 * gateway-disconnect counter with no recovery. The overlay sits between the
 * scrape and `evaluateMetrics`, so the alert rules and the remediation guard
 * run unmodified. Dark by default: production and unarmed staging see the
 * scraped samples untouched.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CREATOR_MAP_STALE_SECONDS,
  EMPTY_STATE,
  evaluateMetrics,
  type MetricsAlertState,
  type Sample,
} from "../src/alert-rules.ts";
import {
  FAULT_CREATOR_MAP_AGE_SECONDS,
  FAULT_CREATOR_MAP_GENERATION,
  UNPAIRED_FAULT_VAR,
  VOICE_STALL_FAULT_VAR,
  applyStagingFaultInjection,
  stagingFaultArmed,
} from "../src/staging-fault.ts";

const NOW = 3_000_000;

function gauge(name: string, value: number): Sample {
  return { name, labels: {}, value };
}

function gatewaySamples(disconnects: number, resumes = 2, ready = 3): Sample[] {
  return [
    gauge("two_bot_gateway_disconnects_total", disconnects),
    gauge("two_bot_gateway_resumes_total", resumes),
    { name: "two_bot_gateway_events_total", labels: { event: "READY" }, value: ready },
  ];
}

const STAGING = { TWO_DEPLOY_ENV: "staging" };
const VOICE_ARMED = { ...STAGING, [VOICE_STALL_FAULT_VAR]: "1" };
const UNPAIRED_ARMED = { ...STAGING, [UNPAIRED_FAULT_VAR]: "1" };
const BOTH_ARMED = { ...STAGING, [VOICE_STALL_FAULT_VAR]: "1", [UNPAIRED_FAULT_VAR]: "1" };

test("dark by default: unarmed env returns the samples untouched", () => {
  const samples = [gauge("two_bot_voice_creator_map_generation", 0), ...gatewaySamples(5)];
  for (const env of [{}, STAGING, { TWO_DEPLOY_ENV: "production", [VOICE_STALL_FAULT_VAR]: "1" }]) {
    assert.equal(applyStagingFaultInjection(samples, EMPTY_STATE, env), samples);
  }
});

test("non-exact values never arm: only staging plus exact \"1\"", () => {
  const samples = [gauge("two_bot_voice_creator_map_generation", 0)];
  for (const env of [
    { ...STAGING, [VOICE_STALL_FAULT_VAR]: "true" },
    { ...STAGING, [VOICE_STALL_FAULT_VAR]: "0" },
    { ...STAGING, [VOICE_STALL_FAULT_VAR]: "" },
    { ...STAGING, [VOICE_STALL_FAULT_VAR]: " 1" },
    { TWO_DEPLOY_ENV: "staging ", [VOICE_STALL_FAULT_VAR]: "1" },
    { TWO_DEPLOY_ENV: "production", [VOICE_STALL_FAULT_VAR]: "1" },
  ]) {
    assert.deepEqual(stagingFaultArmed(env), { voiceStall: false, unpairedDisconnect: false }, JSON.stringify(env));
    assert.equal(applyStagingFaultInjection(samples, EMPTY_STATE, env), samples);
  }
  assert.deepEqual(stagingFaultArmed(VOICE_ARMED), { voiceStall: true, unpairedDisconnect: false });
  assert.deepEqual(stagingFaultArmed(UNPAIRED_ARMED), { voiceStall: false, unpairedDisconnect: true });
  assert.deepEqual(stagingFaultArmed(BOTH_ARMED), { voiceStall: true, unpairedDisconnect: true });
});

test("voice drill holds generation live with age past the staleness bound", () => {
  assert.equal(FAULT_CREATOR_MAP_GENERATION, 1);
  assert.ok(FAULT_CREATOR_MAP_AGE_SECONDS > CREATOR_MAP_STALE_SECONDS);
  const out = applyStagingFaultInjection([], EMPTY_STATE, VOICE_ARMED);
  assert.deepEqual(
    out,
    [
      gauge("two_bot_voice_creator_map_generation", FAULT_CREATOR_MAP_GENERATION),
      gauge("two_bot_voice_creator_map_age_seconds", FAULT_CREATOR_MAP_AGE_SECONDS),
    ],
  );
});

test("voice drill replaces a fresh map instead of duplicating the series", () => {
  const samples = [
    gauge("two_bot_voice_creator_map_generation", 7),
    gauge("two_bot_voice_creator_map_age_seconds", 12),
  ];
  const out = applyStagingFaultInjection(samples, EMPTY_STATE, VOICE_ARMED);
  assert.equal(out.length, 2);
  assert.deepEqual(out, [
    gauge("two_bot_voice_creator_map_generation", FAULT_CREATOR_MAP_GENERATION),
    gauge("two_bot_voice_creator_map_age_seconds", FAULT_CREATOR_MAP_AGE_SECONDS),
  ]);
  // The caller's scrape is never mutated.
  assert.deepEqual(samples, [
    gauge("two_bot_voice_creator_map_generation", 7),
    gauge("two_bot_voice_creator_map_age_seconds", 12),
  ]);
});

test("held voice gauges fire voice_creator_map_stale through the unmodified rules", () => {
  const faulted = applyStagingFaultInjection([], EMPTY_STATE, VOICE_ARMED);
  const { firing } = evaluateMetrics(faulted, EMPTY_STATE, NOW);
  assert.deepEqual(firing, ["voice_creator_map_stale"]);
});

test("unpaired drill opens an outstanding disconnect and holds it to the page bound", () => {
  let prev: MetricsAlertState = EMPTY_STATE;
  let firing: string[] = [];
  // One baseline tick plus three outstanding ticks: the rule pages on the
  // third consecutive outstanding sample.
  for (let tick = 0; tick < 4; tick++) {
    const faulted = applyStagingFaultInjection(gatewaySamples(5), prev, UNPAIRED_ARMED);
    ({ firing, state: prev } = evaluateMetrics(faulted, prev, NOW + tick * 60));
  }
  assert.ok(firing.includes("gateway_unpaired_disconnect"), `expected unpaired firing, got ${firing}`);
  // A flat container keeps firing while the drill stays armed: the injector
  // advances the counter past its own stored baseline each tick.
  const held = applyStagingFaultInjection(gatewaySamples(5), prev, UNPAIRED_ARMED);
  const next = evaluateMetrics(held, prev, NOW + 4 * 60);
  assert.ok(next.firing.includes("gateway_unpaired_disconnect"));
});

test("unpaired drill never invents a recovery: real growth still clears", () => {
  // Reach firing first.
  let prev: MetricsAlertState = EMPTY_STATE;
  for (let tick = 0; tick < 4; tick++) {
    const faulted = applyStagingFaultInjection(gatewaySamples(5), prev, UNPAIRED_ARMED);
    ({ state: prev } = evaluateMetrics(faulted, prev, NOW + tick * 60));
  }
  // A RESUMED landing on staging clears the outstanding loss even mid-drill.
  const recovered = applyStagingFaultInjection(gatewaySamples(5, 3, 3), prev, UNPAIRED_ARMED);
  const { firing } = evaluateMetrics(recovered, prev, NOW + 4 * 60);
  assert.ok(!firing.includes("gateway_unpaired_disconnect"));
});

test("disarming resolves: fresh gauges clear the stall on the next tick", () => {
  const faulted = applyStagingFaultInjection([], EMPTY_STATE, VOICE_ARMED);
  const firing = evaluateMetrics(faulted, EMPTY_STATE, NOW).state;
  assert.ok(firing.firing.includes("voice_creator_map_stale"));
  const fresh = [
    gauge("two_bot_voice_creator_map_generation", 4),
    gauge("two_bot_voice_creator_map_age_seconds", 9),
  ];
  const { firing: cleared } = evaluateMetrics(applyStagingFaultInjection(fresh, firing, {}), firing, NOW + 60);
  assert.ok(!cleared.includes("voice_creator_map_stale"));
});

test("fault lines stay public-safe", () => {
  const out = applyStagingFaultInjection(gatewaySamples(5), EMPTY_STATE, BOTH_ARMED);
  const text = JSON.stringify(out);
  assert.doesNotMatch(text, /TOG-|PAP-|bearer|token|secret|webhook/i);
  assert.ok(Object.keys({ [VOICE_STALL_FAULT_VAR]: 1, [UNPAIRED_FAULT_VAR]: 1 }).length === 2);
});
