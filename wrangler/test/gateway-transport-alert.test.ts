import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, evaluatePromotionGate, interruptDispatchDrops, packetFilename,
  parseExposition, ruleFor, transitionMessages,
  GATEWAY_LATENCY_DEAD_SAMPLES, GATEWAY_RECONNECT_FLAP_SAMPLES,
  HANDLER_DURATION_BUCKET_LE, HANDLER_SATURATED_P99_SECONDS, HANDLER_SATURATED_MIN_OBS,
  HANDLER_SATURATED_MIN_SLOW, HANDLER_SATURATED_SHARE, HANDLER_SATURATED_SAMPLES,
  JOB_RUNS_JOBS, JOB_RUNS_OUTCOMES, JOB_FAILURE_SHARE_RATIO, JOB_FAILURE_SHARE_MIN_RUNS,
  JOB_FAILURE_SHARE_MIN_FAILURES,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines, RUST_ALERT_AXES, rustLabels } from "./fixtures/metrics.ts";

// Acceptance contract: the four gateway-transport/dispatch/job-run families
// with zero alert-rule hits get pipeline coverage. Two page (dead gateway,
// reconnect flapping: the gateway family pages) and two ticket (handler
// saturation, intermittent job failures: degraded-but-working signals).
// `two_bot_job_runs_total` is consumed as an outcome-share rule, not
// deferred: `job_consecutive_failures` pages a job failing three in a row,
// but a job failing every other run never trips it, so the share rule owns
// the intermittent case. The handler `_sum`/`_count` rows stay unconsumed by
// design (the window count reads from the `+Inf` bucket delta); unknown job
// labels and unknown handler `le` rows are ignored, never subjects. Absent
// required rows fail the whole scrape through the validator (the
// metrics_scrape_lost path), so evaluator-side missing holds like malformed
// input instead of manufacturing a page.

const DEAD = "gateway_latency_dead";
const FLAP = "gateway_reconnect_flapping";
const SATURATED = "handler_duration_saturated";
const SHARE = "job_failure_share";
const NEW_RULES = [DEAD, FLAP, SATURATED, SHARE];
const LATENCY_SERIES = "two_bot_gateway_latency_seconds";
const RECONNECT_SERIES = "two_bot_gateway_reconnects_total";
const HANDLER_SERIES = "two_bot_handler_duration_seconds_bucket";
const RUNS_SERIES = "two_bot_job_runs_total";
const NOW = 1_000_000;

function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}
const ev = (lines: string[], prev: MetricsAlertState = EMPTY_STATE, gatewayReady?: boolean) =>
  evaluateMetrics(parseExposition(body(lines)), prev, NOW, gatewayReady);

const latencyLines = (value: string): string[] => [`${LATENCY_SERIES} ${value}`];
const flapLines = (reconnects: number, latency: string): string[] => [
  `${LATENCY_SERIES} ${latency}`,
  `${RECONNECT_SERIES} ${reconnects}`,
];
const handlerLines = (fast: number, total: number, latency: string): string[] => [
  `${LATENCY_SERIES} ${latency}`,
  ...[...HANDLER_DURATION_BUCKET_LE].filter((le) => le !== "5" && le !== "+Inf").map((le) =>
    `${HANDLER_SERIES}{le="${le}"} ${fast}`),
  `${HANDLER_SERIES}{le="5"} ${total}`,
  `${HANDLER_SERIES}{le="+Inf"} ${total}`,
];
const jobLines = (runs: Record<string, { s: number; f: number }>, latency: string): string[] => [
  `${LATENCY_SERIES} ${latency}`,
  ...[...JOB_RUNS_JOBS].flatMap((job) => [
    `${RUNS_SERIES}{job="${job}",outcome="success"} ${runs[job]?.s ?? 0}`,
    `${RUNS_SERIES}{job="${job}",outcome="failure"} ${runs[job]?.f ?? 0}`,
  ]),
];

// Process-incarnation fence rows (wire contract from
// gateway-process-incarnation-acceptance.test.ts): two distinct validated
// identities plus stable pairing counters that never fire
// gateway_unpaired_disconnect on their own.
const INCARNATION_SERIES = "two_bot_process_incarnation";
const WORDS_A = [19088743, 2309737967, 2147483648, 4294967295];
const WORDS_B = [0, 1, 0, 0];
const identityLines = (words: number[]): string[] =>
  words.map((value, part) => `${INCARNATION_SERIES}{part="${part}"} ${value}`);
const pairingLines = (): string[] => [
  `two_bot_gateway_disconnects_total 0`,
  `two_bot_gateway_resumes_total 1`,
  `two_bot_gateway_events_total{event="READY"} 1`,
];

function headingAnchors(text: string): Set<string> {
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")));
}

