import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import worker, { type Env } from "../src/index.ts";
import {
  EMPTY_STATE, JOB_ERROR_CLASSES, JOB_RUNS_JOBS, PROMOTION_ERROR_GATE_RULES, REST_PENDING_STUCK_SAMPLES, REST_REFUSAL_SURGE_MIN, RULES, RUNBOOK_BASE_URL, evaluateMetrics, evaluatePromotionGate, interruptDispatchDrops, packetFilename, parseExposition, ruleFor, runbookUrl, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, rustLabels } from "./fixtures/metrics.ts";

const NOW = 1_000_000;
function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}
const ev = (lines: string[], prev: MetricsAlertState = EMPTY_STATE, gatewayReady?: boolean) =>
  evaluateMetrics(parseExposition(body(lines)), prev, NOW, gatewayReady);

/** GitHub-style heading slugs for a doc's text; literal underscores survive. */
function headingAnchors(text: string): Set<string> {
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")));
}

/** Heading-slug index for one checked-in doc (single definition reused below). */
function docAnchors(doc: string): Set<string> {
  return headingAnchors(readFileSync(new URL(`../../docs/${doc}`, import.meta.url), "utf8"));
}

test("job stale fires only past two intervals and ignores never-succeeded", () => {
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 1200}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 1201}`]).firing, ["job_stale:rank"]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} 0`]).firing, []);
});

test("creator-map staleness fires past two refresh intervals once a merge landed", () => {
  const map = (generation: number, age: number) => [
    `two_bot_voice_creator_map_generation ${generation}`,
    `two_bot_voice_creator_map_age_seconds ${age}`,
  ];
  // At the bound is not past it; one failed refresh alone cannot fire.
  assert.deepEqual(ev(map(3, 600)).firing, []);
  const stale = ev(map(3, 601));
  assert.deepEqual(stale.firing, ["voice_creator_map_stale"]);
  // Generation 0 is never-merged (boot or voice parked): the age is defined
  // as 0 there, so even a synthetic large age must not fire.
  assert.deepEqual(ev(map(0, 99999)).firing, []);
  // A landed merge resets the age and resolves the alert.
  assert.deepEqual(ev(map(4, 0), stale.state).firing, []);
  // Older containers lack the optional gauges: absence holds a firing alert
  // instead of resolving it, and never fires on its own.
  assert.deepEqual(ev([], stale.state).firing, ["voice_creator_map_stale"]);
  assert.deepEqual(ev([]).firing, []);
  // Duplicated or labeled rows are malformed for a label-free gauge: held,
  // not resolved and not newly fired.
  assert.deepEqual(ev([...map(3, 601), "two_bot_voice_creator_map_age_seconds 601"], stale.state).firing, ["voice_creator_map_stale"]);
  assert.deepEqual(ev([`two_bot_voice_creator_map_generation 3`, `two_bot_voice_creator_map_age_seconds{guild="1"} 601`]).firing, []);
});

test("consecutive failures fire at 3 and retain classless messages for older exposition", () => {
  assert.deepEqual(ev([`two_bot_job_consecutive_failures{job="counter"} 2`]).firing, []);
  const failed = ev([`two_bot_job_consecutive_failures{job="counter"} 3`]);
  assert.deepEqual(failed.firing, ["job_consecutive_failures:counter"]);
  assert.doesNotMatch(transitionMessages([], failed.firing)[0]!, /last error class/);
});

test("job failure alerts include only a fixed last error class and retain it through resolution", () => {
  for (const errorClass of JOB_ERROR_CLASSES) {
    const failing = ev([
      `two_bot_job_consecutive_failures{job="counter"} 3`,
      ...JOB_ERROR_CLASSES.map((candidate) =>
        `two_bot_job_last_error_class{job="counter",class="${candidate}"} ${candidate === errorClass ? 1 : 0}`),
    ]);
    assert.deepEqual(failing.firing, ["job_consecutive_failures:counter"]);
    assert.equal(failing.state.jobErrorClasses?.["counter"], errorClass);
    const fired = transitionMessages([], failing.firing, {}, failing.state.jobErrorClasses)[0]!;
    assert.ok(fired.includes(`job_consecutive_failures:counter (last error class: ${errorClass})`));

    const resolved = ev([
      `two_bot_job_consecutive_failures{job="counter"} 0`,
      ...JOB_ERROR_CLASSES.map((candidate) =>
        `two_bot_job_last_error_class{job="counter",class="${candidate}"} 0`),
    ], failing.state);
    assert.deepEqual(resolved.firing, []);
    const resolution = transitionMessages(
      failing.firing,
      resolved.firing,
      failing.state.jobErrorClasses,
      resolved.state.jobErrorClasses,
    )[0]!;
    assert.ok(resolution.includes(`job_consecutive_failures:counter (last error class: ${errorClass})`));
  }
});

test("unrecognized or ambiguous job error classes never enter alert messages", () => {
  const failed = ev([
    `two_bot_job_consecutive_failures{job="counter"} 3`,
    `two_bot_job_last_error_class{job="counter",class="database"} 1`,
    `two_bot_job_last_error_class{job="counter",class="https://example.invalid/member/123?body=synthetic"} 1`,
  ]);
  assert.equal(failed.state.jobErrorClasses?.["counter"], undefined);
  const message = transitionMessages([], failed.firing, {}, failed.state.jobErrorClasses)[0]!;
  assert.doesNotMatch(message, /example\.invalid|member\/123|body=synthetic/);
});

test("feed and self-role failures alert and resolve independently without a fixed stale policy", () => {
  const jobs = ["feeds", "self_role_recovery"];
  // Fresh timestamps (within the 2-hour job_unknown_stale window) so the
  // coarse fallback stays silent and this test proves consecutive-failure
  // independence alone; staleness coverage is pinned by the catalog guard.
  const failing = ev(jobs.flatMap((job) => [
    `two_bot_job_consecutive_failures{job="${job}"} 3`,
    `two_bot_job_last_success_timestamp_seconds{job="${job}"} ${NOW - 60}`,
  ]));
  assert.deepEqual(failing.firing, jobs.map((job) => `job_consecutive_failures:${job}`));
  for (const recovered of jobs) {
    const remaining = jobs.find((job) => job !== recovered)!;
    const result = ev(jobs.map((job) =>
      `two_bot_job_consecutive_failures{job="${job}"} ${job === recovered ? 0 : 3}`), failing.state);
    assert.deepEqual(result.firing, [`job_consecutive_failures:${remaining}`]);
    const messages = transitionMessages(failing.firing, result.firing);
    assert.equal(messages.length, 1);
    assert.match(messages[0]!, /RESOLVED/);
    assert.ok(messages[0]!.includes(`job_consecutive_failures:${recovered}`));
  }
});

test("unban sweep and automod expiry failures alert and resolve independently without a fixed stale policy", () => {
  const jobs = ["member_unban_sweep", "automod_expiry"];
  // Fresh timestamps (within the 2-hour job_unknown_stale window) so the
  // coarse fallback stays silent and this test proves consecutive-failure
  // independence alone; staleness coverage is pinned by the catalog guard.
  // In particular, an automod success must not clear a failing unban streak.
  const failing = ev(jobs.flatMap((job) => [
    `two_bot_job_consecutive_failures{job="${job}"} 3`,
    `two_bot_job_last_success_timestamp_seconds{job="${job}"} ${NOW - 60}`,
  ]));
  assert.deepEqual(failing.firing, jobs.map((job) => `job_consecutive_failures:${job}`));
  for (const recovered of jobs) {
    const remaining = jobs.find((job) => job !== recovered)!;
    const result = ev(jobs.map((job) =>
      `two_bot_job_consecutive_failures{job="${job}"} ${job === recovered ? 0 : 3}`), failing.state);
    assert.deepEqual(result.firing, [`job_consecutive_failures:${remaining}`]);
    const messages = transitionMessages(failing.firing, result.firing);
    assert.equal(messages.length, 1);
    assert.match(messages[0]!, /RESOLVED/);
    assert.ok(messages[0]!.includes(`job_consecutive_failures:${recovered}`));
  }
});

test("429 rate uses the delta, ignores resets and tiny windows", () => {
  const first = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="429"} 0`]);
  const hot = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="429"} 20`], first.state);
  assert.deepEqual(hot.firing, ["rest_429_rate"]);
  const small = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="429"} 5`], first.state);
  assert.deepEqual(small.firing, []);
  const restart = ev([`two_bot_rest_requests_total{route="other",result="429"} 1`], hot.state);
  assert.deepEqual(restart.firing, []);
});

test("5xx rate uses the delta, baselines legacy state, and ignores resets and tiny windows", () => {
  const first = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="5xx"} 0`]);
  const hot = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="5xx"} 20`], first.state);
  assert.deepEqual(hot.firing, ["rest_5xx_rate"]);
  const small = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="5xx"} 5`], first.state);
  assert.deepEqual(small.firing, []);
  const restart = ev([`two_bot_rest_requests_total{route="other",result="5xx"} 1`], hot.state);
  assert.deepEqual(restart.firing, []);

  const legacy = { ...first.state, restTotal: 1000 };
  delete legacy.rest5xx;
  const baseline = ev([
    `two_bot_rest_requests_total{route="other",result="2xx"} 1010`,
    `two_bot_rest_requests_total{route="other",result="5xx"} 50`,
  ], legacy);
  assert.deepEqual(baseline.firing, []);
  assert.equal(baseline.state.rest5xx, 50);
  const later = ev([
    `two_bot_rest_requests_total{route="other",result="2xx"} 1018`,
    `two_bot_rest_requests_total{route="other",result="5xx"} 62`,
  ], baseline.state);
  assert.deepEqual(later.firing, ["rest_5xx_rate"]);

  // A 429 burst is not a 5xx burst: the two rates track independently.
  const other = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="429"} 20`], first.state);
  assert.deepEqual(other.firing, ["rest_429_rate"]);
});

test("alert messages carry build revision, failure detail and the log-query link", () => {
  const firing = transitionMessages([], ["rest_5xx_rate"], {}, {}, {
    buildRevision: "abc123",
    failedStep: "gateway_run",
    errorCode: "42501",
  })[0]!;
  assert.match(firing, /Build: abc123/);
  assert.match(firing, /Failure: gateway_run\/42501/);
  assert.match(firing, /Logs: https:\/\/developers\.cloudflare\.com/);
  const fallback = transitionMessages([], ["rest_5xx_rate"])[0]!;
  assert.match(fallback, /Build: unknown/);
  assert.doesNotMatch(fallback, /Failure:/);
});

test("promotion error gate fails a stage only on a 5xx burst", () => {
  assert.deepEqual(PROMOTION_ERROR_GATE_RULES, ["rest_5xx_rate"]);
  // Injected 5xx spike through the real evaluator fails the gate.
  const first = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="5xx"} 0`]);
  const hot = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="5xx"} 20`], first.state);
  assert.deepEqual(hot.firing, ["rest_5xx_rate"]);
  assert.deepEqual(evaluatePromotionGate(hot.firing), { fail: true, reasons: ["rest_5xx_rate"] });
  // 429 backpressure passes: it is not a rollback trigger.
  assert.deepEqual(evaluatePromotionGate(["rest_429_rate"]), { fail: false, reasons: [] });
  // Quiet windows and unrelated subjects pass.
  assert.deepEqual(evaluatePromotionGate([]), { fail: false, reasons: [] });
  assert.deepEqual(evaluatePromotionGate(["job_stale:rank"]), { fail: false, reasons: [] });
});

test("db pool saturation needs three consecutive samples", () => {
  const lines = ["two_bot_db_pool_configured 1", "two_bot_db_pool_connections 5", "two_bot_db_pool_max_connections 5", "two_bot_db_pool_idle_connections 0"];
  let state = EMPTY_STATE;
  for (const expected of [[], [], ["db_pool_saturated"]]) {
    const r = ev(lines, state);
    assert.deepEqual(r.firing, expected);
    state = r.state;
  }
  assert.equal(ev(["two_bot_db_pool_configured 1", "two_bot_db_pool_connections 5", "two_bot_db_pool_max_connections 5", "two_bot_db_pool_idle_connections 1"], state).state.poolStreak, 0);
});

test("transitions notify once on fire and once on resolve", () => {
  assert.equal(transitionMessages([], ["job_stale:rank"]).length, 1);
  assert.equal(transitionMessages(["job_stale:rank"], ["job_stale:rank"]).length, 0);
  assert.match(transitionMessages(["job_stale:rank"], [])[0]!, /RESOLVED/);
});

test("db errors fire on a burst, ignore trickles and restarts", () => {
  const base = ev([`two_bot_db_errors_total{op="admission"} 0`, `two_bot_db_errors_total{op="other"} 0`]);
  assert.deepEqual(base.firing, []);
  const trickle = ev([`two_bot_db_errors_total{op="admission"} 2`, `two_bot_db_errors_total{op="other"} 0`], base.state);
  assert.deepEqual(trickle.firing, []);
  const burst = ev([`two_bot_db_errors_total{op="admission"} 5`, `two_bot_db_errors_total{op="other"} 0`], base.state);
  assert.deepEqual(burst.firing, ["db_errors"]);
  const restart = ev([`two_bot_db_errors_total{op="admission"} 1`], burst.state);
  assert.deepEqual(restart.firing, []);
});

test("send admission blocked needs three consecutive windows with new refusals", () => {
  const lines = (n: number) => [`two_bot_send_admissions_total{outcome="blocked"} ${n}`];
  let state = EMPTY_STATE;
  for (const expected of [[], [], ["send_admission_blocked"]]) {
    const r = ev(lines(state.sendBlocked + 1), state);
    assert.deepEqual(r.firing, expected);
    state = r.state;
  }
  // A quiet window clears the streak; a counter reset (restart) resets it.
  assert.equal(ev(lines(state.sendBlocked), state).state.sendBlockedStreak, 0);
  assert.equal(ev([`two_bot_send_admissions_total{outcome="blocked"} 0`], state).state.sendBlockedStreak, 0);
});

test("voice failures fire on the lifecycle-drill failure exposition, silent on healthy", () => {
  // Synthetic exposition mirroring docs/voice-lifecycle-alert-drill.md Q4:
  // 42 successful creates, then injected create/discord x7,
  // move/persistence x4 and dead-letter create x3.
  const failing = [
    `two_bot_voice_operations_total{op="create",outcome="success"} 42`,
    `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
    `two_bot_voice_operations_total{op="move",outcome="persistence"} 4`,
    `two_bot_voice_dead_letters_total{action="create"} 3`,
  ];
  assert.deepEqual(ev(failing).firing, ["voice_failures"]);
  const healthy = [`two_bot_voice_operations_total{op="create",outcome="success"} 42`];
  assert.deepEqual(ev(healthy).firing, []);
});

