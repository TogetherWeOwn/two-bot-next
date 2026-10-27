// Builder-owned event-voice rollout and liveness regressions.
import { test } from "node:test";
import assert from "node:assert/strict";
import { EMPTY_STATE, evaluateMetrics, JOB_INTERVAL_SECONDS, parseExposition } from "../src/alert-rules.ts";
import { parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsLines } from "./fixtures/metrics.ts";

const isEventVoice = (line: string) => line.startsWith("two_bot_job_") && line.includes('job="event_voice"');

test("rollback containers without EventVoice still validate all older alert inputs", () => {
  const older = metricsLines().filter((line) => !isEventVoice(line));
  const parsed = parseAlertScrape(older.join("\n"));
  assert.ok(parsed);
  assert.ok(parsed.some((sample) => sample.labels.job === "rank"));
  assert.equal(parseAlertScrape(older.filter((line) => !line.startsWith('two_bot_job_consecutive_failures{job="rank"}')).join("\n")), null);
});

test("a present EventVoice requires every current required job series", () => {
  const current = metricsLines();
  const required = current.filter((line) => isEventVoice(line) && !line.startsWith("two_bot_job_last_error_class"));
  assert.equal(required.length, 4);
  assert.ok(parseAlertScrape(current.join("\n")));
  for (const missing of required) {
    assert.equal(parseAlertScrape(current.filter((line) => line !== missing).join("\n")), null, missing);
  }
  const older = current.filter((line) => !isEventVoice(line));
  assert.equal(parseAlertScrape([...older, 'two_bot_job_consecutive_failures{job="event_voice"} NaN'].join("\n")), null);
});

test("EventVoice staleness covers the cadence plus a full attempt timeout", () => {
  assert.equal(JOB_INTERVAL_SECONDS.event_voice, 60);
  const now = 1_000_000;
  const firing = (age: number) => evaluateMetrics(parseExposition(
    `two_bot_job_last_success_timestamp_seconds{job="event_voice"} ${now - age}\n`,
  ), EMPTY_STATE, now).firing;
  for (const age of [120, 150, 180]) assert.deepEqual(firing(age), [], `age ${age}`);
  assert.deepEqual(firing(181), ["job_stale:event_voice"]);
  assert.deepEqual(evaluateMetrics(parseExposition(
    'two_bot_job_last_success_timestamp_seconds{job="event_voice"} 0\n',
  ), EMPTY_STATE, now).firing, []);
});