test("catalog: rule ids match the Rust evidence list in order, with page and ticket severities", () => {
  const evidence = readFileSync(new URL("../../crates/core/src/evidence.rs", import.meta.url), "utf8");
  const rustIds = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(evidence);
  assert.ok(rustIds, "ALERT_RULE_IDS not found in crates/core/src/evidence.rs");
  assert.deepEqual(RULES.map((r) => r.id), [...rustIds[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!));
  // Concurrent sibling alert PRs append to the same tail, so positions are
  // relative: the four transport rules land after the historical prefix in
  // catalog order, wherever the tail currently ends.
  const ids = RULES.map((r) => r.id);
  const prefixAt = ids.indexOf("prefix_trigger_verdict_refusals");
  assert.ok(prefixAt >= 0, "historical prefix must end with the prefix-verdict ticket");
  const positions = NEW_RULES.map((id) => ids.indexOf(id));
  for (const [i, id] of NEW_RULES.entries()) {
    assert.equal(ids.filter((r) => r === id).length, 1, `${id} is catalogued exactly once`);
    assert.ok(positions[i]! > prefixAt, `${id} appends after the historical prefix`);
    if (i > 0) assert.ok(positions[i]! > positions[i - 1]!, `${id} keeps catalog order after ${NEW_RULES[i - 1]}`);
    assert.equal(
      packetFilename(id, "2026-10-09T20-11-06Z"),
      `evidence-${id}-2026-10-09T20-11-06Z.json`,
    );
  }
  assert.equal(ruleFor(DEAD)?.severity, undefined, "a dead gateway pages");
  assert.equal(ruleFor(FLAP)?.severity, undefined, "a flapping session pages");
  assert.equal(ruleFor(SATURATED)?.severity, "ticket", "a slow pipeline tickets, never pages");
  assert.equal(ruleFor(SHARE)?.severity, "ticket", "an intermittent job tickets, never pages");
  // Transport tickets must never fail a promotion stage.
  for (const id of NEW_RULES) {
    assert.deepEqual(evaluatePromotionGate([id]), { fail: false, reasons: [] }, `${id} stays out of the gate`);
  }
});

test("rule label vocabularies and bounds match the Rust contracts", () => {
  assert.deepEqual([...JOB_RUNS_JOBS], rustLabels("JOBS"));
  assert.deepEqual([...JOB_RUNS_OUTCOMES], rustLabels("JOB_OUTCOMES"));
  assert.deepEqual([...HANDLER_DURATION_BUCKET_LE], ["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf"]);
  assert.deepEqual(ALERT_INPUT_LABELS[HANDLER_SERIES]!["le"], [...HANDLER_DURATION_BUCKET_LE]);
  assert.deepEqual(RUST_ALERT_AXES[HANDLER_SERIES]!["le"], [...HANDLER_DURATION_BUCKET_LE]);
  // The `le` literal has no pub Rust const (private BUCKETS_MICROS), so it
  // also pins against the documented rendering in docs/metrics.md.
  const metrics = readFileSync(new URL("../../docs/metrics.md", import.meta.url), "utf8");
  assert.match(metrics, /`0\.001`, `0\.005`, `0\.01`, `0\.05`, `0\.1`, `0\.5`,\s*`1`, `5`, `\+Inf`/);
  assert.equal(GATEWAY_LATENCY_DEAD_SAMPLES, 3);
  assert.equal(GATEWAY_RECONNECT_FLAP_SAMPLES, 3);
  assert.equal(HANDLER_SATURATED_P99_SECONDS, 1);
  assert.equal(HANDLER_SATURATED_MIN_OBS, 10);
  assert.equal(HANDLER_SATURATED_MIN_SLOW, 2);
  assert.equal(HANDLER_SATURATED_SHARE, 0.01);
  assert.equal(HANDLER_SATURATED_SAMPLES, 3);
  assert.equal(JOB_FAILURE_SHARE_RATIO, 0.1);
  assert.equal(JOB_FAILURE_SHARE_MIN_RUNS, 10);
  assert.equal(JOB_FAILURE_SHARE_MIN_FAILURES, 2);
});

test("runbook and metrics docs name each rule, its anchor and its next action", () => {
  const cases = [
    { id: DEAD, anchor: "alert-gateway-latency-dead", heading: "Alert: gateway latency dead", keyword: /unchanged/ },
    { id: FLAP, anchor: "alert-gateway-reconnect-flapping", heading: "Alert: gateway reconnect flapping", keyword: /HELLO/ },
    { id: SATURATED, anchor: "alert-handler-duration-saturated", heading: "Alert: handler duration saturated", keyword: /p99/ },
    { id: SHARE, anchor: "alert-job-failure-share", heading: "Alert: job failure share", keyword: /intermittent/ },
  ] as const;
  const runbook = readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8");
  const anchors = headingAnchors(runbook);
  const metrics = readFileSync(new URL("../../docs/metrics.md", import.meta.url), "utf8");
  for (const { id, anchor, heading, keyword } of cases) {
    const rule = ruleFor(id);
    assert.ok(rule, `ruleFor(${id}) must resolve`);
    assert.equal(rule.runbook.split("#")[1], anchor);
    assert.ok(anchors.has(anchor), `docs/runbook.md needs a '#${anchor}' heading`);
    const at = runbook.indexOf(heading);
    assert.ok(at >= 0, `docs/runbook.md needs a '${heading}' heading`);
    const next = runbook.indexOf("\n#### ", at);
    const section = runbook.slice(at, next < 0 ? undefined : next);
    assert.match(section, /First response/, `${id}: runbook section names the next action`);
    assert.match(section, keyword, `${id}: runbook section names the fixed reason`);
    assert.ok(metrics.includes(`\`${id}`), `docs/metrics.md must name ${id}`);
  }
});

test("fired packets carry no raw series text", () => {
  let dead = ev(latencyLines("NaN"));
  dead = ev(latencyLines("NaN"), dead.state);
  dead = ev(latencyLines("NaN"), dead.state);
  let flap = ev(flapLines(0, "0.011"));
  for (const [n, latency] of [[1, "0.012"], [2, "0.013"], [3, "0.014"]] as const) {
    flap = ev(flapLines(n, latency), flap.state);
  }
  let saturated = ev(handlerLines(100, 100, "0.011"));
  saturated = ev(handlerLines(110, 120, "0.012"), saturated.state);
  saturated = ev(handlerLines(120, 140, "0.013"), saturated.state);
  saturated = ev(handlerLines(130, 160, "0.014"), saturated.state);
  const shareBase = ev(jobLines({}, "0.011"));
  const share = ev(jobLines({ counter: { s: 8, f: 2 } }, "0.012"), shareBase.state);
  assert.deepEqual(dead.firing, [DEAD]);
  assert.deepEqual(flap.firing, [FLAP]);
  assert.deepEqual(saturated.firing, [SATURATED]);
  assert.deepEqual(share.firing, [`${SHARE}:counter`]);
  const packets = transitionMessages([], [DEAD, FLAP, SATURATED, `${SHARE}:counter`]);
  assert.equal(packets.length, 4);
  for (const packet of packets) {
    assert.doesNotMatch(packet, /two_bot_|NaN|0\.01[1234]/, `packet must not leak series text: ${packet}`);
  }
});

test("scrape validation: the four consumed families are required with Rust-pinned labels", () => {
  const families: Record<string, Record<string, readonly string[]>> = {
    [LATENCY_SERIES]: {},
    [RECONNECT_SERIES]: {},
    [HANDLER_SERIES]: { le: [...HANDLER_DURATION_BUCKET_LE] },
    [RUNS_SERIES]: { job: rustLabels("JOBS"), outcome: rustLabels("JOB_OUTCOMES") },
  };
  for (const [family, axes] of Object.entries(families)) {
    assert.deepEqual(ALERT_INPUT_LABELS[family], axes,
      `${family}: required input pins the Rust label contract`);
    assert.equal(Object.hasOwn(ALERT_OPTIONAL_INPUT_LABELS, family), false,
      `${family}: always emitted, never optional`);
    assert.ok(metricsBody().includes(`${family} `) || metricsBody().includes(`${family}{`),
      `fixture emits ${family}`);
  }
  const full = metricsBody();
  assert.ok(parseAlertScrape(full), "current exposition parses");
  // Absence fails the whole scrape: a missing required family is blindness
  // (the metrics_scrape_lost path), never a quiet transport rule.
  for (const family of Object.keys(families)) {
    const dropped = metricsLines()
      .filter((l) => !(l.startsWith(`${family} `) || l.startsWith(`${family}{`)))
      .join("\n");
    assert.equal(parseAlertScrape(dropped), null, `missing ${family} fails the scrape`);
  }
  // Present but malformed fails the whole scrape: a bad row must never
  // silently hold a rule off or on.
  const poison = (family: string, value: string) => {
    const lines = full.split("\n");
    const i = lines.findIndex((l) => l.startsWith(`${family}{`) || l.startsWith(`${family} `));
    assert.ok(i >= 0, `fixture must emit ${family}`);
    lines[i] = `${lines[i]!.replace(/ 0$/, "")} ${value}`;
    return lines.join("\n");
  };
  for (const value of ["+Inf", "-Inf", "-1", "garbage", "1e20"]) {
    for (const family of Object.keys(families)) {
      assert.equal(parseAlertScrape(poison(family, value)), null, `${family}: ${value}`);
    }
  }
  // Counter families additionally reject NaN and fractions; the latency
  // gauge accepts both (unknown marker; sub-second samples).
  for (const family of [RECONNECT_SERIES, HANDLER_SERIES, RUNS_SERIES]) {
    for (const value of ["NaN", "0.5"]) {
      assert.equal(parseAlertScrape(poison(family, value)), null, `${family}: ${value}`);
    }
  }
  assert.ok(parseAlertScrape(poison(LATENCY_SERIES, "NaN")), "latency NaN parses");
  assert.ok(parseAlertScrape(poison(LATENCY_SERIES, "0.5")), "latency fraction parses");
  assert.equal(parseAlertScrape(full.replace(`${RECONNECT_SERIES} 0`, `${RECONNECT_SERIES}{shard="0"} 0`)), null,
    "a labeled row is not the label-free counter");
  assert.equal(parseAlertScrape(`${full}${RECONNECT_SERIES} 1\n`), null, "duplicate rows are rejected");
});

test("handler sum and count rows stay unconsumed passthrough", () => {
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  for (const sibling of ["two_bot_handler_duration_seconds_sum", "two_bot_handler_duration_seconds_count"]) {
    assert.ok(!rules.includes(`gauge("${sibling}")`), `no rule may consume ${sibling}`);
  }
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, "two_bot_handler_duration_seconds_sum"), false);
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, "two_bot_handler_duration_seconds_count"), false);
  // Sibling rows ride along without changing the verdict: saturated buckets
  // ticket with or without them.
  const withSiblings = (fast: number, total: number, latency: string) => [
    ...handlerLines(fast, total, latency),
    `two_bot_handler_duration_seconds_sum ${(total * 1.5).toFixed(3)}`,
    `two_bot_handler_duration_seconds_count ${total}`,
  ];
  let state = ev(withSiblings(100, 100, "0.011")).state;
  state = ev(withSiblings(110, 120, "0.012"), state).state;
  state = ev(withSiblings(120, 140, "0.013"), state).state;
  assert.deepEqual(ev(withSiblings(130, 160, "0.014"), state).firing, [SATURATED]);
});