test("admission refusals never page voice failures or dilute a real failure ratio", () => {
  const base = ev([`two_bot_voice_operations_total{op="create",outcome="success"} 3`]);
  // One member hopping between their room and the creator: ten creates succeed
  // while ten admission refusals land in the same window.
  const hopping = ev([
    `two_bot_voice_operations_total{op="create",outcome="success"} 13`,
    `two_bot_voice_operations_total{op="create",outcome="refused"} 10`,
  ], base.state);
  assert.deepEqual(hopping.firing, []);
  // One failure in ten admitted operations is 10%: the same refusals must not
  // dilute it below the 5% line.
  const diluted = ev([
    `two_bot_voice_operations_total{op="create",outcome="success"} 12`,
    `two_bot_voice_operations_total{op="create",outcome="discord"} 1`,
    `two_bot_voice_operations_total{op="create",outcome="refused"} 10`,
  ], base.state);
  assert.deepEqual(diluted.firing, ["voice_failures"]);
});

test("voice failures ignore trickles and restarts, fire on lone dead-letters/orphans", () => {
  const base = ev([`two_bot_voice_operations_total{op="create",outcome="success"} 42`]);
  // One failure in 43 ops (2.3%) is a trickle, not a storm.
  const trickle = ev(
    [`two_bot_voice_operations_total{op="create",outcome="success"} 42`, `two_bot_voice_operations_total{op="create",outcome="discord"} 1`],
    base.state,
  );
  assert.deepEqual(trickle.firing, []);
  // A counter that went backwards means the process restarted: no window.
  const restart = ev([`two_bot_voice_operations_total{op="create",outcome="success"} 1`], ev([
    `two_bot_voice_operations_total{op="create",outcome="success"} 42`,
    `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
    `two_bot_voice_operations_total{op="move",outcome="persistence"} 4`,
  ]).state);
  assert.deepEqual(restart.firing, []);
  // A single new dead-letter or orphan pages even below the ratio window.
  const quiet = ev([]);
  assert.deepEqual(ev([`two_bot_voice_dead_letters_total{action="create"} 1`], quiet.state).firing, ["voice_failures"]);
  assert.deepEqual(ev([`two_bot_voice_orphans_total 1`], quiet.state).firing, ["voice_failures"]);
});

/** Creator-join SLI lines: received joins, benign duplicate refusals, moves within 5 s. */
function joinLines(received: number, duplicate: number, fast: number): string[] {
  return [
    `two_bot_voice_joins_received_total ${received}`,
    `two_bot_voice_join_refused_total{reason="duplicate"} ${duplicate}`,
    `two_bot_voice_join_to_move_seconds_bucket{le="5"} ${fast}`,
  ];
}
/** Burn windows need real time gaps, so these tests drive evaluateMetrics directly. */
const evAt = (lines: string[], prev: MetricsAlertState, now: number) =>
  evaluateMetrics(parseExposition(body(lines)), prev, now);

test("voice join fast burn pages on a 1 h spike while the diluted 6 h window stays silent", () => {
  // A full-history baseline keeps the long windows from treating startup as a window.
  let state = evAt(joinLines(0, 0, 0), EMPTY_STATE, -21600).state;
  state = evAt(joinLines(0, 0, 0), state, -300).state;
  state = evAt(joinLines(0, 0, 0), state, 0).state;
  // Five healthy hours: 9,900 genuine joins, 99 slow (2x burn, silent).
  state = evAt(joinLines(10000, 100, 9801), state, 17700).state;
  state = evAt(joinLines(10000, 100, 9801), state, 18000).state;
  assert.deepEqual(evAt(joinLines(10000, 100, 9801), state, 18000).firing, []);
  // The last hour burns at ~40x: 990 genuine, 198 never moved within 5 s.
  state = evAt(joinLines(10800, 108, 10434), state, 21000).state;
  state = evAt(joinLines(10900, 109, 10513), state, 21300).state;
  const spiking = evAt(joinLines(11000, 110, 10593), state, 21600);
  assert.deepEqual(spiking.firing, ["voice_join_fast_burn"]);
  assert.equal(ruleFor("voice_join_fast_burn")?.severity, undefined, "fast burn pages");
  // The delayed receive window dilutes the same spike below the 6x slow line.
  assert.ok(!spiking.firing.includes("voice_join_slow_burn"));
  // Recovery: a healthy hour stops the page.
  const recovered = evAt(joinLines(21000, 210, 20493), spiking.state, 25200);
  assert.deepEqual(recovered.firing, []);
});

test("voice join slow burn tickets on a sustained 6 h grind that never trips fast burn", () => {
  // Steady 8x burn, one minute at a time: 100 genuine joins and 4 slow
  // moves per tick. The cadence keeps genuine traffic in the short windows.
  let state = EMPTY_STATE;
  for (let i = 0; i <= 360; i++) {
    state = evAt(joinLines(i * 100, 0, i * 96), state, i * 60).state;
  }
  const grinding = evAt(joinLines(361 * 100, 0, 361 * 96), state, 361 * 60);
  // 361 ticks: 36,100 genuine, 34,656 fast, 1,444 slow = 8x burn.
  assert.deepEqual(grinding.firing, ["voice_join_slow_burn"]);
  assert.equal(grinding.joinSlowCovered, true);
  assert.equal(ruleFor("voice_join_slow_burn")?.severity, "ticket");
  // The same 8x never reaches the 14.4x fast line in any 1 h / 5 min window.
  assert.ok(!grinding.firing.includes("voice_join_fast_burn"));
});

test("voice join burn holds the previous firing state when the join series go missing", () => {
  // A firing ticket survives an exposition without the join series: the
  // tracker reads joinHeld to decide nothing instead of resolving the card.
  let state = EMPTY_STATE;
  for (let i = 0; i <= 360; i++) {
    state = evAt(joinLines(i * 100, 0, i * 96), state, i * 60).state;
  }
  const firing = evAt(joinLines(361 * 100, 0, 361 * 96), state, 361 * 60);
  assert.deepEqual(firing.firing, ["voice_join_slow_burn"]);
  assert.equal(firing.joinHeld, false);
  assert.equal(firing.joinSlowCovered, true);
  const held = evAt([], firing.state, 362 * 60);
  assert.deepEqual(held.firing, ["voice_join_slow_burn"]);
  assert.equal(held.joinHeld, true);
  assert.equal(held.joinSlowCovered, false);
});

test("voice join burn reports slow-window coverage separately from the firing verdict", () => {
  // A fresh ring is silent but uncovered: the tracker must read coverage,
  // not silence, before it may resolve the card.
  const first = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0);
  assert.deepEqual(first.firing, []);
  assert.equal(first.joinSlowCovered, false);
  const second = evAt(joinLines(12, 0, 10), first.state, 60);
  assert.deepEqual(second.firing, []);
  assert.equal(second.joinSlowCovered, false);
  // Full quiet history is covered and stays silent: covered-but-quiet may
  // still resolve, uncovered may not.
  let state = EMPTY_STATE;
  for (let now = -21600; now <= 0; now += 60) {
    state = evAt(joinLines(0, 0, 0), state, now).state;
  }
  const quiet = evAt(joinLines(0, 0, 0), state, 60);
  assert.deepEqual(quiet.firing, []);
  assert.equal(quiet.joinSlowCovered, true);
});

test("voice join burn stays silent on healthy traffic, quiet windows and benign duplicates", () => {
  // Healthy at a fifth of the budget: 1,000 genuine, 1 slow (0.2x burn).
  let state = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0).state;
  state = evAt(joinLines(1010, 10, 999), state, 3600).state;
  assert.deepEqual(evAt(joinLines(2010, 20, 1998), state, 7200).firing, []);
  // Quiet guilds never fire: no joins, no windows.
  const quiet = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0);
  assert.deepEqual(evAt(joinLines(0, 0, 0), quiet.state, 3600).firing, []);
  // Benign post-move refusals are not genuine joins: 50 duplicate refusals
  // against zero moves is working as designed, not burn.
  const dupes = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0);
  assert.deepEqual(evAt(joinLines(50, 50, 0), dupes.state, 3600).firing, []);
  // Below the volume floor even total failure stays silent: 5 bad joins in
  // both long and short windows cannot page a guild that barely uses voice.
  let thin = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0);
  thin = evAt(joinLines(0, 0, 0), thin.state, 3300);
  assert.deepEqual(evAt(joinLines(5, 0, 0), thin.state, 3600).firing, []);
  // At the floor the same total failure does fire: 20 genuine and 10 in the
  // short window, 0 fast anywhere.
  let floor = evAt(joinLines(0, 0, 0), EMPTY_STATE, -21600).state;
  floor = evAt(joinLines(0, 0, 0), floor, -18300).state;
  floor = evAt(joinLines(0, 0, 0), floor, -300).state;
  floor = evAt(joinLines(0, 0, 0), floor, 0).state;
  const atFloor = evAt(joinLines(10, 0, 0), floor, 3300);
  assert.deepEqual(evAt(joinLines(20, 0, 0), atFloor.state, 3600).firing, ["voice_join_fast_burn", "voice_join_slow_burn"]);
});

test("voice join burn ignores a just-received join while its move is still in flight", () => {
  let state = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0).state;
  const inFlight = evAt(joinLines(12, 0, 11), state, 60);
  assert.deepEqual(inFlight.firing, []);
  const completed = evAt(joinLines(12, 0, 12), inFlight.state, 120);
  assert.deepEqual(completed.firing, []);
});

test("voice join burn waits for full window history after a fresh ring", () => {
  let state = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0).state;
  state = evAt(joinLines(12, 0, 10), state, 60).state;
  // The next scrape sees the received counter, but no long-window boundary
  // has a snapshot before it yet. A partial ring must not act like full windows.
  const partial = evAt(joinLines(12, 0, 10), state, 120);
  assert.deepEqual(partial.firing, []);
  assert.equal(partial.joinSlowCovered, false);
});

test("voice join burn stays silent when healthy bursts land on window edges", () => {
  let state = EMPTY_STATE;
  // Build complete history with a low, healthy arrival rate. All joins move
  // within five seconds; the bursts sit on the 6 h, 30 min, 1 h and 5 min edges.
  for (let minute = -360; minute <= 360; minute++) {
    const arrivals = minute <= 0 ? 0 : 3 + (minute === 1 ? 40 : 0) + (minute === 331 ? 20 : 0)
      + (minute === 301 ? 30 : 0) + (minute === 356 ? 10 : 0);
    const total = (state.joinBurn?.at(-1)?.received ?? 0) + arrivals;
    state = evAt(joinLines(total, 0, total), state, minute * 60).state;
  }
  const settled = evAt(joinLines(state.joinBurn?.at(-1)?.received ?? 0, 0, state.joinBurn?.at(-1)?.fastMoves ?? 0), state, 360 * 60 + 60);
  assert.deepEqual(settled.firing, []);
  assert.equal(settled.joinSlowCovered, true);
  assert.ok(!settled.firing.includes("voice_join_fast_burn"), "healthy bursts must not page");
  assert.ok(!settled.firing.includes("voice_join_slow_burn"), "healthy bursts must not ticket");
});

test("voice join burn remains silent across the reviewed healthy edge-burst scenario", () => {
  let state = EMPTY_STATE;
  let received = 0;
  for (let tick = -360; tick <= 360; tick++) {
    const arrivals = tick > 0 && tick < 360 ? 3 + (tick === 300 ? 30 : 0) + (tick === 355 ? 3 : 0) : 0;
    received += arrivals;
    state = evAt(joinLines(received, 0, received), state, tick * 60).state;
  }
  assert.deepEqual(state.firing, []);
});

test("voice join burn requires more than one bad move per window", () => {
  let state = evAt(joinLines(0, 0, 0), EMPTY_STATE, 0).state;
  // A quiet hour gives the fast windows complete history without any burn.
  for (let i = 1; i <= 60; i++) {
    state = evAt(joinLines(0, 0, 0), state, i * 60).state;
  }
  state = evAt(joinLines(12, 0, 11), state, 3660).state;
  // One completed slow move among twelve joins must not page or ticket.
  assert.deepEqual(evAt(joinLines(12, 0, 11), state, 3720).firing, []);
});

test("voice join burn rebuilds on restart and holds through malformed scrapes", () => {
  let state = EMPTY_STATE;
  for (let now = -21600; now <= 0; now += 60) {
    state = evAt(joinLines(0, 0, 0), state, now).state;
  }
  for (let i = 1; i <= 60; i++) {
    state = evAt(joinLines(i * 100, 0, i * 90), state, i * 60).state;
  }
  const firing = evAt(joinLines(6100, 0, 5490), state, 3660);
  assert.deepEqual(firing.firing, ["voice_join_fast_burn", "voice_join_slow_burn"]);
  // A counter that went backwards means the process restarted: the ring is
  // rebuilt from this sample and both rules resolve instead of firing.
  const restart = evAt(joinLines(1, 0, 0), firing.state, 3720);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.joinBurn?.length, 1);
  // A scrape that renames the bucket holds the firing key instead of
  // resolving it: a Rust rename must fail loudly, never go quiet.
  const renamed = evAt([
    `two_bot_voice_joins_received_total 6200`,
    `two_bot_voice_join_refused_total{reason="duplicate"} 0`,
    `two_bot_voice_join_to_move_seconds_bucket{le="5s"} 5580`,
  ], firing.state, 3720);
  assert.deepEqual(renamed.firing, ["voice_join_fast_burn", "voice_join_slow_burn"]);
  // Five healthy hours dilute the old slow burn and resolve both rules.
  let recoveredState = firing.state;
  for (let i = 1; i <= 299; i++) {
    recoveredState = evAt(joinLines(6100 + i * 100, 0, 5490 + i * 100), recoveredState, 3660 + i * 60).state;
  }
  const recovered = evAt(joinLines(36100, 0, 35490), recoveredState, 21660);
  assert.deepEqual(recovered.firing, []);
});

test("gateway missed events fire on any increase, never on the first sample or a reset", () => {
  const base = ev([`two_bot_gateway_missed_events_total 0`]);
  assert.deepEqual(base.firing, []);
  // First sample with a nonzero counter only stores the baseline.
  const first = ev([`two_bot_gateway_missed_events_total 5`]);
  assert.deepEqual(first.firing, []);
  // An increase between two samples fires.
  const fire = ev([`two_bot_gateway_missed_events_total 6`], first.state);
  assert.deepEqual(fire.firing, ["gateway_missed_events"]);
  // A flat window recovers (no increase, no fire).
  assert.deepEqual(ev([`two_bot_gateway_missed_events_total 6`], fire.state).firing, []);
  // A counter that went backwards means the process restarted: no window.
  const restart = ev([`two_bot_gateway_missed_events_total 1`], fire.state);
  assert.deepEqual(restart.firing, []);
  // The post-restart baseline fires again on the next increase.
  assert.deepEqual(ev([`two_bot_gateway_missed_events_total 2`], restart.state).firing, ["gateway_missed_events"]);
});

const gateway = (disconnects: number, recoveries: number, ready = 0) => [
  `two_bot_gateway_disconnects_total ${disconnects}`,
  `two_bot_gateway_resumes_total ${recoveries}`,
  `two_bot_gateway_events_total{event="READY"} ${ready}`,
];
const UNPAIRED = "gateway_unpaired_disconnect";

test("gateway unpaired: a reconnect that resumes within two samples never pages", () => {
  const base = ev(gateway(0, 0));
  const sameSample = ev(gateway(1, 1), base.state);
  assert.deepEqual(sameSample.firing, []);
  const nextSample = ev(gateway(2, 1), sameSample.state);
  assert.deepEqual(nextSample.firing, []);
  assert.deepEqual(ev(gateway(2, 2), nextSample.state).firing, []);
  // A fresh READY pairs with a disconnect just like a RESUMED does.
  const readyPaired = ev(gateway(3, 2, 1), ev(gateway(2, 2), nextSample.state).state);
  assert.deepEqual(readyPaired.firing, []);
  // Two samples without a recovery stay below the bound; the recovery lands on the second.
  const late1 = ev(gateway(4, 2, 1), readyPaired.state);
  const late2 = ev(gateway(4, 2, 1), late1.state);
  assert.deepEqual([late1.firing, late2.firing], [[], []]);
  assert.equal(late2.state.gatewayUnpairedStreak, 2);
  assert.deepEqual(ev(gateway(4, 3, 1), late2.state).firing, []);
});

test("gateway unpaired pages on the third unpaired sample and resolves once recoveries catch up", () => {
  const base = ev(gateway(0, 0));
  const one = ev(gateway(1, 0), base.state);
  const two = ev(gateway(1, 0), one.state);
  assert.deepEqual([one.firing, two.firing], [[], []]);
  const fire = ev(gateway(1, 0), two.state);
  assert.deepEqual(fire.firing, [UNPAIRED]);
  assert.deepEqual(ev(gateway(1, 0), fire.state).firing, [UNPAIRED]);
  assert.deepEqual(ev(gateway(1, 1), fire.state).firing, []);
});

test("gateway unpaired: one outage with failed retries ends in a single RESUMED and never pages", () => {
  // A close frame plus one failed Twilight retry, then a single RESUMED in the
  // same window: N disconnects against one recovery is retry noise, not a loss.
  const base = ev(gateway(0, 0));
  const recovered = ev(gateway(2, 1), base.state);
  assert.deepEqual(recovered.state.gatewayUnpaired, 0);
  assert.deepEqual(recovered.firing, []);
  // No remainder lingers: flat samples stay quiet indefinitely.
  let quiet = recovered;
  for (let i = 0; i < 5; i++) {
    quiet = ev(gateway(2, 1), quiet.state);
    assert.deepEqual(quiet.firing, []);
  }
  assert.equal(quiet.state.gatewayUnpairedStreak, 0);
  // A later healthy disconnect+resume pair stays quiet too.
  const pair = ev(gateway(3, 2), quiet.state);
  assert.deepEqual(pair.firing, []);
  assert.deepEqual(ev(gateway(3, 2), pair.state).firing, []);
});

test("gateway unpaired: close frame, then op 9, then a fresh READY never pages", () => {
  const base = ev(gateway(0, 0));
  const close = ev(gateway(1, 0), base.state);
  const op9 = ev(gateway(2, 0), close.state);
  assert.deepEqual([close.firing, op9.firing], [[], []]);
  const ready = ev(gateway(2, 0, 1), op9.state);
  assert.deepEqual(ready.firing, []);
  let quiet = ready;
  for (let i = 0; i < 5; i++) {
    quiet = ev(gateway(2, 0, 1), quiet.state);
    assert.deepEqual(quiet.firing, []);
  }
});

test("gateway unpaired never fires on the first sample, which only stores the baseline", () => {
  const first = ev(gateway(5, 0));
  assert.deepEqual(first.firing, []);
  assert.equal(first.state.gatewayUnpaired, 0);
  assert.deepEqual(ev(gateway(5, 0), first.state).firing, []);
});

test("gateway unpaired restart holds the firing key for one sample, then re-baselines", () => {
  const base = ev(gateway(0, 0));
  const fire = [1, 1, 1].reduce((s) => ev(gateway(1, 0), s.state), base);
  assert.deepEqual(fire.firing, [UNPAIRED]);
  // Counters went backwards: no window, so the sample neither pages nor clears.
  const restart = ev(gateway(0, 1), fire.state);
  assert.deepEqual(restart.firing, [UNPAIRED]);
  assert.equal(restart.state.gatewayUnpaired, 0);
  // Growth after the restart is measured from the new baseline and holds while unpaired.
  const again = ev(gateway(1, 1), restart.state);
  assert.deepEqual(again.firing, [UNPAIRED]);
  assert.deepEqual(ev(gateway(1, 2), again.state).firing, []);
});

test("gateway unpaired restart never pages by itself", () => {
  const base = ev(gateway(0, 0));
  const pending = ev(gateway(1, 0), base.state);
  const restart = ev(gateway(0, 0), pending.state);
  assert.deepEqual(restart.firing, []);
  const s1 = ev(gateway(1, 0), restart.state);
  const s2 = ev(gateway(1, 0), s1.state);
  assert.deepEqual([s1.firing, s2.firing], [[], []]);
});

test("gateway unpaired: missing, duplicated, or non-numeric samples never clear a firing state", () => {
  let fired = ev(gateway(0, 0));
  for (let i = 0; i < 3; i++) fired = ev(gateway(1, 0), fired.state);
  assert.deepEqual(fired.firing, [UNPAIRED]);
  const absent = ev([`two_bot_job_consecutive_failures{job="counter"} 0`], fired.state);
  assert.deepEqual(absent.firing, [UNPAIRED]);
  assert.deepEqual(absent.state.gatewayUnpaired, fired.state.gatewayUnpaired);
  const nan = ev(["two_bot_gateway_disconnects_total NaN", "two_bot_gateway_resumes_total 0", `two_bot_gateway_events_total{event="READY"} 0`], fired.state);
  assert.deepEqual(nan.firing, [UNPAIRED]);
  const duplicated = ev([...gateway(1, 0), "two_bot_gateway_resumes_total 0"], fired.state);
  assert.deepEqual(duplicated.firing, [UNPAIRED]);
  assert.deepEqual(ev(gateway(1, 1), absent.state).firing, []);
});

const NOT_READY = "gateway_not_ready";

test("gateway not ready pages on the third consecutive non-ready sample and clears on any ready sample", () => {
  // The first valid sample only stores the baseline, even while non-ready.
  const one = ev([], EMPTY_STATE, false);
  assert.deepEqual(one.firing, []);
  assert.equal(one.state.gatewayNotReadySeen, true);
  assert.equal(one.state.gatewayNotReadyStreak, 1);
  const two = ev([], one.state, false);
  assert.deepEqual(two.firing, []);
  assert.equal(two.state.gatewayNotReadyStreak, 2);
  const fire = ev([], two.state, false);
  assert.deepEqual(fire.firing, [NOT_READY]);
  // Stays firing while non-ready; starting (Armed) counts, not just down.
  assert.deepEqual(ev([], fire.state, false).firing, [NOT_READY]);
  // Any ready sample clears at once.
  const clear = ev([], fire.state, true);
  assert.deepEqual(clear.firing, []);
  assert.equal(clear.state.gatewayNotReadyStreak, 0);
  // Ready never fires and never starts a streak.
  const calm = ev([], EMPTY_STATE, true);
  assert.deepEqual(calm.firing, []);
  assert.equal(calm.state.gatewayNotReadyStreak, 0);
});

test("gateway not ready: invalid samples hold the streak and the firing state", () => {
  let fired = ev([], EMPTY_STATE, false);
  fired = ev([], fired.state, false);
  fired = ev([], fired.state, false);
  assert.deepEqual(fired.firing, [NOT_READY]);
  // An invalid sample (failed probe or unparsable body) changes nothing.
  const held = ev([], fired.state, undefined);
  assert.deepEqual(held.firing, [NOT_READY]);
  assert.equal(held.state.gatewayNotReadyStreak, fired.state.gatewayNotReadyStreak);
  // ...and never starts a streak on its own.
  const fresh = ev([], EMPTY_STATE, undefined);
  assert.deepEqual(fresh.firing, []);
  assert.equal(fresh.state.gatewayNotReadySeen, false);
});

test("gateway not ready restart re-baselines the streak and holds a firing alert", () => {
  const flat = (disconnects: number, recoveries: number) => [
    `two_bot_gateway_disconnects_total ${disconnects}`,
    `two_bot_gateway_resumes_total ${recoveries}`,
    `two_bot_gateway_events_total{event="READY"} 0`,
  ];
  let fired = ev(flat(5, 3), EMPTY_STATE, false);
  fired = ev(flat(5, 3), fired.state, false);
  fired = ev(flat(5, 3), fired.state, false);
  assert.deepEqual(fired.firing, [NOT_READY]);
  // Counters went backwards: the process restarted. The streak re-baselines
  // from this sample but the firing alert holds rather than clearing.
  const restart = ev(flat(0, 0), fired.state, false);
  assert.deepEqual(restart.firing, [NOT_READY]);
  assert.equal(restart.state.gatewayNotReadyStreak, 1);
  // Samples after the restart recount from the new baseline while held.
  const again = ev(flat(0, 0), restart.state, false);
  assert.deepEqual(again.firing, [NOT_READY]);
  assert.equal(again.state.gatewayNotReadyStreak, 2);
  // A restart that lands on ready clears at once: the new process came up
  // connected, so readiness resolves even across the restart re-baseline.
  const restartReady = ev(flat(0, 0), fired.state, true);
  assert.deepEqual(restartReady.firing, []);
  assert.equal(restartReady.state.gatewayNotReadyStreak, 0);
  assert.deepEqual(ev(flat(0, 0), restartReady.state, true).firing, []);
});

test("gateway not ready restart with an invalid sample re-baselines the streak and holds firing", () => {
  const flat = (disconnects: number, recoveries: number) => [
    `two_bot_gateway_disconnects_total ${disconnects}`,
    `two_bot_gateway_resumes_total ${recoveries}`,
    `two_bot_gateway_events_total{event="READY"} 0`,
  ];
  let fired = ev(flat(5, 3), EMPTY_STATE, false);
  fired = ev(flat(5, 3), fired.state, false);
  fired = ev(flat(5, 3), fired.state, false);
  assert.deepEqual(fired.firing, [NOT_READY]);
  // Counters went backwards but this sample's readiness is invalid: the stale
  // streak re-baselines to zero instead of surviving the restart, while the
  // firing alert holds rather than clearing.
  const restart = ev(flat(0, 0), fired.state, undefined);
  assert.deepEqual(restart.firing, [NOT_READY]);
  assert.equal(restart.state.gatewayNotReadyStreak, 0);
  // The next valid non-ready sample recounts from 1: it cannot fire off the
  // pre-restart streak.
  const one = ev(flat(0, 0), restart.state, false);
  assert.equal(one.state.gatewayNotReadyStreak, 1);
  // ...and a sub-threshold streak never pages off a restart either.
  const pending = ev(flat(5, 3), ev(flat(5, 3), EMPTY_STATE, false).state, false);
  const stale = ev(flat(0, 0), pending.state, undefined);
  assert.deepEqual(stale.firing, []);
  assert.equal(stale.state.gatewayNotReadyStreak, 0);
  assert.deepEqual(ev(flat(0, 0), stale.state, false).firing, []);
});

test("gateway not ready restart never pages by itself", () => {
  const flat = (disconnects: number, recoveries: number) => [
    `two_bot_gateway_disconnects_total ${disconnects}`,
    `two_bot_gateway_resumes_total ${recoveries}`,
    `two_bot_gateway_events_total{event="READY"} 0`,
  ];
  const pending = ev(flat(5, 3), ev(flat(5, 3), EMPTY_STATE, false).state, false);
  const restart = ev(flat(0, 0), pending.state, false);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.gatewayNotReadyStreak, 1);
  const s1 = ev(flat(0, 0), restart.state, false);
  assert.deepEqual(s1.firing, []);
});

test("gateway not ready reads Durable Object state written before its fields existed", () => {
  const legacy = {
    firing: [] as string[], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0, sendBlocked: 0, sendBlockedStreak: 0,
    voiceOps: 0, voiceFailures: 0, voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {},
  } as MetricsAlertState;
  const first = ev([], legacy, false);
  assert.deepEqual(first.firing, []);
  assert.equal(first.state.gatewayNotReadySeen, true);
  assert.equal(first.state.gatewayNotReadyStreak, 1);
});

test("gateway unpaired reads Durable Object state written before its fields existed", () => {
  const legacy = {
    firing: [] as string[], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0, sendBlocked: 0, sendBlockedStreak: 0,
    voiceOps: 0, voiceFailures: 0, voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {},
  } as MetricsAlertState;
  const first = ev(gateway(7, 0), legacy);
  assert.deepEqual(first.firing, []);
  assert.deepEqual(ev(gateway(8, 0), first.state).firing, []);
});

test("gateway checkpoint failures fire on any increase, never on the first sample or a reset", () => {
  // Baseline samples are silent, even with a nonzero counter.
  const base = ev([`two_bot_gateway_checkpoint_failures_total{stage="pre_commit"} 0`, `two_bot_gateway_checkpoint_failures_total{stage="commit"} 0`]);
  assert.deepEqual(base.firing, []);
  const first = ev([`two_bot_gateway_checkpoint_failures_total{stage="pre_commit"} 2`, `two_bot_gateway_checkpoint_failures_total{stage="commit"} 3`]);
  assert.deepEqual(first.firing, []);
  // An increase on `commit` fires; the next flat sample resolves.
  const fire = ev(
    [`two_bot_gateway_checkpoint_failures_total{stage="pre_commit"} 2`, `two_bot_gateway_checkpoint_failures_total{stage="commit"} 4`],
    first.state,
  );
  assert.deepEqual(fire.firing, ["gateway_checkpoint_failures"]);
  assert.deepEqual(
    ev(
      [`two_bot_gateway_checkpoint_failures_total{stage="pre_commit"} 2`, `two_bot_gateway_checkpoint_failures_total{stage="commit"} 4`],
      fire.state,
    ).firing,
    [],
  );
  // An increase on `pre_commit` fires too: the rule sums over every stage.
  assert.deepEqual(
    ev(
      [`two_bot_gateway_checkpoint_failures_total{stage="pre_commit"} 3`, `two_bot_gateway_checkpoint_failures_total{stage="commit"} 4`],
      fire.state,
    ).firing,
    ["gateway_checkpoint_failures"],
  );
  // A counter that went backwards means the process restarted: no window.
  const restart = ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 1`], fire.state);
  assert.deepEqual(restart.firing, []);
  // The post-restart baseline fires again on the next increase.
  assert.deepEqual(
    ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 2`], restart.state).firing,
    ["gateway_checkpoint_failures"],
  );
  // An exposition without the series neither fires nor throws.
  assert.deepEqual(ev([], first.state).firing.filter((k) => k === "gateway_checkpoint_failures"), []);
  // DO storage written before the new fields existed does not throw: the
  // missing counters read as zero and the missing seen-flags as false.
  const legacy = { ...first.state, checkpointFailures: undefined, checkpointFailuresSeen: undefined } as unknown as typeof first.state;
  assert.deepEqual(
    ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 9`], legacy).firing.filter((k) => k === "gateway_checkpoint_failures"),
    [],
  );
});

