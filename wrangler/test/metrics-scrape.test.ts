import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ALERT_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines, RUST_ALERT_AXES, rustLabels } from "./fixtures/metrics.ts";

const valid = metricsBody();

test("alert input dimensions match the Rust always-emitted scrape contract", () => {
  assert.deepEqual(Object.keys(ALERT_INPUT_LABELS).sort(), Object.keys(RUST_ALERT_AXES).sort());
  for (const [name, axes] of Object.entries(RUST_ALERT_AXES)) {
    const expected = Object.fromEntries(Object.entries(axes).map(([key, dimension]) => [key, rustLabels(dimension)]));
    assert.deepEqual(ALERT_INPUT_LABELS[name], expected, name);
  }
  // New consumed families cannot silently bypass validation.
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  const consumed = [...rules.matchAll(/gauge\("([^"]+)"\)/g)].map((m) => m[1]!);
  assert.deepEqual([...new Set(consumed)].sort(), Object.keys(ALERT_INPUT_LABELS).sort());
  assert.ok(parseAlertScrape(valid));
});

test("every current label combination is required, not merely one row per family", () => {
  const lines = metricsLines();
  for (let i = 0; i < lines.length; i++) {
    assert.equal(parseAlertScrape(lines.filter((_, j) => j !== i).join("\n")), null, lines[i]);
  }
});

test("every consumed series rejects nonfinite, negative, fractional and nonnumeric inputs", () => {
  const lines = metricsLines();
  for (const name of Object.keys(ALERT_INPUT_LABELS)) {
    const index = lines.findIndex((line) => line.startsWith(`${name}{`) || line.startsWith(`${name} `));
    for (const value of ["NaN", "+Inf", "-Inf", "Infinity", "1e999", "-1", "0.5", "0x10", "garbage", "1e20"]) {
      const bad = [...lines];
      bad[index] = bad[index]!.replace(/ 0$/, ` ${value}`);
      assert.equal(parseAlertScrape(bad.join("\n")), null, `${name}: ${value}`);
    }
  }
  assert.equal(parseAlertScrape(valid.replace("two_bot_db_pool_configured 0", "two_bot_db_pool_configured 2")), null);
});

for (const [name, body] of Object.entries({
  empty: "", comments: "# HELP x y\n", html: "<html>synthetic-body-marker</html>", json: '{"ready":true}',
  partial: 'two_bot_job_consecutive_failures{job="rank"} 0\n',
  malformed: `${valid}not exposition\n`, trailing: `${valid}new_metric 1 extra\n`,
  badLabels: `${valid}new_metric{broken} 1\n`,
  duplicateLabels: `${valid}new_metric{job="rank",job="other"} 1\n`,
  badEscape: `${valid}new_metric{a="bad\\q"} 1\n`,
  duplicateSeries: `${valid}two_bot_db_pool_configured 0\n`,
  missingLabel: valid.replace('{job="rank"} 0', '{} 0'),
  extraLabel: valid.replace('{job="rank"} 0', '{job="rank",unexpected="x"} 0'),
  wrongLabel: valid.replace('{job="rank"} 0', '{task="rank"} 0'),
  inheritedFamily: `${valid}two_bot_internal_actions_total{family="__proto__",outcome="auth_failure"} 0\n`,
})) {
  test(`invalid exposition is rejected: ${name}`, () => assert.equal(parseAlertScrape(body), null));
}

test("additional series, new dimensions, reordered labels and unrelated NaN/Inf are accepted", () => {
  const text = valid.replace('{route="other",result="2xx"}', '{result="2xx",route="other"}')
    + 'new_histogram_bucket{le="+Inf"} 10\n'
    + 'future_metric{escaped="quote\\\" brace} slash\\\\ newline\\n"} +Inf\n'
    + 'two_bot_job_consecutive_failures{job="new_job"} 0\n'
    + 'two_bot_internal_actions_total{family="new_family",outcome="new_outcome"} 0\n';
  const samples = parseAlertScrape(text);
  assert.ok(samples);
  assert.ok(Number.isNaN(samples.find((s) => s.name === "two_bot_gateway_latency_seconds")?.value));
  assert.equal(samples.find((s) => s.name === "future_metric")?.value, Infinity);
  assert.equal(samples.find((s) => s.name === "future_metric")?.labels.escaped, 'quote" brace} slash\\ newline\n');
  assert.ok(parseAlertScrape(valid.replace("two_bot_gateway_missed_events_total 0", "two_bot_gateway_missed_events_total 18446744073709551615")));
});