test("dead gateway pages on the third evidenceless sample, NaN or unchanged", () => {
  // Unknown: NaN until the first ACK lands on this connection.
  const one = ev(latencyLines("NaN"));
  assert.deepEqual(one.firing, []);
  assert.equal(one.state.gatewayLatencySeen, true);
  assert.equal(one.state.gatewayLatencyDeadStreak, 1);
  const two = ev(latencyLines("NaN"), one.state);
  assert.deepEqual(two.firing, []);
  const fire = ev(latencyLines("NaN"), two.state);
  assert.deepEqual(fire.firing, [DEAD]);
  assert.deepEqual(ev(latencyLines("NaN"), fire.state).firing, [DEAD]);
  // Stale: a finite value with no fresh ACK behind it stops moving.
  let stale = ev(latencyLines("0.045"));
  stale = ev(latencyLines("0.045"), stale.state);
  assert.deepEqual([stale.firing, stale.state.gatewayLatencyDeadStreak], [[], 1]);
  stale = ev(latencyLines("0.045"), stale.state);
  assert.deepEqual(stale.firing, []);
  assert.deepEqual(ev(latencyLines("0.045"), stale.state).firing, [DEAD]);
  // Mixed unknown and stale accumulate: none of them is fresh evidence.
  let mixed = ev(latencyLines("0.045"));
  mixed = ev(latencyLines("0.045"), mixed.state);
  mixed = ev(latencyLines("NaN"), mixed.state);
  assert.deepEqual(mixed.firing, []);
  assert.deepEqual(ev(latencyLines("NaN"), mixed.state).firing, [DEAD]);
});