test("gateway checkpoint failure pages on the pre-exit scrape and resolves after the restart", () => {
  // Models the linger-then-exit lifecycle: `shutdown::RUNTIME_FAILURE_LINGER`
  // (75 s) outlasts one 60 s keepalive tick plus two 6 s probe timeouts with
  // about 3 s of margin (pinned in checkpoint-linger.test.ts), so the
  // incremented counter is scraped before the process exits and the restart
  // resets it to zero.
  const baseline = ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 0`]);
  assert.deepEqual(baseline.firing, []);
  // Pre-exit scrape during the linger: the increase pages.
  const page = ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 1`], baseline.state);
  assert.deepEqual(page.firing, ["gateway_checkpoint_failures"]);
  // Post-restart scrape: the counter reset resolves without firing.
  const afterRestart = ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 0`], page.state);
  assert.deepEqual(afterRestart.firing, []);
  // The replacement stays quiet until a new failure increments again.
  assert.deepEqual(ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 0`], afterRestart.state).firing, []);
  assert.deepEqual(
    ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 1`], afterRestart.state).firing,
    ["gateway_checkpoint_failures"],
  );
});

test("receiver refusals need three consecutive windows with new refusals, never the first sample or a reset", () => {
  const refused = (family: string, outcome: string, n: number) =>
    `two_bot_internal_actions_total{family="${family}",outcome="${outcome}"} ${n}`;
  const executed = (family: string, n: number) =>
    `two_bot_internal_actions_total{family="${family}",outcome="executed"} ${n}`;
  const mod = (auth: number, rate = 2, exec = 10) => [
    refused("moderation", "auth_failure", auth),
    refused("moderation", "rate_limit", rate),
    executed("moderation", exec),
  ];
  // First sample only stores the baseline, even with nonzero refusals.
  const first = ev(mod(5));
  assert.deepEqual(first.firing, []);
  // One or two rising windows stay silent: a single forged pre-auth probe
  // (always family `other`) must not page.
  const second = ev(mod(6, 2, 100), first.state);
  assert.deepEqual(second.firing, []);
  const singleOther = ev(
    [`two_bot_internal_actions_total{family="other",outcome="auth_failure"} 1`],
    ev([`two_bot_internal_actions_total{family="other",outcome="auth_failure"} 0`]).state,
  );
  assert.deepEqual(singleOther.firing, []);
  // Two rising windows stay silent; the third consecutive rise fires with
  // the family subject; executed growth alone stays silent.
  const third = ev(mod(7, 2, 101), second.state);
  assert.deepEqual(third.firing, []);
  const fire = ev(mod(8, 2, 102), third.state);
  assert.deepEqual(fire.firing, ["receiver_refusals:moderation"]);
  // A flat window clears the streak and recovers (no increase, no fire).
  const flat = ev(mod(8, 2, 103), fire.state);
  assert.deepEqual(flat.firing, []);
  assert.equal(flat.state.receiverRefusalStreaks["moderation"], 0);
  // Other families track their own streak: one `membership` refusal stays silent.
  const mixedOne = ev([
    refused("moderation", "auth_failure", 8),
    refused("moderation", "rate_limit", 2),
    refused("membership", "unknown_key", 1),
    executed("membership", 3),
  ], flat.state);
  assert.deepEqual(mixedOne.firing, []);
  // A counter that went backwards means the process restarted: no window.
  const restart = ev([refused("moderation", "auth_failure", 1)], fire.state);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.receiverRefusalStreaks["moderation"] ?? 0, 0);
  // The post-restart baseline needs three fresh rises before firing again.
  const r1 = ev([refused("moderation", "auth_failure", 2)], restart.state);
  const r2 = ev([refused("moderation", "auth_failure", 3)], r1.state);
  assert.deepEqual(r1.firing, []);
  assert.deepEqual(r2.firing, []);
  assert.deepEqual(ev([refused("moderation", "auth_failure", 4)], r2.state).firing, ["receiver_refusals:moderation"]);
});

test("ticker stale fires past 10 minutes, ignores boot, parked and fresh tickers", () => {
  // Boot (never succeeded) and parked (never registered) stay zero: silent.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} 0`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} 0`]).firing, []);
  // Fresh ticks are silent; a 10-minute-old success is stale.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} ${NOW - 600}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} ${NOW - 601}`]).firing, ["ticker_stale:scheduled_messages"]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} ${NOW - 601}`]).firing, ["ticker_stale:settings"]);
  // Recovery: a fresh success after a stale window stops firing.
  const stale = ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} ${NOW - 601}`]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} ${NOW - 1}`], stale.state).firing, []);
  // Jobs outside TICKER_STALE_JOBS never fire this rule, however stale.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="audit_retry"} ${NOW - 3600}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 3600}`]).firing.filter((k) => k.startsWith("ticker_stale")), []);
});

