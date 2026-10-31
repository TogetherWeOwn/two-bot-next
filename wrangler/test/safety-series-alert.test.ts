import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, evaluatePromotionGate, packetFilename,
  parseExposition, ruleFor, runbookUrl, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines } from "./fixtures/metrics.ts";

// Acceptance contract: the seven emitted safety/liveness series with zero
// alert-rule hits get pipeline coverage. Six are consumed here, each ticketing
// on any increase between valid samples: the retention purge-skip counter,
// the voice dispatch-timeout, watchdog-stuck and halt-recovery counters, and
// the REST-breaker opens and global-pause counters. Breaker closes
// (`two_bot_rest_guard_breaker_closes_total`) is deliberately unconsumed: a
// close is the cooling of an episode the opens ticket already named, so it
// stays resolution evidence with its deferral recorded in the catalog.

const RETENTION = "retention_replay_needed";
const TIMEOUTS = "voice_dispatch_timeouts";
const STUCKS = "voice_watchdog_stucks";
const RECOVERIES = "voice_halt_recoveries";
const OPENS = "rest_breaker_opens";
const PAUSES = "rest_global_pauses";
const RETENTION_SERIES = "two_bot_retention_replay_needed_total";
const TIMEOUTS_SERIES = "two_bot_voice_dispatch_timeouts_total";
const STUCKS_SERIES = "two_bot_voice_watchdog_stucks_total";
const RECOVERIES_SERIES = "two_bot_voice_halt_recoveries_total";
const OPENS_SERIES = "two_bot_rest_guard_breaker_opens_total";
const PAUSES_SERIES = "two_bot_rest_guard_global_pauses_total";
const CLOSES_SERIES = "two_bot_rest_guard_breaker_closes_total";
const NOW = 1_000_000;

const line = (series: string, value: number | string) => `${series} ${value}`;
const ev = (lines: string[], state: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(lines.join("\n")), state, NOW);

const counterCases = [
  { id: RETENTION, series: RETENTION_SERIES, get: (s: MetricsAlertState) => s.retentionReplayNeeded },
  { id: "gateway_drain_skips", series: "two_bot_gateway_drain_skips_total", get: (s: MetricsAlertState) => s.gatewayDrainSkips },
  { id: TIMEOUTS, series: TIMEOUTS_SERIES, get: (s: MetricsAlertState) => s.voiceDispatchTimeouts },
  { id: STUCKS, series: STUCKS_SERIES, get: (s: MetricsAlertState) => s.voiceWatchdogStucks },
  { id: RECOVERIES, series: RECOVERIES_SERIES, get: (s: MetricsAlertState) => s.voiceHaltRecoveries },
  { id: OPENS, series: OPENS_SERIES, get: (s: MetricsAlertState) => s.restBreakerOpens },
  { id: PAUSES, series: PAUSES_SERIES, get: (s: MetricsAlertState) => s.restGlobalPauses },
] as const;

function headingAnchors(text: string): Set<string> {
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")));
}