test("dead gateway clears on any fresh ACK and never fires on a clean boot", () => {
  // A finite change is fresh evidence, even mid-streak.
  let streak = ev(latencyLines("NaN"));
  streak = ev(latencyLines("NaN"), streak.state);
  assert.deepEqual(ev(latencyLines("0.045"), streak.state).firing, []);
  assert.equal(ev(latencyLines("0.045"), streak.state).state.gatewayLatencyDeadStreak, 0);
  // A finite value after unknown always counts as fresh, even when its
  // micros repeat the stale reading from before the unknown window.
  let repeat = ev(latencyLines("0.045"));
  repeat = ev(latencyLines("NaN"), repeat.state);
  const fresh = ev(latencyLines("0.045"), repeat.state);
  assert.deepEqual(fresh.firing, []);
  assert.equal(fresh.state.gatewayLatencyDeadStreak, 0);
  // A firing page resolves on the next fresh sample.
  let firing = ev(latencyLines("NaN"));
  firing = ev(latencyLines("NaN"), firing.state);
  firing = ev(latencyLines("NaN"), firing.state);
  assert.deepEqual(firing.firing, [DEAD]);
  assert.deepEqual(ev(latencyLines("0.045"), firing.state).firing, []);
  // Clean boot: Connected within ~1 min, first ACK within another interval.
  let boot = ev(latencyLines("NaN"));
  boot = ev(latencyLines("0.041"), boot.state);
  assert.deepEqual(ev(latencyLines("0.043"), boot.state).firing, []);
});

test("dead gateway co-fires with gateway_not_ready on a never-connected gateway", () => {
  // The latency page fires from the exposition while the readiness page
  // fires from the probe: a gateway that never connects trips both, and the
  // pair attributes the outage (never-connected) versus a lone latency page
  // (half-dead connection with readyz still Connected).
  let state = ev(latencyLines("NaN"), EMPTY_STATE, false).state;
  state = ev(latencyLines("NaN"), state, false).state;
  const both = ev(latencyLines("NaN"), state, false);
  assert.deepEqual(both.firing.sort(), [DEAD, "gateway_not_ready"].sort());
});