test("unknown-cadence jobs ticket past the fallback window, never on fresh or never-succeeded", () => {
  // Boot (never succeeded) and parked (never registered) stay zero: silent.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="audit_retry"} 0`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="other"} 0`]).firing, []);
  // Fresh successes are silent, up to and including the 2-hour fallback edge.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="audit_retry"} ${NOW - 7200}`]).firing, []);
  // A stale success timestamp tickets (never pages) with the job subject.
  const firing = ev([`two_bot_job_last_success_timestamp_seconds{job="audit_retry"} ${NOW - 7201}`]).firing;
  assert.deepEqual(firing, ["job_unknown_stale:audit_retry"]);
  assert.equal(ruleFor("job_unknown_stale:audit_retry")?.severity, "ticket");
  // Future labels the scrape accepts fail closed the same way, with no catalog entry.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="unban_sweep"} ${NOW - 7201}`]).firing, ["job_unknown_stale:unban_sweep"]);
  // Event-driven session_checkpoint never tickets however stale: idle guilds
  // (no dispatches for 2+ hours) are healthy, not wedged.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="session_checkpoint"} 0`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="session_checkpoint"} ${NOW - 7200}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="session_checkpoint"} ${NOW - 7201}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="session_checkpoint"} ${NOW - 86400}`]).firing, []);
  // Mapped jobs never take this path, however stale: rank keeps its exact
  // job_stale key and tickers keep ticker_stale.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 7201}`]).firing, ["job_stale:rank"]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} ${NOW - 7201}`]).firing, ["ticker_stale:scheduled_messages"]);
  // Recovery: a fresh success after a stale window stops firing.
  const stale = ev([`two_bot_job_last_success_timestamp_seconds{job="other"} ${NOW - 7201}`]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="other"} ${NOW - 1}`], stale.state).firing, []);
});

test("fired packets carry the shared rule-id spelling (TOG-12100)", () => {
  const window = "2026-10-09T20-11-06Z";
  // Single shared spelling with the Rust canonical list (ALERT_RULE_IDS in
  // crates/core/src/evidence.rs); both sides pin every spelling here and there.
  assert.deepEqual(RULES.map((r) => r.id), [
    "job_stale",
    "job_consecutive_failures",
    "rest_429_rate",
    "rest_5xx_rate",
    "db_pool_saturated",
    "db_errors",
    "send_admission_blocked",
    "voice_failures",
    "gateway_missed_events",
    "gateway_unpaired_disconnect",
    "gateway_not_ready",
    "ticker_stale",
    "receiver_refusals",
    "dispatch_drops",
    "job_unknown_stale",
    "gateway_checkpoint_failures",
    "voice_join_fast_burn",
    "voice_join_slow_burn",
    "voice_creator_map_stale",
    "community_facts_drain_failures",
    "eventsub_post_failed_share",
    "error_budget_fast_burn",
    "error_budget_slow_burn",
    "audit_delivery_halt",
    "rest_admission_breaker_open",
    "eventsub_tampered",
    "eventsub_reconcile_failed",
    "event_store_consumer_lag",
    "prefix_trigger_verdict_refusals",
    "voice_worker_halted",
    "voice_live_self_heal",
    "voice_live_not_ready",
    "voice_queue_stall",
    "retention_replay_needed",
    "voice_dispatch_timeouts",
    "voice_watchdog_stucks",
    "voice_halt_recoveries",
    "rest_breaker_opens",
    "rest_global_pauses",
    "gateway_latency_dead",
    "gateway_reconnect_flapping",
    "handler_duration_saturated",
    "job_failure_share",
    "rest_token_invalid",
    "rest_refusal_surge",
    "rest_global_responses_stuck",
    "feed_fetch_failures",
    "eventsub_receive_gap",
    "eventsub_reconcile_prior_failed",
    "automod_sanction_refused",
    "moderation_enforcement_failures",
    "voice_rest_stuck",
    "voice_global_backoff_capped",
  ]);
  assert.equal(packetFilename("job_stale:rank", window), `evidence-job_stale-${window}.json`);
  assert.equal(packetFilename("job_consecutive_failures:counter", window), `evidence-job_consecutive_failures-${window}.json`);
  assert.equal(packetFilename("rest_429_rate", window), `evidence-rest_429_rate-${window}.json`);
  assert.equal(packetFilename("rest_5xx_rate", window), `evidence-rest_5xx_rate-${window}.json`);
  assert.equal(packetFilename("db_pool_saturated", window), `evidence-db_pool_saturated-${window}.json`);
  assert.equal(packetFilename("db_errors", window), `evidence-db_errors-${window}.json`);
  assert.equal(packetFilename("send_admission_blocked", window), `evidence-send_admission_blocked-${window}.json`);
  assert.equal(packetFilename("voice_failures", window), `evidence-voice_failures-${window}.json`);
  assert.equal(packetFilename("gateway_missed_events", window), `evidence-gateway_missed_events-${window}.json`);
  assert.equal(packetFilename("gateway_unpaired_disconnect", window), `evidence-gateway_unpaired_disconnect-${window}.json`);
  assert.equal(packetFilename("gateway_not_ready", window), `evidence-gateway_not_ready-${window}.json`);
  assert.equal(packetFilename("ticker_stale:scheduled_messages", window), `evidence-ticker_stale-${window}.json`);
  assert.equal(packetFilename("receiver_refusals:moderation", window), `evidence-receiver_refusals-${window}.json`);
  assert.equal(packetFilename("dispatch_drops:reactions", window), `evidence-dispatch_drops-${window}.json`);
  assert.equal(packetFilename("job_unknown_stale:audit_retry", window), `evidence-job_unknown_stale-${window}.json`);
  assert.equal(packetFilename("gateway_checkpoint_failures", window), `evidence-gateway_checkpoint_failures-${window}.json`);
  assert.equal(packetFilename("voice_join_fast_burn", window), `evidence-voice_join_fast_burn-${window}.json`);
  assert.equal(packetFilename("voice_join_slow_burn", window), `evidence-voice_join_slow_burn-${window}.json`);
  assert.equal(packetFilename("voice_creator_map_stale", window), `evidence-voice_creator_map_stale-${window}.json`);
  assert.equal(packetFilename("community_facts_drain_failures", window), `evidence-community_facts_drain_failures-${window}.json`);
  assert.equal(packetFilename("eventsub_post_failed_share", window), `evidence-eventsub_post_failed_share-${window}.json`);
  assert.equal(packetFilename("event_store_consumer_lag", window), `evidence-event_store_consumer_lag-${window}.json`);
  assert.equal(packetFilename("error_budget_fast_burn", window), `evidence-error_budget_fast_burn-${window}.json`);
  assert.equal(packetFilename("error_budget_slow_burn", window), `evidence-error_budget_slow_burn-${window}.json`);
  assert.equal(packetFilename("rest_admission_breaker_open", window), `evidence-rest_admission_breaker_open-${window}.json`);
  assert.equal(packetFilename("audit_delivery_halt", window), `evidence-audit_delivery_halt-${window}.json`);
  assert.equal(packetFilename("eventsub_tampered", window), `evidence-eventsub_tampered-${window}.json`);
  assert.equal(packetFilename("eventsub_reconcile_failed", window), `evidence-eventsub_reconcile_failed-${window}.json`);
  assert.equal(packetFilename("prefix_trigger_verdict_refusals", window), `evidence-prefix_trigger_verdict_refusals-${window}.json`);
  assert.equal(packetFilename("voice_worker_halted", window), `evidence-voice_worker_halted-${window}.json`);
  assert.equal(packetFilename("voice_live_self_heal", window), `evidence-voice_live_self_heal-${window}.json`);
  assert.equal(packetFilename("voice_live_not_ready", window), `evidence-voice_live_not_ready-${window}.json`);
  assert.equal(packetFilename("voice_queue_stall", window), `evidence-voice_queue_stall-${window}.json`);
  assert.equal(packetFilename("retention_replay_needed", window), `evidence-retention_replay_needed-${window}.json`);
  assert.equal(packetFilename("voice_dispatch_timeouts", window), `evidence-voice_dispatch_timeouts-${window}.json`);
  assert.equal(packetFilename("voice_watchdog_stucks", window), `evidence-voice_watchdog_stucks-${window}.json`);
  assert.equal(packetFilename("voice_halt_recoveries", window), `evidence-voice_halt_recoveries-${window}.json`);
  assert.equal(packetFilename("rest_breaker_opens", window), `evidence-rest_breaker_opens-${window}.json`);
  assert.equal(packetFilename("rest_global_pauses", window), `evidence-rest_global_pauses-${window}.json`);
  assert.equal(packetFilename("gateway_latency_dead", window), `evidence-gateway_latency_dead-${window}.json`);
  assert.equal(packetFilename("gateway_reconnect_flapping", window), `evidence-gateway_reconnect_flapping-${window}.json`);
  assert.equal(packetFilename("handler_duration_saturated", window), `evidence-handler_duration_saturated-${window}.json`);
  assert.equal(packetFilename("job_failure_share:counter", window), `evidence-job_failure_share-${window}.json`);
  assert.equal(packetFilename("rest_token_invalid", window), `evidence-rest_token_invalid-${window}.json`);
  assert.equal(packetFilename("rest_refusal_surge", window), `evidence-rest_refusal_surge-${window}.json`);
  assert.equal(packetFilename("rest_global_responses_stuck", window), `evidence-rest_global_responses_stuck-${window}.json`);
  assert.equal(packetFilename("feed_fetch_failures", window), `evidence-feed_fetch_failures-${window}.json`);
  assert.equal(packetFilename("eventsub_receive_gap", window), `evidence-eventsub_receive_gap-${window}.json`);
  assert.equal(packetFilename("eventsub_reconcile_prior_failed", window), `evidence-eventsub_reconcile_prior_failed-${window}.json`);
  assert.equal(packetFilename("automod_sanction_refused", window), `evidence-automod_sanction_refused-${window}.json`);
  assert.equal(packetFilename("moderation_enforcement_failures", window), `evidence-moderation_enforcement_failures-${window}.json`);
  assert.equal(packetFilename("voice_rest_stuck", window), `evidence-voice_rest_stuck-${window}.json`);
  assert.equal(packetFilename("voice_global_backoff_capped", window), `evidence-voice_global_backoff_capped-${window}.json`);
  // Unknown keys get no filename rather than a misleading one; hostile
  // window stamps stay filename-safe.
  assert.equal(packetFilename("no_such_rule", window), undefined);
  assert.equal(packetFilename("rest_429_rate", "2026-10-09T20:11:06Z"), "evidence-rest_429_rate-2026-10-09T20-11-06Z.json");
});

test("eventsub post failures accumulate across low-volume samples until the floor", () => {
  const posts = (attempted: number, succeeded: number, failed: number, other: number) => [
    `two_bot_eventsub_posts_total{outcome="attempted"} ${attempted}`,
    `two_bot_eventsub_posts_total{outcome="succeeded"} ${succeeded}`,
    `two_bot_eventsub_posts_total{outcome="failed"} ${failed}`,
    `two_bot_eventsub_posts_total{outcome="other"} ${other}`,
  ];
  // One failed post per sample: every window is sub-floor, so nothing fires
  // while attempts accumulate toward the 10-attempt floor.
  let state = ev(posts(10, 9, 1, 0)).state;
  for (let n = 11; n <= 18; n++) {
    const next = ev(posts(n, 9, n - 9, 0), state);
    assert.deepEqual(next.firing, [], `one failure per sample stays quiet with ${n - 10} accumulated`);
    state = next.state;
  }
  // The tenth accumulated attempt reaches the floor at a 100% share: fires.
  const fired = ev(posts(20, 9, 11, 0), state);
  assert.deepEqual(fired.firing, ["eventsub_post_failed_share"]);
  // The judged window rebaselines but the ticket holds: the next sub-floor
  // window accumulates toward the next floor instead of resolving.
  const held = ev(posts(21, 9, 12, 0), fired.state);
  assert.deepEqual(held.firing, ["eventsub_post_failed_share"]);
  assert.deepEqual(transitionMessages(fired.firing, held.firing), [], "holding is not a new ALERT");
  // A healthy accumulation (floor met, share at the line) rebaselines quietly.
  let healthy = ev(posts(0, 0, 0, 0)).state;
  for (let n = 1; n <= 9; n++) healthy = ev(posts(n, n, 0, 0), healthy).state;
  assert.deepEqual(ev(posts(10, 10, 0, 0), healthy).firing, []);
});

test("every rule links to an existing runbook heading", () => {
  const slugs = docAnchors("runbook.md");
  for (const rule of RULES) {
    const anchor = rule.runbook.split("#")[1]!;
    assert.ok(slugs.has(anchor), `${rule.id}: missing runbook heading for #${anchor}`);
  }
});