test("catalog: rule ids match the Rust evidence list, preserve append positions and severities", () => {
  const evidence = readFileSync(new URL("../../crates/core/src/evidence.rs", import.meta.url), "utf8");
  const rustIds = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(evidence);
  assert.ok(rustIds, "ALERT_RULE_IDS not found in crates/core/src/evidence.rs");
  assert.deepEqual(RULES.map((r) => r.id), [...rustIds[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!));
  // The 29-rule historical prefix ends with the prefix-verdict ticket; the
  // voice-stall quartet appends at 29-32 and the six safety-series rules
  // stack after it at 33-38 without displacing anything.
  assert.equal(RULES[28]?.id, "prefix_trigger_verdict_refusals");
  assert.equal(RULES[29]?.id, "voice_worker_halted");
  assert.equal(RULES[30]?.id, "voice_live_self_heal");
  assert.equal(RULES[31]?.id, "voice_live_not_ready");
  assert.equal(RULES[32]?.id, "voice_queue_stall");
  assert.equal(RULES[33]?.id, RETENTION);
  assert.equal(RULES[34]?.id, TIMEOUTS);
  assert.equal(RULES[35]?.id, STUCKS);
  assert.equal(RULES[36]?.id, RECOVERIES);
  assert.equal(RULES[37]?.id, OPENS);
  assert.equal(RULES[38]?.id, PAUSES);
  for (const id of [RETENTION, TIMEOUTS, STUCKS, RECOVERIES, OPENS, PAUSES]) {
    assert.equal(RULES.filter((r) => r.id === id).length, 1, `${id} is catalogued exactly once`);
    assert.equal(
      packetFilename(id, "2026-10-09T20-11-06Z"),
      `evidence-${id}-2026-10-09T20-11-06Z.json`,
    );
  }
  assert.equal(ruleFor(RETENTION)?.severity, "ticket", "a skipped purge awaiting replay tickets");
  assert.equal(ruleFor(TIMEOUTS)?.severity, "ticket", "one slow turn tickets, never pages");
  assert.equal(ruleFor(STUCKS)?.severity, "ticket", "one stuck window tickets, never pages");
  assert.equal(ruleFor(RECOVERIES)?.severity, "ticket", "a covered halt tickets, never pages");
  assert.equal(ruleFor(OPENS)?.severity, "ticket", "a breaker trip tickets like its sustained-open sibling");
  assert.equal(ruleFor(PAUSES)?.severity, "ticket", "a Discord global pause tickets, never pages");
  // Tickets about lifecycle counters must never fail a promotion stage.
  for (const id of [RETENTION, TIMEOUTS, STUCKS, RECOVERIES, OPENS, PAUSES]) {
    assert.deepEqual(evaluatePromotionGate([id]), { fail: false, reasons: [] }, `${id} stays out of the gate`);
  }
});

test("runbook and metrics docs name each rule, its anchor and its next action", () => {
  const cases = [
    { id: RETENTION, anchor: "alert-retention-replay-needed", heading: "Alert: retention replay needed", keyword: /Operator replay/ },
    { id: TIMEOUTS, anchor: "alert-voice-dispatch-timeouts", heading: "Alert: voice dispatch timeouts", keyword: /observe bound/ },
    { id: STUCKS, anchor: "alert-voice-watchdog-stucks", heading: "Alert: voice watchdog stucks", keyword: /with no progress/ },
    { id: RECOVERIES, anchor: "alert-voice-halt-recoveries", heading: "Alert: voice halt recoveries", keyword: /CredentialRefused/ },
    { id: OPENS, anchor: "alert-rest-breaker-opens", heading: "Alert: REST breaker opens", keyword: /discord_breaker_open/ },
    { id: PAUSES, anchor: "alert-rest-global-pauses", heading: "Alert: REST global pauses", keyword: /discord_global_pause/ },
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
    assert.ok(metrics.includes(`\`${id}\``), `docs/metrics.md must name ${id}`);
  }
});

test("scrape validation: the six consumed families are optional-when-present and label-free", () => {
  const families = [RETENTION_SERIES, TIMEOUTS_SERIES, STUCKS_SERIES, RECOVERIES_SERIES, OPENS_SERIES, PAUSES_SERIES];
  for (const family of families) {
    assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, family), false,
      `${family}: older containers without the family must still scrape`);
    assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[family], {},
      `${family}: optional input with no labels`);
    assert.ok(metricsBody().includes(`${family} 0`), `fixture emits ${family}`);
  }
  const full = metricsBody();
  assert.ok(parseAlertScrape(full), "current exposition parses");
  const older = metricsLines()
    .filter((l) => !families.some((family) => l.startsWith(`${family} `) || l.startsWith(`${family}{`)))
    .join("\n");
  assert.ok(parseAlertScrape(older), "older exposition without the safety-series family still parses");
  // Present but malformed fails the whole scrape: a bad row must never
  // silently hold a rule off or on.
  for (const value of ["NaN", "+Inf", "-1", "0.5", "garbage", "1e20"]) {
    for (const family of families) {
      assert.equal(parseAlertScrape(full.replace(`${family} 0`, `${family} ${value}`)), null,
        `${family}: ${value}`);
    }
  }
  assert.equal(parseAlertScrape(full.replace(`${OPENS_SERIES} 0`, `${OPENS_SERIES}{route="other"} 0`)), null,
    "a labeled row is not the label-free counter");
  assert.equal(parseAlertScrape(`${full}${PAUSES_SERIES} 1\n`), null, "duplicate rows are rejected");
});

test("breaker closes stay unconsumed: cooling alone never fires any rule", () => {
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, CLOSES_SERIES), false);
  assert.equal(Object.hasOwn(ALERT_OPTIONAL_INPUT_LABELS, CLOSES_SERIES), false);
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  assert.ok(!rules.includes(`gauge("${CLOSES_SERIES}")`), "no rule may consume the deferred closes family");
  const closes = (n: number) => [`${CLOSES_SERIES} ${n}`];
  let state = ev(closes(0)).state;
  for (const n of [1, 2, 5] as const) {
    state = ev(closes(n), state).state;
    assert.deepEqual(state.firing, [], `closes=${n} stays silent`);
  }
});