test("dead gateway holds across missing and malformed samples, never fires on them", () => {
  // Missing and malformed rows hold a firing page without manufacturing
  // evidence or recovery.
  let firing = ev(latencyLines("NaN"));
  firing = ev(latencyLines("NaN"), firing.state);
  firing = ev(latencyLines("NaN"), firing.state);
  assert.deepEqual(firing.firing, [DEAD]);
  assert.deepEqual(ev([], firing.state).firing, [DEAD]);
  assert.deepEqual(ev([...latencyLines("0.045"), "two_bot_gateway_latency_seconds 0.046"], firing.state).firing, [DEAD]);
  assert.deepEqual(ev([`${LATENCY_SERIES}{shard="0"} 0.045`], firing.state).firing, [DEAD]);
  assert.deepEqual(ev(latencyLines("-1"), firing.state).firing, [DEAD]);
  assert.deepEqual(ev(latencyLines("+Inf"), firing.state).firing, [DEAD]);
  // ...and a fresh sample after the gap still resolves.
  assert.deepEqual(ev(latencyLines("0.049"), firing.state).firing, []);
  // From clean state, missing and malformed samples never start a streak.
  assert.deepEqual(ev([]).firing, []);
  assert.equal(ev([]).state.gatewayLatencySeen, false);
  assert.deepEqual(ev([...latencyLines("0.045"), "two_bot_gateway_latency_seconds 0.046"]).firing, []);
  // A failed scrape breaks the streak; the next valid sample re-baselines
  // and holds a firing page rather than resolving it.
  let pending = ev(latencyLines("NaN"));
  pending = ev(latencyLines("NaN"), pending.state);
  assert.equal(pending.state.gatewayLatencyDeadStreak, 2);
  const broken = interruptDispatchDrops(pending.state);
  assert.equal(broken.gatewayLatencySeen, false);
  const rebaselined = ev(latencyLines("NaN"), broken);
  assert.deepEqual(rebaselined.firing, []);
  assert.equal(rebaselined.state.gatewayLatencyDeadStreak, 1);
  const held = ev(latencyLines("NaN"), interruptDispatchDrops(firing.state));
  assert.deepEqual(held.firing, [DEAD]);
});

test("dead gateway reads Durable Object state written before its fields existed", () => {
  const legacy = { ...EMPTY_STATE } as MetricsAlertState;
  delete legacy.gatewayLatencySeen;
  delete legacy.gatewayLatencyDeadStreak;
  delete legacy.gatewayLatencyLast;
  const first = ev(latencyLines("NaN"), legacy);
  assert.deepEqual(first.firing, []);
  assert.equal(first.state.gatewayLatencySeen, true);
  assert.equal(first.state.gatewayLatencyDeadStreak, 1);
});

test("reconnect flapping pages on three consecutive rises, silent on blips", () => {
  const base = ev(flapLines(0, "0.011"));
  assert.deepEqual(base.firing, []);
  // One blip (a clean recovery) and even two rising windows stay silent.
  const blip = ev(flapLines(1, "0.012"), base.state);
  assert.deepEqual(blip.firing, []);
  assert.deepEqual(ev(flapLines(1, "0.013"), blip.state).firing, []);
  const two = ev(flapLines(2, "0.013"), blip.state);
  assert.deepEqual(two.firing, []);
  // The third consecutive rise pages; a flat valid sample resolves at once.
  const fire = ev(flapLines(3, "0.014"), two.state);
  assert.deepEqual(fire.firing, [FLAP]);
  assert.deepEqual(ev(flapLines(3, "0.015"), fire.state).firing, []);
});

test("reconnect flapping rebaselines on restart and holds across gaps", () => {
  let firing = ev(flapLines(0, "0.011"));
  for (const [n, latency] of [[1, "0.012"], [2, "0.013"], [3, "0.014"]] as const) {
    firing = ev(flapLines(n, latency), firing.state);
  }
  assert.deepEqual(firing.firing, [FLAP]);
  // A backwards counter means the process restarted: re-baseline and resolve.
  const restart = ev(flapLines(0, "0.015"), firing.state);
  assert.deepEqual(restart.firing, []);
  // Growth after the restart recounts from the new baseline.
  let again = ev(flapLines(1, "0.016"), restart.state);
  again = ev(flapLines(2, "0.017"), again.state);
  assert.deepEqual(again.firing, []);
  assert.deepEqual(ev(flapLines(3, "0.018"), again.state).firing, [FLAP]);
  // Missing, duplicated and non-numeric samples hold a firing page.
  assert.deepEqual(ev([`two_bot_gateway_latency_seconds 0.019`], firing.state).firing, [FLAP]);
  assert.deepEqual(ev([...flapLines(3, "0.019"), "two_bot_gateway_reconnects_total 3"], firing.state).firing, [FLAP]);
  assert.deepEqual(ev(["two_bot_gateway_reconnects_total NaN"], firing.state).firing, [FLAP]);
  // A failed scrape breaks the streak; the next valid sample re-baselines
  // and holds a firing page rather than resolving it.
  const held = ev(flapLines(3, "0.020"), interruptDispatchDrops(firing.state));
  assert.deepEqual(held.firing, [FLAP]);
  assert.equal(held.state.gatewayReconnectStreak, 0);
});