test("runbook heading slugs are unique", () => {
  const headings = [...readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8").matchAll(/^#+ (.+)$/gm)];
  const slugs = headings.map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-"));
  assert.equal(new Set(slugs).size, slugs.length, "duplicate runbook heading slug");
});

test("heading anchors keep literal underscores from inline-code headings", () => {
  const slugs = headingAnchors("## Self-diagnosing gateway failures (`/readyz` `gateway_failure`)\n");
  assert.deepEqual([...slugs], ["self-diagnosing-gateway-failures-readyz-gateway_failure"]);
});

test("job failure runbook has a first action for every fixed error class", () => {
  const text = readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8");
  const rows = new Map(text.split("\n").filter((line) => line.startsWith("| `")).map((line) => {
    const cells = line.split("|");
    return [cells[1]!.trim().replaceAll("`", ""), cells[2]!.trim()];
  }));
  for (const errorClass of JOB_ERROR_CLASSES) {
    assert.ok(rows.get(errorClass), `missing first response for ${errorClass}`);
  }
});

test("every fired packet carries a runbook deep link that resolves in checked-in docs", () => {
  const slugs = docAnchors("runbook.md");
  // Fire one of each rule kind so every RULES entry is covered.
  const firing = [
    ...ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 1201}`]).firing,
    ...ev([`two_bot_job_consecutive_failures{job="counter"} 3`]).firing,
    ...ev(
      [`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="429"} 20`],
      ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="429"} 0`]).state,
    ).firing,
    ...ev(
      [`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="5xx"} 20`],
      ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="5xx"} 0`]).state,
    ).firing,
  ];
  const poolLines = ["two_bot_db_pool_configured 1", "two_bot_db_pool_connections 5", "two_bot_db_pool_max_connections 5", "two_bot_db_pool_idle_connections 0"];
  let pool = EMPTY_STATE;
  for (let i = 0; i < 3; i++) pool = ev(poolLines, pool).state;
  firing.push(...pool.firing);
  firing.push(
    ...ev(
      [`two_bot_db_errors_total{op="admission"} 5`, `two_bot_db_errors_total{op="other"} 0`],
      ev([`two_bot_db_errors_total{op="admission"} 0`, `two_bot_db_errors_total{op="other"} 0`]).state,
    ).firing,
  );
  let blocked = EMPTY_STATE;
  for (let i = 1; i <= 3; i++) {
    blocked = ev([`two_bot_send_admissions_total{outcome="blocked"} ${i}`], blocked).state;
  }
  firing.push(...blocked.firing);
  firing.push(
    ...ev([
      `two_bot_voice_operations_total{op="create",outcome="success"} 42`,
      `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
      `two_bot_voice_operations_total{op="move",outcome="persistence"} 4`,
      `two_bot_voice_dead_letters_total{action="create"} 3`,
    ]).firing,
  );
  firing.push(
    ...ev(
      [`two_bot_gateway_missed_events_total 1`],
      ev([`two_bot_gateway_missed_events_total 0`]).state,
    ).firing,
  );
  let unpaired = ev(gateway(0, 0));
  for (let i = 0; i < 3; i++) unpaired = ev(gateway(1, 0), unpaired.state);
  firing.push(...unpaired.firing);
  let notReady = ev([], EMPTY_STATE, false);
  for (let i = 0; i < 2; i++) notReady = ev([], notReady.state, false);
  firing.push(...notReady.firing);
  firing.push(
    ...ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} ${NOW - 601}`]).firing,
  );
  let refused = ev([`two_bot_internal_actions_total{family="moderation",outcome="auth_failure"} 0`]);
  for (const n of [1, 2, 3]) {
    refused = ev([`two_bot_internal_actions_total{family="moderation",outcome="auth_failure"} ${n}`], refused.state);
  }
  firing.push(...refused.firing);
  let dropped = ev([`two_bot_dispatch_drops_total{lane="reactions"} 0`]);
  for (const n of [1, 2, 3]) {
    dropped = ev([`two_bot_dispatch_drops_total{lane="reactions"} ${n}`], dropped.state);
  }
  firing.push(...dropped.firing);
  firing.push(
    ...ev([`two_bot_job_last_success_timestamp_seconds{job="audit_retry"} ${NOW - 7201}`]).firing,
    ...ev(
      [`two_bot_gateway_checkpoint_failures_total{stage="commit"} 1`],
      ev([`two_bot_gateway_checkpoint_failures_total{stage="commit"} 0`]).state,
    ).firing,
  );
  // Complete coverage lets a 1 h spike at ~38x budget burn trip both the
  // fast (1 h + 5 min) and slow (6 h + 30 min) windows.
  const burnBase = NOW - 3600;
  let burn = evaluateMetrics(parseExposition(body(joinLines(0, 0, 0))), EMPTY_STATE, burnBase - 18300);
  burn = evaluateMetrics(parseExposition(body(joinLines(0, 0, 0))), burn.state, burnBase - 300);
  burn = evaluateMetrics(parseExposition(body(joinLines(0, 0, 0))), burn.state, burnBase);
  burn = evaluateMetrics(parseExposition(body(joinLines(900, 9, 720))), burn.state, burnBase + 3300);
  burn = evaluateMetrics(parseExposition(body(joinLines(1000, 10, 800))), burn.state, burnBase + 3600);
  firing.push(...burn.firing);
  // Forced REST error-budget burn: ~10k eligible events/hour with 2 budget
  // errors/min over 125 min burns near 12x in the 15 min / 2 min windows
  // (fast page) and near 12x in the 2 h / 15 min windows (slow ticket); the
  // per-sample 5xx share stays ~1%, so rest_5xx_rate stays silent.
  const restBudget = (total: number, errors5xx: number) => [
    `two_bot_rest_requests_total{route="other",result="2xx"} ${total - errors5xx}`,
    `two_bot_rest_requests_total{route="other",result="5xx"} ${errors5xx}`,
  ];
  let budget = evaluateMetrics(parseExposition(body(restBudget(100_000, 1_000))), EMPTY_STATE, NOW - 7600);
  let budgetTotal = 100_000;
  let budgetErrors = 1_000;
  for (let minute = 1; minute <= 125; minute += 1) {
    budgetTotal += 167;
    budgetErrors += 2;
    budget = evaluateMetrics(
      parseExposition(body(restBudget(budgetTotal, budgetErrors))),
      budget.state,
      NOW - 7600 + minute * 60,
    );
  }
  firing.push(...budget.firing);
  firing.push(...ev(["two_bot_voice_creator_map_generation 3", "two_bot_voice_creator_map_age_seconds 601"]).firing);
  let halted = EMPTY_STATE;
  for (let i = 0; i < 3; i++) halted = ev(["two_bot_audit_delivery_halt 1"], halted).state;
  firing.push(...halted.firing);
  const drainFamily = (error: number) => [
    `two_bot_community_facts_drain_failures_total{reason="error"} ${error}`,
    `two_bot_community_facts_drain_failures_total{reason="timeout"} 0`,
    `two_bot_community_facts_drain_failures_total{reason="other"} 0`,
  ];
  let drained = ev(drainFamily(0));
  for (const n of [1, 2, 3]) {
    drained = ev(drainFamily(n), drained.state);
  }
  firing.push(...drained.firing);
  firing.push(
    ...ev(
      [
        `two_bot_eventsub_posts_total{outcome="attempted"} 120`,
        `two_bot_eventsub_posts_total{outcome="succeeded"} 90`,
        `two_bot_eventsub_posts_total{outcome="failed"} 25`,
        `two_bot_eventsub_posts_total{outcome="other"} 5`,
      ],
      ev([
        `two_bot_eventsub_posts_total{outcome="attempted"} 100`,
        `two_bot_eventsub_posts_total{outcome="succeeded"} 90`,
        `two_bot_eventsub_posts_total{outcome="failed"} 5`,
        `two_bot_eventsub_posts_total{outcome="other"} 5`,
      ]).state,
    ).firing,
  );
  let breaker = EMPTY_STATE;
  for (let i = 0; i < 3; i++) breaker = ev(["two_bot_rest_guard_breaker_open 1"], breaker).state;
  firing.push(...breaker.firing);
  let tampered = ev([`two_bot_eventsub_classified_total{kind="tampered"} 0`]);
  for (const n of [1, 2, 3]) {
    tampered = ev([`two_bot_eventsub_classified_total{kind="tampered"} ${n}`], tampered.state);
  }
  firing.push(...tampered.firing);
  let lag = ev(["two_bot_event_store_consumer_lag_seconds 90"]);
  lag = ev(["two_bot_event_store_consumer_lag_seconds 90"], lag.state);
  lag = ev(["two_bot_event_store_consumer_lag_seconds 90"], lag.state);
  firing.push(...lag.firing);
  const reconcilePasses = (healthy: number, pending: number, failed: number) => [
    `two_bot_eventsub_reconcile_passes_total{outcome="healthy"} ${healthy}`,
    `two_bot_eventsub_reconcile_passes_total{outcome="pending"} ${pending}`,
    `two_bot_eventsub_reconcile_passes_total{outcome="failed"} ${failed}`,
  ];
  firing.push(...ev(reconcilePasses(3, 1, 1), ev(reconcilePasses(3, 1, 0)).state).firing);
  let prefix = ev([`two_bot_gateway_prefix_trigger_refused_total{reason="verdict"} 0`]);
  for (const n of [1, 2, 3]) {
    prefix = ev([`two_bot_gateway_prefix_trigger_refused_total{reason="verdict"} ${n}`], prefix.state);
  }
  firing.push(...prefix.firing);
  firing.push(...ev(["two_bot_voice_worker_halted 1"]).firing);
  firing.push(
    ...ev(
      [`two_bot_voice_live_self_heal_total 6`],
      ev([`two_bot_voice_live_self_heal_total 5`]).state,
    ).firing,
  );
  firing.push(...ev(["two_bot_voice_live_not_ready_seconds 121"]).firing);
  firing.push(...ev(["two_bot_voice_queue_oldest_age_seconds 121"]).firing);
  for (const series of [
    "two_bot_retention_replay_needed_total",
    "two_bot_voice_dispatch_timeouts_total",
    "two_bot_voice_watchdog_stucks_total",
    "two_bot_voice_halt_recoveries_total",
    "two_bot_rest_guard_breaker_opens_total",
    "two_bot_rest_guard_global_pauses_total",
  ]) {
    firing.push(...ev([`${series} 1`], ev([`${series} 0`]).state).firing);
  }
  // Gateway transport rules carry fresh latency in every multi-sample chain
  // except the dead-gateway battery itself, whose NaN streak is the signal.
  let dead = ev(["two_bot_gateway_latency_seconds NaN"]);
  dead = ev(["two_bot_gateway_latency_seconds NaN"], dead.state);
  dead = ev(["two_bot_gateway_latency_seconds NaN"], dead.state);
  firing.push(...dead.firing);
  let flap = ev(["two_bot_gateway_latency_seconds 0.011", "two_bot_gateway_reconnects_total 0"]);
  for (const [n, latency] of [[1, "0.012"], [2, "0.013"], [3, "0.014"]] as const) {
    flap = ev([`two_bot_gateway_latency_seconds ${latency}`, `two_bot_gateway_reconnects_total ${n}`], flap.state);
  }
  firing.push(...flap.firing);
  const handlerLines = (fast: number, total: number, latency: string) => [
    `two_bot_gateway_latency_seconds ${latency}`,
    ...["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1"].map((le) =>
      `two_bot_handler_duration_seconds_bucket{le="${le}"} ${fast}`),
    `two_bot_handler_duration_seconds_bucket{le="5"} ${total}`,
    `two_bot_handler_duration_seconds_bucket{le="+Inf"} ${total}`,
  ];
  let saturated = ev(handlerLines(100, 100, "0.011"));
  saturated = ev(handlerLines(110, 120, "0.012"), saturated.state);
  saturated = ev(handlerLines(120, 140, "0.013"), saturated.state);
  saturated = ev(handlerLines(130, 160, "0.014"), saturated.state);
  firing.push(...saturated.firing);
  const jobLines = (counterSuccess: number, counterFailure: number, latency: string) => [
    `two_bot_gateway_latency_seconds ${latency}`,
    ...JOB_RUNS_JOBS.flatMap((job) => [
      `two_bot_job_runs_total{job="${job}",outcome="success"} ${job === "counter" ? counterSuccess : 0}`,
      `two_bot_job_runs_total{job="${job}",outcome="failure"} ${job === "counter" ? counterFailure : 0}`,
    ]),
  ];
  firing.push(...ev(jobLines(8, 2, "0.012"), ev(jobLines(0, 0, "0.011")).state).firing);
  firing.push(...ev(["two_bot_rest_guard_token_invalid 1"]).firing);
  firing.push(
    ...ev(
      [
        `two_bot_rest_guard_invalid_requests_total 10`,
        `two_bot_rest_guard_rejected_requests_total 0`,
      ],
      ev([
        `two_bot_rest_guard_invalid_requests_total 0`,
        `two_bot_rest_guard_rejected_requests_total 0`,
      ]).state,
    ).firing,
  );
  let stuck = ev(["two_bot_rest_guard_pending_global_responses 2"]);
  stuck = ev(["two_bot_rest_guard_pending_global_responses 2"], stuck.state);
  stuck = ev(["two_bot_rest_guard_pending_global_responses 1"], stuck.state);
  firing.push(...stuck.firing);
  const feedErrors = (transport: number) => [
    "invalid_source", "non_public_address", "redirect_refused", "too_large", "invalid_encoding", "unsupported_content_type", "unsupported_encoding", "resolve", "transport", "deadline", "http_error", "other",
  ].map((c) => `two_bot_feed_fetch_errors_total{error_class="${c}"} ${c === "transport" ? transport : 0}`);
  firing.push(...ev(feedErrors(1), ev(feedErrors(0)).state).firing);
  const gapLines = (received: number, classified: number) => [
    `two_bot_eventsub_received_total ${received}`,
    ...rustLabels("EVENTSUB_DELIVERY_KINDS").map((k) =>
      `two_bot_eventsub_classified_total{kind="${k}"} ${k === "notification" ? classified : 0}`),
  ];
  let gap = ev(gapLines(100, 100));
  for (const n of [101, 102, 103]) gap = ev(gapLines(n, 100), gap.state);
  firing.push(...gap.firing);
  const priorFailed = (n: number) => [
    `two_bot_eventsub_reconcile_errors_total{error_class="reconcile_prior_failed"} ${n}`,
    ...reconcilePasses(n, 0, 0),
  ];
  firing.push(...ev(priorFailed(1), ev(priorFailed(0)).state).firing);
  const sanctionRefused = (n: number) => [`two_bot_automod_decisions_total{outcome="sanction_refused"} ${n}`];
  firing.push(...ev(sanctionRefused(1), ev(sanctionRefused(0)).state).firing);
  const enforcements = (kickDiscord: number) => {
    const lines: string[] = [];
    for (const verb of ["ban", "tempban", "kick", "timeout", "warn", "purge", "slowmode", "lockdown", "unlock"]) {
      for (const outcome of ["discord_error", "storage_error"]) {
        lines.push(`two_bot_moderation_enforcements_total{verb="${verb}",outcome="${outcome}"} ${verb === "kick" && outcome === "discord_error" ? kickDiscord : 0}`);
      }
    }
    return lines;
  };
  firing.push(...ev(enforcements(1), ev(enforcements(0)).state).firing);
  let voiceStuck = ev(["two_bot_voice_rest_in_flight_age_seconds 0"]);
  for (const n of [61, 75, 90]) voiceStuck = ev([`two_bot_voice_rest_in_flight_age_seconds ${n}`], voiceStuck.state);
  firing.push(...voiceStuck.firing);
  firing.push(...ev(["two_bot_voice_global_backoff_capped_total 1"], ev(["two_bot_voice_global_backoff_capped_total 0"]).state).firing);
  assert.equal(firing.length, RULES.length, `expected one firing key per rule, got: ${firing.join(", ")}`);
  const packets = transitionMessages([], firing);
  assert.equal(packets.length, RULES.length);
  for (const rule of RULES) {
    const packet = packets.find((p) => p.includes(`: ${rule.summary}. Runbook: `));
    assert.ok(packet, `${rule.id}: no fired packet carries its summary`);
    assert.ok(packet!.includes(runbookUrl(rule)), `${rule.id}: packet lacks full deep link ${runbookUrl(rule)}`);
    const afterRunbook = packet!.slice(packet!.indexOf("Runbook: ") + "Runbook: ".length);
    const url = afterRunbook.split(". Build:")[0]!;
    assert.ok(url.startsWith(RUNBOOK_BASE_URL), `${rule.id}: runbook is not a full docs URL: ${url}`);
    const anchor = url.slice(`${RUNBOOK_BASE_URL}runbook.md#`.length);
    assert.ok(slugs.has(anchor), `${rule.id}: packet URL anchor #${anchor} missing from docs/runbook.md`);
    assert.equal(ruleFor(firing.find((k) => k.startsWith(rule.id))!)?.id, rule.id);
    assert.match(packet!, /Build: unknown\. Logs: https:\/\//, `${rule.id}: packet lacks build/log context`);
  }
});