for (const { id, series, get } of counterCases) {
  test(`${id} tickets on any increase, never on the first sample or a reset`, () => {
    const count = (n: number | string) => [line(series, n)];
    // First sample only stores the baseline, even with a large nonzero count.
    assert.deepEqual(ev(count(0)).firing, []);
    const baseline = ev(count(10_000));
    assert.deepEqual(baseline.firing, []);
    assert.equal(get(baseline.state), 10_000);
    // Any increase tickets, however small; each increase is one window verdict.
    const fired = ev(count(10_001), baseline.state);
    assert.deepEqual(fired.firing, [id]);
    const ticket = transitionMessages(baseline.firing, fired.firing)[0]!;
    assert.match(ticket, new RegExp(`ALERT ${id} \\(ticket\\):`));
    assert.ok(ticket.includes(runbookUrl(ruleFor(id)!)), "ticket carries the runbook deep link");
    const flat = ev(count(10_001), fired.state);
    assert.deepEqual(flat.firing, [], "a flat window is a quiet window, not a held ticket");
    assert.match(transitionMessages(fired.firing, flat.firing)[0]!, new RegExp(`RESOLVED ${id}\\.`));
    assert.deepEqual(ev(count(10_005), flat.state).firing, [id], "the next increase tickets again");
    // A counter that went backwards means the process restarted: re-baseline silently.
    const restart = ev(count(2), fired.state);
    assert.deepEqual(restart.firing, []);
    assert.equal(get(restart.state), 2);
    assert.deepEqual(ev(count(3), restart.state).firing, [id], "post-restart growth tickets");
  });
}

test("a healthy guild exposition fires no rule at all", () => {
  const samples = parseAlertScrape(metricsBody());
  assert.ok(samples, "healthy fixture parses");
  assert.deepEqual(evaluateMetrics(samples, EMPTY_STATE, NOW).firing, []);
});

for (const { id, series, get } of counterCases) {
  test(`${id}: a missing sample keeps the baseline but posts no window verdict`, () => {
    const count = (n: number | string) => [line(series, n)];
    const baseline = ev(count(5));
    const fired = ev(count(6), baseline.state);
    assert.deepEqual(fired.firing, [id]);
    // No window, no verdict: the ticket lifecycle is per increase, so a gap
    // carries no evidence either way and the baseline survives it.
    for (const gap of [[], [line(series, "NaN")]] as const) {
      const unknown = ev([...gap], fired.state);
      assert.deepEqual(unknown.firing, []);
      assert.equal(get(unknown.state), 6, "the gap must not corrupt the baseline");
      assert.deepEqual(ev(count(7), unknown.state).firing, [id], "growth after the gap tickets");
      assert.deepEqual(ev(count(6), unknown.state).firing, [], "flat after the gap stays silent");
    }
  });
}

for (const { id, series, get } of counterCases) {
  const invalidSamples: Record<string, string[]> = {
    "absent family": [],
    "NaN": [line(series, "NaN")],
    "infinity": [line(series, "+Inf")],
    "negative": [line(series, -1)],
    "fractional": [line(series, 1.5)],
    "unsafe integer": [line(series, "9007199254740992")],
    "duplicate row": [line(series, 7), line(series, 7)],
    "extra label": [`${series}{guild="1"} 7`],
  };
  for (const [name, lines] of Object.entries(invalidSamples)) {
    test(`${id}: invalid sample (${name}) never fires and never corrupts the baseline`, () => {
      assert.deepEqual(ev(lines).firing.filter((k) => k === id), [],
        "missing evidence is not a firing reason");
      const fired = ev([line(series, 7)], ev([line(series, 6)]).state);
      assert.deepEqual(fired.firing, [id]);
      const unknown = ev(lines, fired.state);
      assert.deepEqual(unknown.firing.filter((k) => k === id), [],
        "missing evidence posts no window verdict");
      assert.equal(get(unknown.state), 7, "the gap must not corrupt the baseline");
    });
  }
}

test("safety-series state survives DO reconstruction and reads legacy storage without the new fields", () => {
  const count = (n: number) => [line(TIMEOUTS_SERIES, n)];
  // A baseline round-tripped through JSON keeps judging windows.
  let state = ev(count(40)).state;
  state = ev(count(40), JSON.parse(JSON.stringify(state))).state;
  assert.deepEqual(state.firing, []);
  assert.deepEqual(ev(count(41), JSON.parse(JSON.stringify(state))).firing, [TIMEOUTS]);
  // Storage written before these rules existed has none of their fields: the
  // first counter sample is a baseline, never a firing.
  const legacy = {
    firing: [] as string[], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0,
    sendBlocked: 0, sendBlockedStreak: 0, voiceOps: 0, voiceFailures: 0,
    voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {},
  } as MetricsAlertState;
  for (const { id, series } of counterCases) {
    assert.deepEqual(ev([line(series, 7)], legacy).firing.filter((k) => k === id), [],
      `${id}: legacy storage baselines without firing`);
  }
});

test("the new rules do not disturb unrelated rules sharing the same scrape", () => {
  const unrelated = [`two_bot_job_consecutive_failures{job="counter"} 3`];
  const base = ev(
    [line(RETENTION_SERIES, 9), line(OPENS_SERIES, 4), ...unrelated],
  );
  assert.deepEqual([...base.firing].sort(), ["job_consecutive_failures:counter"].sort());
  const fired = ev(
    [line(RETENTION_SERIES, 10), line(OPENS_SERIES, 4), ...unrelated],
    base.state,
  );
  assert.deepEqual([...fired.firing].sort(),
    ["job_consecutive_failures:counter", RETENTION].sort());
});