test("handler saturation tickets on three consecutive saturated windows", () => {
  const clean = ev(handlerLines(100, 100, "0.011"));
  assert.deepEqual(clean.firing, []);
  // Each window below completes 20 dispatches with 10 slower than 1 s.
  const one = ev(handlerLines(110, 120, "0.012"), clean.state);
  const two = ev(handlerLines(120, 140, "0.013"), one.state);
  assert.deepEqual([one.firing, two.firing], [[], []]);
  assert.equal(two.state.handlerStreak, 2);
  const fire = ev(handlerLines(130, 160, "0.014"), two.state);
  assert.deepEqual(fire.firing, [SATURATED]);
  assert.deepEqual(ev(handlerLines(140, 180, "0.015"), fire.state).firing, [SATURATED]);
  // A judged-quiet window clears at once.
  assert.deepEqual(ev(handlerLines(160, 180, "0.016"), fire.state).firing, []);
});

test("handler saturation stays silent on healthy, quiet and singleton windows", () => {
  const clean = ev(handlerLines(100, 100, "0.011")).state;
  // Healthy: every completion lands at or below 1 s.
  assert.deepEqual(ev(handlerLines(120, 120, "0.012"), clean).firing, []);
  // Quiet: 9 completions with 8 slow is still below the volume floor.
  assert.deepEqual(ev(handlerLines(101, 109, "0.012"), clean).firing, []);
  // Singleton: one slow dispatch among 10 never tickets.
  assert.deepEqual(ev(handlerLines(109, 110, "0.012"), clean).firing, []);
  // At the share line (2 slow of 200) the window is not past it.
  assert.deepEqual(ev(handlerLines(298, 300, "0.012"), clean).firing, []);
  // Past the line (2 slow of 100) the streak starts but needs three windows.
  const edge = ev(handlerLines(198, 200, "0.012"), clean);
  assert.deepEqual(edge.firing, []);
  assert.equal(edge.state.handlerStreak, 1);
});

test("handler saturation resets on restart and holds across gaps", () => {
  let firing = ev(handlerLines(100, 100, "0.011"));
  firing = ev(handlerLines(110, 120, "0.012"), firing.state);
  firing = ev(handlerLines(120, 140, "0.013"), firing.state);
  firing = ev(handlerLines(130, 160, "0.014"), firing.state);
  assert.deepEqual(firing.firing, [SATURATED]);
  // A backwards bucket means the process restarted: re-baseline and resolve.
  const restart = ev(handlerLines(10, 12, "0.015"), firing.state);
  assert.deepEqual(restart.firing, []);
  assert.deepEqual(restart.state.handlerBuckets, {
    "0.001": 10, "0.005": 10, "0.01": 10, "0.05": 10, "0.1": 10, "0.5": 10, "1": 10, "5": 12, "+Inf": 12,
  });
  // Missing and malformed samples drop the baseline and hold the ticket.
  assert.deepEqual(ev([`two_bot_gateway_latency_seconds 0.016`], firing.state).firing, [SATURATED]);
  assert.deepEqual(ev(handlerLines(140, 180, "0.016").slice(0, 9), firing.state).firing, [SATURATED]);
  const badBucket = handlerLines(140, 180, "0.016");
  badBucket[9] = `${HANDLER_SERIES}{le="+Inf"} NaN`;
  assert.deepEqual(ev(badBucket, firing.state).firing, [SATURATED]);
  // A failed scrape breaks the streak; the next valid sample re-baselines
  // and holds a firing ticket rather than resolving it.
  const held = ev(handlerLines(140, 180, "0.017"), interruptDispatchDrops(firing.state));
  assert.deepEqual(held.firing, [SATURATED]);
  assert.equal(held.state.handlerStreak, 0);
});