test("/ops/metrics: 404 without a configured token, 401 without/with a wrong bearer, proxied with the right one", async () => {
  const calls: Request[] = [];
  const env = (token?: string) => ({
    // The fence stamps the deployment header on every DO forward, so the
    // fixture must carry the version-metadata binding like production.
    CF_VERSION_METADATA: { id: "synthetic-metrics-deployment" },
    METRICS_SCRAPE_TOKEN: token,
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: { getByName: () => ({ fetch: async (r: Request) => { calls.push(r); return new Response("ok"); } }) },
  }) as unknown as Env;
  const ctx = {} as ExecutionContext;
  const token = "synthetic-alert-rules-scrape-token-0123456789";
  const get = (e: Env, auth?: string) =>
    worker.fetch(new Request("https://w.invalid/ops/metrics", { headers: auth ? { authorization: auth } : {} }), e, ctx);
  assert.equal((await get(env(), "Bearer anything")).status, 404);
  assert.equal((await get(env("short"))).status, 404, "short token is not configured");
  assert.equal((await get(env(token))).status, 401);
  assert.equal((await get(env(token), "Bearer nope")).status, 401);
  assert.equal((await get(env(token), "Basic nope")).status, 401);
  assert.equal(calls.length, 0, "unauthenticated requests never reach the container");
  const ok = await get(env(token), `Bearer ${token}`);
  assert.equal(ok.status, 200);
  assert.equal(calls.length, 1);
  const post = await worker.fetch(new Request("https://w.invalid/ops/metrics", { method: "POST", headers: { authorization: `Bearer ${token}` } }), env(token), ctx);
  assert.equal(post.status, 404);
});

