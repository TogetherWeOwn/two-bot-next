import { test } from "node:test";
import assert from "node:assert/strict";
import { EMPTY_STATE, evaluateMetrics, parseExposition } from "../src/alert-rules.ts";
import { parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody } from "./fixtures/metrics.ts";

const HALT = "two_bot_audit_delivery_halt";
const KEY = "audit_delivery_halt";
const failure = (value: number) => `two_bot_job_consecutive_failures{job="audit_retry"} ${value}`;
const samples = (halt: number, failures: number) => parseExposition(`${HALT} ${halt}\n${failure(failures)}`);

test("a retained Rust halt gauge beside unreadable-sweep failures neither fires nor recovers", () => {
  let state = EMPTY_STATE;
  for (let i = 0; i < 2; i++) state = evaluateMetrics(samples(1, 0), state, i * 60).state;
  for (let i = 2; i < 6; i++) {
    // audit_runtime::sweep returns Database before updating the halt gauge;
    // the supervisor increments the consecutive-failure series instead.
    state = evaluateMetrics(samples(1, 1), state, i * 60).state;
    assert.deepEqual(state.firing, []);
    assert.equal(state.auditHaltStreak, 0);
  }
  for (let i = 6; i < 9; i++) state = evaluateMetrics(samples(1, 0), state, i * 60).state;
  assert.deepEqual(state.firing, [KEY]);
  for (const value of [1, 0]) {
    state = evaluateMetrics(samples(value, 1), state, 600).state;
    assert.deepEqual(state.firing, [KEY], "failed sweep cannot authorize a transition even beside a zero gauge");
  }
  assert.deepEqual(evaluateMetrics(samples(0, 0), state, 660).firing, []);
});

test("process boot's default zero gauge cannot resolve a persistent halt before a successful sweep", () => {
  let state = EMPTY_STATE;
  for (let i = 0; i < 3; i++) state = evaluateMetrics(samples(1, 0), state, i * 60).state;
  const boot = parseAlertScrape(metricsBody())!;
  assert.deepEqual(evaluateMetrics(boot, state, 180).firing, [KEY]);
  const healthy = parseAlertScrape(metricsBody().replace(
    'two_bot_job_last_success_timestamp_seconds{job="audit_retry"} 0',
    'two_bot_job_last_success_timestamp_seconds{job="audit_retry"} 240',
  ))!;
  assert.deepEqual(evaluateMetrics(healthy, state, 240).firing, []);
});

test("the complete scrape rejects non-boolean halt values and remains compatible without the optional gauge", () => {
  const base = metricsBody().split("\n").filter((line) => !line.startsWith(HALT)).join("\n");
  assert.ok(parseAlertScrape(base), "older exposition remains usable for other alert rules");
  for (const value of ["2", "-1", "0.5", "NaN", "+Inf"]) {
    assert.equal(parseAlertScrape(`${base}\n${HALT} ${value}`), null, value);
  }
  for (const value of [0, 1]) assert.ok(parseAlertScrape(`${base}\n${HALT} ${value}`));
});

test("a gap interrupts the pending run and an extra labeled duplicate cannot masquerade as a valid gauge", () => {
  let state = EMPTY_STATE;
  for (let i = 0; i < 2; i++) state = evaluateMetrics(samples(1, 0), state, i * 60).state;
  const mixed = parseExposition(`${HALT} 1\n${HALT}{reason="synthetic"} 1`);
  state = evaluateMetrics(mixed, state, 120).state;
  assert.equal(state.auditHaltStreak, 0);
  for (let i = 0; i < 2; i++) {
    state = evaluateMetrics(samples(1, 0), state, 180 + i * 60).state;
    assert.deepEqual(state.firing, []);
  }
  assert.deepEqual(evaluateMetrics(samples(1, 0), state, 300).firing, [KEY]);
});