test("job failure share tickets a sick job and stays silent on healthy runs", () => {
  const clean = ev(jobLines({}, "0.011")).state;
  // Judged failing window: 2 failures in 10 counter runs (20%).
  const fire = ev(jobLines({ counter: { s: 8, f: 2 } }, "0.012"), clean);
  assert.deepEqual(fire.firing, [`${SHARE}:counter`]);
  // Healthy jobs never ticket: 10 clean runs judge quiet.
  assert.deepEqual(ev(jobLines({ rank: { s: 10, f: 0 } }, "0.012"), clean).firing, []);
  // One bad run never tickets, even at the floor.
  assert.deepEqual(ev(jobLines({ rank: { s: 9, f: 1 } }, "0.012"), clean).firing, []);
  // Sub-floor windows accumulate: 5 runs then 5 more judge as one window.
  let acc = ev(jobLines({ feeds: { s: 4, f: 1 } }, "0.012"), clean);
  assert.deepEqual(acc.firing, []);
  acc = ev(jobLines({ feeds: { s: 8, f: 2 } }, "0.013"), acc.state);
  assert.deepEqual(acc.firing, [`${SHARE}:feeds`]);
  // A judged-healthy window resolves; a sub-floor window holds the ticket.
  const held = ev(jobLines({ counter: { s: 9, f: 2 } }, "0.013"), fire.state);
  assert.deepEqual(held.firing, [`${SHARE}:counter`]);
  assert.deepEqual(ev(jobLines({ counter: { s: 18, f: 2 } }, "0.014"), fire.state).firing, []);
});

test("job failure share tracks jobs independently and survives restarts and gaps", () => {
  const clean = ev(jobLines({}, "0.011")).state;
  const fire = ev(jobLines({ counter: { s: 8, f: 2 }, rank: { s: 10, f: 0 } }, "0.012"), clean);
  assert.deepEqual(fire.firing, [`${SHARE}:counter`]);
  // A sick rank tickets beside counter; a healed counter resolves alone.
  const both = ev(jobLines({ counter: { s: 16, f: 4 }, rank: { s: 18, f: 2 } }, "0.013"), fire.state);
  assert.deepEqual(both.firing.sort(), [`${SHARE}:counter`, `${SHARE}:rank`].sort());
  assert.deepEqual(
    ev(jobLines({ counter: { s: 26, f: 4 }, rank: { s: 26, f: 4 } }, "0.014"), both.state).firing,
    [`${SHARE}:rank`],
  );
  // A backwards counter means the process restarted: re-baseline and resolve.
  const restart = ev(jobLines({ counter: { s: 1, f: 0 } }, "0.015"), fire.state);
  assert.deepEqual(restart.firing, []);
  // Missing and malformed samples preserve firing and baseline.
  assert.deepEqual(ev([`two_bot_gateway_latency_seconds 0.016`], fire.state).firing, [`${SHARE}:counter`]);
  const missingOutcome = jobLines({ counter: { s: 16, f: 4 } }, "0.016")
    .filter((l) => l !== `${RUNS_SERIES}{job="counter",outcome="failure"} 4`);
  assert.deepEqual(ev(missingOutcome, fire.state).firing, [`${SHARE}:counter`]);
  // Unknown job labels are ignored, never subjects.
  const future = ev([...jobLines({ counter: { s: 16, f: 4 } }, "0.016"),
    `${RUNS_SERIES}{job="future_job",outcome="success"} 50`,
    `${RUNS_SERIES}{job="future_job",outcome="failure"} 50`], fire.state);
  assert.deepEqual(future.firing, [`${SHARE}:counter`]);
});

test("job failure share co-fires with consecutive failures on a continuous outage", () => {
  // A job dead enough to page consecutive_failures tickets here too: the
  // pair means continuous outage, while the share ticket alone means
  // intermittent sickness.
  const clean = ev(jobLines({}, "0.011")).state;
  const firing = ev([
    ...jobLines({ counter: { s: 8, f: 2 } }, "0.012"),
    `two_bot_job_consecutive_failures{job="counter"} 3`,
  ], clean);
  assert.deepEqual(firing.firing.sort(), ["job_consecutive_failures:counter", `${SHARE}:counter`].sort());
});

test("gateway latency dead re-baselines on incarnation restart instead of paging", () => {
  const a = [...pairingLines(), ...identityLines(WORDS_A)];
  const b = [...pairingLines(), ...identityLines(WORDS_B)];
  let s = ev([...latencyLines("0.011"), ...a]);
  s = ev([...latencyLines("0.011"), ...a], s.state);
  s = ev([...latencyLines("0.011"), ...a], s.state);
  assert.equal(s.state.gatewayLatencyDeadStreak, 2);
  assert.deepEqual(s.firing, []);
  // Restart: the new process first ACK measures microsecond-identical RTT.
  // Without the fence this third evidenceless sample would page; with it,
  // the restart re-baselines and holds.
  const restart = ev([...latencyLines("0.011"), ...b], s.state);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.gatewayLatencyDeadStreak, 0);
  assert.equal(restart.state.gatewayLatencyLast, 0.011);
  // Same-process recount after the restart: two more identical samples stay
  // silent, the third pages.
  let again = ev([...latencyLines("0.011"), ...b], restart.state);
  again = ev([...latencyLines("0.011"), ...b], again.state);
  assert.deepEqual(again.firing, []);
  assert.deepEqual(ev([...latencyLines("0.011"), ...b], again.state).firing, [DEAD]);
  // A firing page holds across the boundary instead of resolving: fresh
  // evidence on the restart sample is a new baseline, not a quiet window.
  const firing = ev([...latencyLines("0.011"), ...b], again.state);
  assert.deepEqual(firing.firing, [DEAD]);
  const held = ev([...latencyLines("0.012"), ...a], firing.state);
  assert.deepEqual(held.firing, [DEAD]);
  assert.equal(held.state.gatewayLatencyDeadStreak, 0);
});