test("slow-burn ticket survives a counter reset instead of resolving", () => {
  // Firing ~6x slow burn (167 eligible/min, 1 budget error/min), then a
  // container restart sends the cumulative counters backwards while the
  // leak continues: the ticket must hold, never post RESOLVED, and must
  // not need a fresh 2 h ring to stay filed.
  const restBudget = (total: number, errors5xx: number) => [
    `two_bot_rest_requests_total{route="other",result="2xx"} ${total - errors5xx}`,
    `two_bot_rest_requests_total{route="other",result="5xx"} ${errors5xx}`,
  ];
  const base = NOW - 200 * 60;
  let state = evaluateMetrics(parseExposition(body(restBudget(100_000, 1_000))), EMPTY_STATE, base).state;
  let total = 100_000;
  let errors = 1_000;
  for (let minute = 1; minute <= 125; minute += 1) {
    total += 167;
    errors += 1;
    state = evaluateMetrics(parseExposition(body(restBudget(total, errors))), state, base + minute * 60).state;
  }
  const firingAt = base + 125 * 60;
  assert.ok(
    evaluateMetrics(parseExposition(body(restBudget(total, errors))), state, firingAt + 60).firing.includes(
      "error_budget_slow_burn",
    ),
    "125 min of ~6x burn must ticket before the restart",
  );
  const reset = evaluateMetrics(parseExposition(body(restBudget(2_000, 20))), state, firingAt + 60);
  assert.ok(
    reset.firing.includes("error_budget_slow_burn"),
    "counter reset must hold the firing slow ticket, not resolve it",
  );
  // The leak continues after the restart: the held ticket stays filed while
  // the ring rebuilds, instead of re-ticketing as a new incident 2 h later.
  let rebuilt = reset;
  let rebuiltTotal = 2_000;
  let rebuiltErrors = 20;
  for (let minute = 1; minute <= 20; minute += 1) {
    rebuiltTotal += 167;
    rebuiltErrors += 1;
    rebuilt = evaluateMetrics(
      parseExposition(body(restBudget(rebuiltTotal, rebuiltErrors))),
      rebuilt.state,
      firingAt + 60 + minute * 60,
    );
    assert.ok(
      rebuilt.firing.includes("error_budget_slow_burn"),
      `ticket must stay filed while the ring rebuilds (minute ${minute} after restart)`,
    );
  }
});

test("slow-burn ticket resolves on a covered quiet short window while the long window is unknown", () => {
  // Firing slow burn, then a ~2 h scrape stall with quiet counters ages the
  // 2 h baseline out of tolerance while later quiet scrapes cover the
  // 15 min window: the covered quiet short window must resolve the ticket
  // instead of holding it on history alone.
  const restBudget = (total: number, errors5xx: number) => [
    `two_bot_rest_requests_total{route="other",result="2xx"} ${total - errors5xx}`,
    `two_bot_rest_requests_total{route="other",result="5xx"} ${errors5xx}`,
  ];
  const base = NOW - 200 * 60;
  let state = evaluateMetrics(parseExposition(body(restBudget(100_000, 1_000))), EMPTY_STATE, base).state;
  let total = 100_000;
  let errors = 1_000;
  for (let minute = 1; minute <= 125; minute += 1) {
    total += 167;
    errors += 1;
    state = evaluateMetrics(parseExposition(body(restBudget(total, errors))), state, base + minute * 60).state;
  }
  const firingAt = base + 125 * 60;
  total += 100;
  const stalledAt = firingAt + 7500;
  let current = evaluateMetrics(parseExposition(body(restBudget(total, errors))), state, stalledAt);
  assert.ok(
    current.firing.includes("error_budget_slow_burn"),
    "ticket must hold across the stall while the short window is uncovered",
  );
  for (let minute = 1; minute <= 16; minute += 1) {
    total += 167;
    current = evaluateMetrics(
      parseExposition(body(restBudget(total, errors))),
      current.state,
      stalledAt + minute * 60,
    );
  }
  assert.ok(
    !current.firing.includes("error_budget_slow_burn"),
    "covered quiet 15 min window must resolve the ticket even with the 2 h window unknown",
  );
});

const TOKEN_INVALID = "two_bot_rest_guard_token_invalid";
const GUARD_INVALID = "two_bot_rest_guard_invalid_requests_total";
const GUARD_REJECTED = "two_bot_rest_guard_rejected_requests_total";
const GUARD_PENDING = "two_bot_rest_guard_pending_global_responses";
const TOKEN_RULE = "rest_token_invalid";
const SURGE_RULE = "rest_refusal_surge";
const STUCK_RULE = "rest_global_responses_stuck";

test("rest token invalid pages immediately on the latch and resolves on a valid clear", () => {
  assert.equal(ruleFor(TOKEN_RULE)?.severity, undefined, "credential death pages");
  // The latch defaults to 0 at boot, so a 1 is never a boot artifact: even
  // the first valid sample fires.
  const fired = ev([`${TOKEN_INVALID} 1`]);
  assert.deepEqual(fired.firing, [TOKEN_RULE]);
  assert.deepEqual(ev([`${TOKEN_INVALID} 0`]).firing, []);
  // A valid clear resolves once: only a new process without the latch reads 0.
  const resolved = ev([`${TOKEN_INVALID} 0`], fired.state);
  assert.deepEqual(resolved.firing, []);
  const messages = transitionMessages(fired.firing, resolved.firing);
  assert.equal(messages.length, 1);
  assert.match(messages[0]!, new RegExp(`RESOLVED ${TOKEN_RULE}\\.`));
  // Staying latched holds the page without re-notifying.
  const held = ev([`${TOKEN_INVALID} 1`], fired.state);
  assert.deepEqual(held.firing, [TOKEN_RULE]);
  assert.deepEqual(transitionMessages(fired.firing, held.firing), []);
});

test("rest token invalid holds across malformed samples without manufacturing recovery", () => {
  const firing = ev([`${TOKEN_INVALID} 1`]);
  const invalidSamples: Record<string, string[]> = {
    "missing family": [],
    "non-boolean 2": [`${TOKEN_INVALID} 2`],
    negative: [`${TOKEN_INVALID} -1`],
    fractional: [`${TOKEN_INVALID} 0.5`],
    NaN: [`${TOKEN_INVALID} NaN`],
    "labeled row": [`${TOKEN_INVALID}{route="other"} 1`],
    "duplicate rows": [`${TOKEN_INVALID} 1`, `${TOKEN_INVALID} 1`],
  };
  for (const [name, lines] of Object.entries(invalidSamples)) {
    assert.deepEqual(ev(lines, firing.state).firing, [TOKEN_RULE], `${name} holds the page`);
    assert.deepEqual(ev(lines).firing, [], `${name} never fires on its own`);
  }
  assert.deepEqual(ev([`${TOKEN_INVALID} 0`], ev([`${TOKEN_INVALID} 2`], firing.state).state).firing, [],
    "a valid clear after the gap still resolves once");
});

test("rest refusal surge pages on a combined burst and ignores baselines, trickles and restarts", () => {
  assert.equal(ruleFor(SURGE_RULE)?.severity, undefined, "a refusal surge pages");
  assert.equal(REST_REFUSAL_SURGE_MIN, 10);
  const surge = (invalid: number, rejected: number) =>
    [`${GUARD_INVALID} ${invalid}`, `${GUARD_REJECTED} ${rejected}`];
  // The first valid sample only stores a baseline, even with nonzero
  // lifetime counts predating the deploy.
  const base = ev(surge(100, 50));
  assert.deepEqual(base.firing, []);
  assert.equal(base.state.restRefusalSeen, true);
  // Either family can surge alone, or both together.
  assert.deepEqual(ev(surge(110, 50), base.state).firing, [SURGE_RULE], "invalid-only burst");
  assert.deepEqual(ev(surge(100, 60), base.state).firing, [SURGE_RULE], "rejected-only burst");
  assert.deepEqual(ev(surge(105, 56), base.state).firing, [SURGE_RULE], "combined burst");
  // Nine new refusals in a window is a trickle, not a surge.
  assert.deepEqual(ev(surge(104, 55), base.state).firing, []);
  // A quiet window after a burst resolves; the burst re-fires on new growth.
  const fired = ev(surge(110, 50), base.state);
  const quiet = ev(surge(110, 50), fired.state);
  assert.deepEqual(quiet.firing, []);
  assert.deepEqual(ev(surge(120, 50), quiet.state).firing, [SURGE_RULE]);
  // Both counters backwards means the process restarted: re-baseline both
  // quietly and resolve instead of firing.
  const restart = ev(surge(5, 5), fired.state);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.restRefusalInvalid, 5);
  assert.deepEqual(ev(surge(15, 5), restart.state).firing, [SURGE_RULE]);
});