test("reconnect flapping re-baselines on incarnation restart instead of paging", () => {
  const a = [...pairingLines(), ...identityLines(WORDS_A)];
  const b = [...pairingLines(), ...identityLines(WORDS_B)];
  let s = ev([...flapLines(1, "0.011"), ...a]);
  s = ev([...flapLines(2, "0.012"), ...a], s.state);
  s = ev([...flapLines(3, "0.013"), ...a], s.state);
  assert.equal(s.state.gatewayReconnectStreak, 2);
  assert.deepEqual(s.firing, []);
  // Restart: the new process reconnected 4 times in its first flaky minute,
  // overtaking the old baseline 3 in one sample. Without the fence this
  // reads as a third consecutive rise and pages; with it, re-baseline.
  const restart = ev([...flapLines(4, "0.014"), ...b], s.state);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.gatewayReconnectStreak, 0);
  assert.equal(restart.state.gatewayReconnects, 4);
  // Same-process recount after the restart pages on the third rise.
  let again = ev([...flapLines(5, "0.015"), ...b], restart.state);
  again = ev([...flapLines(6, "0.016"), ...b], again.state);
  assert.deepEqual(again.firing, []);
  assert.deepEqual(ev([...flapLines(7, "0.017"), ...b], again.state).firing, [FLAP]);
  // A firing page holds across the boundary instead of resolving.
  const firing = ev([...flapLines(7, "0.017"), ...b], again.state);
  assert.deepEqual(firing.firing, [FLAP]);
  assert.deepEqual(ev([...flapLines(8, "0.018"), ...a], firing.state).firing, [FLAP]);
});

test("handler saturation re-baselines on incarnation restart instead of ticketing", () => {
  const a = [...pairingLines(), ...identityLines(WORDS_A)];
  const b = [...pairingLines(), ...identityLines(WORDS_B)];
  let s = ev([...handlerLines(100, 100, "0.011"), ...a]);
  s = ev([...handlerLines(110, 120, "0.012"), ...a], s.state);
  s = ev([...handlerLines(120, 140, "0.013"), ...a], s.state);
  assert.equal(s.state.handlerStreak, 2);
  assert.deepEqual(s.firing, []);
  // Restart: the new process buckets overtake the dead baseline in one
  // sample (20 completions, 10 slow). Without the fence the delta judges
  // a third saturated window; with it, re-baseline and hold quiet.
  const restart = ev([...handlerLines(130, 160, "0.014"), ...b], s.state);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.handlerStreak, 0);
  // Same-process recount after the restart tickets on the third window.
  let again = ev([...handlerLines(140, 180, "0.015"), ...b], restart.state);
  again = ev([...handlerLines(150, 200, "0.016"), ...b], again.state);
  assert.deepEqual(again.firing, []);
  assert.deepEqual(ev([...handlerLines(160, 220, "0.017"), ...b], again.state).firing, [SATURATED]);
  // A firing ticket holds across the boundary instead of resolving.
  const firing = ev([...handlerLines(160, 220, "0.017"), ...b], again.state);
  assert.deepEqual(firing.firing, [SATURATED]);
  assert.deepEqual(ev([...handlerLines(170, 240, "0.018"), ...a], firing.state).firing, [SATURATED]);
});

test("job failure share re-baselines on incarnation restart instead of ticketing", () => {
  const a = [...pairingLines(), ...identityLines(WORDS_A)];
  const b = [...pairingLines(), ...identityLines(WORDS_B)];
  const clean = ev([...jobLines({ counter: { s: 8, f: 0 } }, "0.011"), ...a]).state;
  // Restart: the new process runs overtake the dead baseline in one sample
  // (12 runs, 2 failures = 16%). Without the fence the delta judges a
  // failing window; with it, re-baseline and hold quiet.
  const restart = ev([...jobLines({ counter: { s: 18, f: 2 } }, "0.012"), ...b], clean);
  assert.deepEqual(restart.firing, []);
  assert.deepEqual(restart.state.jobShareBase?.["counter"], { failure: 2, total: 20 });
  // Same-process accumulation after the restart tickets on a judged window.
  const fire = ev([...jobLines({ counter: { s: 26, f: 4 } }, "0.013"), ...b], restart.state);
  assert.deepEqual(fire.firing, [`${SHARE}:counter`]);
  // A firing ticket holds across the boundary instead of resolving.
  const held = ev([...jobLines({ counter: { s: 30, f: 5 } }, "0.014"), ...a], fire.state);
  assert.deepEqual(held.firing, [`${SHARE}:counter`]);
});