test("rest refusal surge survives a 429 retraction on the invalid counter", () => {
  const surge = (invalid: number, rejected: number) =>
    [`${GUARD_INVALID} ${invalid}`, `${GUARD_REJECTED} ${rejected}`];
  // The invalid family is not monotonic: a 429 tallied at headers is
  // withdrawn when its body shows shared scope (`retract_invalid`), so one
  // backwards step on invalid beside rejected growth is a retraction beside
  // a surge, not a restart.
  const base = ev(surge(12, 0));
  assert.deepEqual(ev(surge(11, 15), base.state).firing, [SURGE_RULE],
    "a retraction beside a rejected burst still pages");
  // A retraction straddling a firing window neither resolves the page nor
  // drops the surge: the backwards counter contributes zero growth and
  // re-baselines only itself.
  const firingBase = ev(surge(100, 50));
  const fired = ev(surge(110, 50), firingBase.state);
  assert.deepEqual(fired.firing, [SURGE_RULE]);
  const held = ev(surge(109, 70), fired.state);
  assert.deepEqual(held.firing, [SURGE_RULE], "retraction plus rejected growth stays firing");
  assert.equal(held.state.restRefusalInvalid, 109);
  assert.equal(held.state.restRefusalRejected, 70);
  // A lone retraction with no growth anywhere stays quiet and re-baselines.
  const lone = ev(surge(11, 0), base.state);
  assert.deepEqual(lone.firing, []);
  assert.equal(lone.state.restRefusalInvalid, 11);
});

test("rest refusal surge holds across malformed samples without manufacturing evidence", () => {
  const surge = (invalid: number, rejected: number) =>
    [`${GUARD_INVALID} ${invalid}`, `${GUARD_REJECTED} ${rejected}`];
  const base = ev(surge(100, 50));
  const fired = ev(surge(110, 50), base.state);
  assert.deepEqual(fired.firing, [SURGE_RULE]);
  const invalidSamples: Record<string, string[]> = {
    "missing family": [],
    "missing rejected row": [`${GUARD_INVALID} 120`],
    NaN: [...surge(120, 50).slice(0, 1), `${GUARD_REJECTED} NaN`],
    negative: [`${GUARD_INVALID} -1`, `${GUARD_REJECTED} 50`],
    fractional: [`${GUARD_INVALID} 120.5`, `${GUARD_REJECTED} 50`],
    "labeled row": [`${GUARD_INVALID}{route="other"} 120`, `${GUARD_REJECTED} 50`],
    "duplicate rows": [...surge(120, 50), `${GUARD_INVALID} 120`],
  };
  for (const [name, lines] of Object.entries(invalidSamples)) {
    const held = ev(lines, fired.state);
    assert.deepEqual(held.firing, [SURGE_RULE], `${name} holds the page`);
    assert.deepEqual(transitionMessages(fired.firing, held.firing), [], `${name} does not re-notify`);
    assert.deepEqual(ev(lines, base.state).firing, [], `${name} never fires on its own`);
  }
  // The gap keeps the previous baseline: the next valid burst still fires.
  const afterGap = ev(surge(120, 50), ev([], fired.state).state);
  assert.deepEqual(afterGap.firing, [SURGE_RULE]);
});

test("rest global responses stuck pages on the third consecutive nonzero sample", () => {
  assert.equal(ruleFor(STUCK_RULE)?.severity, undefined, "stuck admission pages");
  assert.equal(REST_PENDING_STUCK_SAMPLES, 3);
  const pending = (n: number) => [`${GUARD_PENDING} ${n}`];
  const one = ev(pending(2));
  const two = ev(pending(2), one.state);
  assert.deepEqual([one.firing, two.firing], [[], []], "brief stalls stay quiet");
  assert.equal(two.state.restPendingStuckStreak, 2);
  const fired = ev(pending(1), two.state);
  assert.deepEqual(fired.firing, [STUCK_RULE], "any nonzero count sustains the streak");
  assert.deepEqual(ev(pending(3), fired.state).firing, [STUCK_RULE]);
  // Drained bodies prove recovery and resolve once.
  const resolved = ev(pending(0), fired.state);
  assert.deepEqual(resolved.firing, []);
  assert.equal(resolved.state.restPendingStuckStreak, 0);
  const messages = transitionMessages(fired.firing, resolved.firing);
  assert.equal(messages.length, 1);
  assert.match(messages[0]!, new RegExp(`RESOLVED ${STUCK_RULE}\\.`));
  // Zero from the start never fires, however often it is sampled.
  assert.deepEqual(ev(pending(0), ev(pending(0), ev(pending(0)).state).state).firing, []);
  // Legacy DO storage without the streak reads as no streak, never as firing.
  const legacy = { ...EMPTY_STATE };
  delete legacy.restPendingStuckStreak;
  const l1 = ev(pending(1), legacy);
  const l2 = ev(pending(1), l1.state);
  assert.deepEqual([l1.firing, l2.firing], [[], []]);
  assert.deepEqual(ev(pending(1), l2.state).firing, [STUCK_RULE]);
});

test("rest global responses stuck breaks on gaps but holds firing tickets", () => {
  const pending = (n: number) => [`${GUARD_PENDING} ${n}`];
  const partial = ev(pending(1), ev(pending(1)).state);
  assert.equal(partial.state.restPendingStuckStreak, 2);
  let fired = ev(pending(1));
  fired = ev(pending(1), fired.state);
  fired = ev(pending(1), fired.state);
  assert.deepEqual(fired.firing, [STUCK_RULE]);
  const invalidSamples: Record<string, string[]> = {
    "missing family": [],
    NaN: [`${GUARD_PENDING} NaN`],
    negative: [`${GUARD_PENDING} -1`],
    fractional: [`${GUARD_PENDING} 0.5`],
    "labeled row": [`${GUARD_PENDING}{route="other"} 1`],
    "duplicate rows": [`${GUARD_PENDING} 1`, `${GUARD_PENDING} 1`],
  };
  for (const [name, lines] of Object.entries(invalidSamples)) {
    const broken = ev(lines, partial.state);
    assert.deepEqual(broken.firing, [], `${name} breaks the pending streak`);
    assert.equal(broken.state.restPendingStuckStreak, 0);
    const held = ev(lines, fired.state);
    assert.deepEqual(held.firing, [STUCK_RULE], `${name} holds the page`);
    assert.deepEqual(ev(pending(0), held.state).firing, [], `${name} still resolves on a valid drain`);
  }
  // A failed scrape breaks the streak through the shared interrupt, never the alert.
  const interrupted = interruptDispatchDrops({ ...partial, firing: [] });
  assert.equal(interrupted.restPendingStuckStreak, 0);
  assert.deepEqual(ev(pending(1), interrupted).firing, []);
  const heldFiring = interruptDispatchDrops(fired.state);
  assert.equal(heldFiring.restPendingStuckStreak, 0);
  assert.deepEqual(ev(pending(1), heldFiring).firing, [STUCK_RULE]);
  assert.deepEqual(ev(pending(0), heldFiring).firing, []);
});

test("paused and remaining gauges never fire the stuck rule on their own", () => {
  // No rule consumes the paused/remaining gauges: a live global deadline is
  // healthy Discord backpressure, and the remaining countdown derives from a
  // monotonic deadline that cannot wedge high. Only stuck bodies page.
  const paused = (remaining: number) => [
    "two_bot_rest_guard_global_paused 1",
    `two_bot_rest_guard_global_pause_remaining_seconds ${remaining}`,
    `${GUARD_PENDING} 0`,
  ];
  let state = EMPTY_STATE;
  for (const remaining of [600, 450, 300, 150, 1]) {
    const next = ev(paused(remaining), state);
    assert.deepEqual(next.firing, [], `a draining pause stays quiet at ${remaining}s remaining`);
    state = next.state;
  }
  // A pending body holds admission even at zero remaining: the stuck rule
  // reads pending alone, never the countdown.
  let stuck = ev([...paused(0).slice(0, 2), `${GUARD_PENDING} 2`]);
  stuck = ev([...paused(0).slice(0, 2), `${GUARD_PENDING} 2`], stuck.state);
  stuck = ev([...paused(0).slice(0, 2), `${GUARD_PENDING} 2`], stuck.state);
  assert.deepEqual(stuck.firing, [STUCK_RULE]);
});

test("guard health inputs validate as optional-when-present label-free series", () => {
  for (const family of [TOKEN_INVALID, GUARD_INVALID, GUARD_REJECTED, GUARD_PENDING]) {
    assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[family], {},
      `${family} is an optional input with no labels`);
    assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, family), false,
      `${family} must never become a required input (rollback containers lack it)`);
  }
  const valid = metricsBody(3);
  for (const family of [TOKEN_INVALID, GUARD_INVALID, GUARD_REJECTED, GUARD_PENDING]) {
    assert.ok(valid.includes(`${family} 0`), `fixture carries ${family} at zero`);
  }
  assert.ok(parseAlertScrape(valid));
  assert.ok(parseAlertScrape(valid.replace(`${TOKEN_INVALID} 0`, `${TOKEN_INVALID} 1`)));
  assert.ok(parseAlertScrape(valid
    .replace(`${GUARD_INVALID} 0`, `${GUARD_INVALID} 12`)
    .replace(`${GUARD_REJECTED} 0`, `${GUARD_REJECTED} 3`)
    .replace(`${GUARD_PENDING} 0`, `${GUARD_PENDING} 2`)));
  // Older exposition without the families still scrapes and still evaluates
  // every pre-existing rule.
  const older = valid.split("\n").filter((line) =>
    ![TOKEN_INVALID, GUARD_INVALID, GUARD_REJECTED, GUARD_PENDING].some((family) => line.startsWith(family))).join("\n");
  const olderSamples = parseAlertScrape(older);
  assert.ok(olderSamples, "rollback container without the guard health families must still scrape");
  assert.ok(evaluateMetrics(olderSamples, EMPTY_STATE, NOW).firing
    .includes("job_consecutive_failures:rank"));
  // Present but malformed fails the whole scrape, like every validated input:
  // a bad health series must never silently hold its rule off or on.
  for (const value of ["2", "-1", "0.5", "NaN", "+Inf"]) {
    assert.equal(parseAlertScrape(valid.replace(`${TOKEN_INVALID} 0`, `${TOKEN_INVALID} ${value}`)), null,
      `token latch ${value}`);
  }
  for (const family of [GUARD_INVALID, GUARD_REJECTED, GUARD_PENDING]) {
    for (const value of ["-1", "0.5", "NaN", "+Inf"]) {
      assert.equal(parseAlertScrape(valid.replace(`${family} 0`, `${family} ${value}`)), null,
        `${family} ${value}`);
    }
    assert.equal(parseAlertScrape(valid.replace(`${family} 0`, `${family}{route="other"} 1`)), null,
      `a labeled ${family} row is not the label-free series`);
    assert.equal(parseAlertScrape(`${valid}${family} 1\n`), null, `duplicate ${family} rows are rejected`);
  }
  assert.equal(parseAlertScrape(valid.replace(`${TOKEN_INVALID} 0`, `${TOKEN_INVALID}{route="other"} 1`)), null,
    "a labeled latch row is not the label-free gauge");
  assert.equal(parseAlertScrape(`${valid}${TOKEN_INVALID} 1\n`), null, "duplicate latch rows are rejected");
});

test("guard-health pages never fail a promotion stage", () => {
  // The canary gate stays 5xx-only: token/surge/stuck pages diagnose the
  // guard, they never roll back a promotion by themselves.
  for (const rule of [TOKEN_RULE, SURGE_RULE, STUCK_RULE]) {
    assert.deepEqual(evaluatePromotionGate([rule]), { fail: false, reasons: [] });
  }
  assert.deepEqual(
    evaluatePromotionGate([TOKEN_RULE, SURGE_RULE, STUCK_RULE, "rest_429_rate"]),
    { fail: false, reasons: [] },
  );
});

test("runbook and metrics docs explain the guard-health pages and the paused deferral", () => {
  const runbook = readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8");
  const section = (heading: string) => {
    const start = runbook.indexOf(heading);
    assert.ok(start >= 0, `runbook must carry ${heading}`);
    return runbook.slice(start, runbook.indexOf("\n#### ", start + heading.length));
  };
  const token = section("#### Alert: REST token invalid");
  assert.match(token, /credential|fatal/i);
  assert.match(token, /First response/);
  const surge = section("#### Alert: REST refusal surge");
  assert.match(surge, /First response/);
  assert.match(surge, /budget|loop|abuse/i);
  const stuck = section("#### Alert: REST global responses stuck");
  assert.match(stuck, /First response/);
  assert.match(stuck, /pending/i);
  assert.match(stuck, /healthy Discord backpressure/i, "the stuck section records why paused/remaining stay out");
  const metrics = readFileSync(new URL("../../docs/metrics.md", import.meta.url), "utf8");
  for (const rule of [TOKEN_RULE, SURGE_RULE, STUCK_RULE]) {
    assert.match(metrics, new RegExp(`\\| \`${rule}\`[^\\n]*\\|`, "i"),
      `metrics.md alert table must list ${rule}`);
  }
  const catalog = readFileSync(new URL("../../docs/observability-event-catalog.md", import.meta.url), "utf8");
  for (const rule of [TOKEN_RULE, SURGE_RULE, STUCK_RULE]) {
    assert.ok(catalog.includes(`| \`${rule}\` |`), `catalog must carry an alert-rule row for ${rule}`);
  }
});
