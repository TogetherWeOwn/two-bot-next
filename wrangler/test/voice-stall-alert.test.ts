import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, VOICE_LIVE_NOT_READY_THRESHOLD_SECONDS,
  VOICE_QUEUE_STALL_THRESHOLD_SECONDS, evaluateMetrics, packetFilename,
  parseExposition, ruleFor, runbookUrl, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines } from "./fixtures/metrics.ts";

// Acceptance contract: the five emitted voice stall series in
// docs/observability-event-catalog.md ("Consumed by no alert rule") get
// pipeline coverage. Four are consumed here: the halt latch pages while 1,
// the self-heal counter tickets on any increase, the live-not-ready age
// pages past the host-watchdog 120 s bound, and the queue oldest age
// tickets past the in-process stuck bound. Queue depth
// (`two_bot_voice_queue_pending{lane}`) is deliberately unconsumed: depth
// without age is not a failure signal, so it stays a manual complement
// with its deferral recorded in the catalog.

const HALTED = "voice_worker_halted";
const SELF_HEAL = "voice_live_self_heal";
const NOT_READY = "voice_live_not_ready";
const STALL = "voice_queue_stall";
const HALTED_SERIES = "two_bot_voice_worker_halted";
const SELF_HEAL_SERIES = "two_bot_voice_live_self_heal_total";
const NOT_READY_SERIES = "two_bot_voice_live_not_ready_seconds";
const STALL_SERIES = "two_bot_voice_queue_oldest_age_seconds";
const DEPTH_SERIES = "two_bot_voice_queue_pending";
const NOW = 1_000_000;

const line = (series: string, value: number | string) => `${series} ${value}`;
const ev = (lines: string[], state: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(lines.join("\n")), state, NOW);

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
  // four stall rules append after it without displacing anything.
  assert.equal(RULES[28]?.id, "prefix_trigger_verdict_refusals");
  assert.equal(RULES[29]?.id, HALTED);
  assert.equal(RULES[30]?.id, SELF_HEAL);
  assert.equal(RULES[31]?.id, NOT_READY);
  assert.equal(RULES[32]?.id, STALL);
  for (const id of [HALTED, SELF_HEAL, NOT_READY, STALL]) {
    assert.equal(RULES.filter((r) => r.id === id).length, 1, `${id} is catalogued exactly once`);
    assert.equal(
      packetFilename(id, "2026-10-09T20-11-06Z"),
      `evidence-${id}-2026-10-09T20-11-06Z.json`,
    );
  }
  assert.equal(ruleFor(HALTED)?.severity, undefined, "a halted guild actor pages");
  assert.equal(ruleFor(SELF_HEAL)?.severity, "ticket", "a covered fault tickets, never pages");
  assert.equal(ruleFor(NOT_READY)?.severity, undefined, "dropped joins past the watchdog bound page");
  assert.equal(ruleFor(STALL)?.severity, "ticket", "an aged queue tickets, never pages");
});

test("runbook and metrics docs name each rule, its anchor and its next action", () => {
  const cases = [
    { id: HALTED, anchor: "alert-voice-worker-halted", heading: "Alert: voice worker halted", keyword: /CredentialRefused/ },
    { id: SELF_HEAL, anchor: "alert-voice-live-self-heal", heading: "Alert: voice live self-heal", keyword: /republished/ },
    { id: NOT_READY, anchor: "alert-voice-live-not-ready", heading: "Alert: voice live not ready", keyword: /restart/ },
    { id: STALL, anchor: "alert-voice-queue-stall", heading: "Alert: voice queue stall", keyword: /watchdog/ },
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

test("scrape validation: the four consumed families are optional-when-present and label-free", () => {
  for (const family of [HALTED_SERIES, SELF_HEAL_SERIES, NOT_READY_SERIES, STALL_SERIES]) {
    assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, family), false,
      `${family}: older containers without the family must still scrape`);
    assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[family], {},
      `${family}: optional input with no labels`);
    assert.ok(metricsBody().includes(`${family} 0`), `fixture emits ${family}`);
  }
  const full = metricsBody();
  assert.ok(parseAlertScrape(full), "current exposition parses");
  assert.ok(parseAlertScrape(full.replace(`${HALTED_SERIES} 0`, `${HALTED_SERIES} 1`)));
  const older = metricsLines()
    .filter((l) => ![HALTED_SERIES, SELF_HEAL_SERIES, NOT_READY_SERIES, STALL_SERIES]
      .some((family) => l.startsWith(`${family} `) || l.startsWith(`${family}{`)))
    .join("\n");
  assert.ok(parseAlertScrape(older), "older exposition without the stall family still parses");
  // Present but malformed fails the whole scrape: a bad row must never
  // silently hold a rule off or on.
  for (const value of ["NaN", "+Inf", "-1", "0.5", "garbage", "1e20"]) {
    for (const family of [SELF_HEAL_SERIES, NOT_READY_SERIES, STALL_SERIES]) {
      assert.equal(parseAlertScrape(full.replace(`${family} 0`, `${family} ${value}`)), null,
        `${family}: ${value}`);
    }
  }
  for (const value of ["2", "-1", "0.5", "NaN", "+Inf"]) {
    assert.equal(parseAlertScrape(full.replace(`${HALTED_SERIES} 0`, `${HALTED_SERIES} ${value}`)), null,
      `${HALTED_SERIES} is a 0/1 latch: ${value}`);
  }
  assert.equal(parseAlertScrape(full.replace(`${HALTED_SERIES} 0`, `${HALTED_SERIES}{guild="1"} 0`)), null,
    "a labeled row is not the label-free latch");
  assert.equal(parseAlertScrape(`${full}${HALTED_SERIES} 1\n`), null, "duplicate rows are rejected");
});

test("queue depth stays unconsumed: rising depths alone never fire any rule", () => {
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, DEPTH_SERIES), false);
  assert.equal(Object.hasOwn(ALERT_OPTIONAL_INPUT_LABELS, DEPTH_SERIES), false);
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  assert.ok(!rules.includes(`gauge("${DEPTH_SERIES}")`), "no rule may consume the deferred depth family");
  const depth = (urgent: number, deferred: number) => [
    `${DEPTH_SERIES}{lane="urgent"} ${urgent}`,
    `${DEPTH_SERIES}{lane="deferred"} ${deferred}`,
  ];
  let state = ev(depth(0, 0)).state;
  for (const [urgent, deferred] of [[10, 0], [10, 50], [1000, 5000]] as const) {
    state = ev(depth(urgent, deferred), state).state;
    assert.deepEqual(state.firing, [], `depth urgent=${urgent} deferred=${deferred} stays silent`);
  }
});

test("halted latch pages while 1 and clears on a valid 0", () => {
  // Level-triggered: the first 1 reading pages without a baseline sample.
  const firing = ev([line(HALTED_SERIES, 1)]);
  assert.deepEqual(firing.firing, [HALTED]);
  const held = ev([line(HALTED_SERIES, 1)], firing.state);
  assert.deepEqual(held.firing, [HALTED]);
  assert.deepEqual(transitionMessages(firing.firing, held.firing), [], "holding is not a new ALERT");
  const page = transitionMessages([], firing.firing)[0]!;
  assert.match(page, new RegExp(`ALERT ${HALTED}:`));
  assert.doesNotMatch(page, /\(ticket\)/, "a halted actor pages");
  assert.ok(page.includes(runbookUrl(ruleFor(HALTED)!)), "page carries the runbook deep link");
  const cleared = ev([line(HALTED_SERIES, 0)], held.state);
  assert.deepEqual(cleared.firing, []);
  const resolved = transitionMessages(held.firing, cleared.firing);
  assert.equal(resolved.length, 1);
  assert.match(resolved[0]!, new RegExp(`RESOLVED ${HALTED}\\. Build: unknown\\. Logs: `));
  // A healthy latch never fires, however many quiet samples pass.
  let quiet = ev([line(HALTED_SERIES, 0)]);
  assert.deepEqual(quiet.firing, []);
  quiet = ev([line(HALTED_SERIES, 0)], quiet.state);
  assert.deepEqual(quiet.firing, []);
});

test("self-heal counter tickets on any increase, never on the first sample or a reset", () => {
  const heal = (n: number | string) => [line(SELF_HEAL_SERIES, n)];
  // First sample only stores the baseline, even with a large nonzero count.
  assert.deepEqual(ev(heal(0)).firing, []);
  const baseline = ev(heal(10_000));
  assert.deepEqual(baseline.firing, []);
  assert.equal(baseline.state.voiceLiveSelfHeal, 10_000);
  // Any increase tickets, however small; each increase is one window verdict.
  const fired = ev(heal(10_001), baseline.state);
  assert.deepEqual(fired.firing, [SELF_HEAL]);
  const ticket = transitionMessages(baseline.firing, fired.firing)[0]!;
  assert.match(ticket, new RegExp(`ALERT ${SELF_HEAL} \\(ticket\\):`));
  assert.ok(ticket.includes(runbookUrl(ruleFor(SELF_HEAL)!)), "ticket carries the runbook deep link");
  const flat = ev(heal(10_001), fired.state);
  assert.deepEqual(flat.firing, [], "a flat window is a quiet window, not a held ticket");
  assert.match(transitionMessages(fired.firing, flat.firing)[0]!, new RegExp(`RESOLVED ${SELF_HEAL}\\.`));
  assert.deepEqual(ev(heal(10_005), flat.state).firing, [SELF_HEAL], "the next increase tickets again");
  // A counter that went backwards means the process restarted: re-baseline silently.
  const restart = ev(heal(2), fired.state);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.voiceLiveSelfHeal, 2);
  assert.deepEqual(ev(heal(3), restart.state).firing, [SELF_HEAL], "post-restart growth tickets");
});

test("live-not-ready pages past 120 s and clears at or below it", () => {
  assert.equal(VOICE_LIVE_NOT_READY_THRESHOLD_SECONDS, 120, "promotes the host-watchdog bound");
  const age = (n: number | string) => [line(NOT_READY_SERIES, n)];
  assert.deepEqual(ev(age(0)).firing, [], "a ready view never fires");
  assert.deepEqual(ev(age(119)).firing, []);
  assert.deepEqual(ev(age(120)).firing, [], "at the bound is not past it");
  const firing = ev(age(121));
  assert.deepEqual(firing.firing, [NOT_READY]);
  const page = transitionMessages([], firing.firing)[0]!;
  assert.match(page, new RegExp(`ALERT ${NOT_READY}:`));
  assert.doesNotMatch(page, /\(ticket\)/, "dropped joins past the watchdog bound page");
  assert.ok(page.includes(runbookUrl(ruleFor(NOT_READY)!)), "page carries the runbook deep link");
  const climbing = ev(age(3600), firing.state);
  assert.deepEqual(climbing.firing, [NOT_READY]);
  assert.deepEqual(transitionMessages(firing.firing, climbing.firing), []);
  const cleared = ev(age(0), climbing.state);
  assert.deepEqual(cleared.firing, []);
  assert.match(transitionMessages(climbing.firing, cleared.firing)[0]!, new RegExp(`RESOLVED ${NOT_READY}\\.`));
  // A recovered-then-degraded view pages again without a new baseline.
  assert.deepEqual(ev(age(200), cleared.state).firing, [NOT_READY]);
});

test("queue stall tickets past 120 s and clears at or below it", () => {
  assert.equal(VOICE_QUEUE_STALL_THRESHOLD_SECONDS, 120, "matches the in-process stuck horizon");
  const age = (n: number | string) => [line(STALL_SERIES, n)];
  assert.deepEqual(ev(age(0)).firing, [], "an empty queue never fires");
  assert.deepEqual(ev(age(119)).firing, []);
  assert.deepEqual(ev(age(120)).firing, [], "at the bound is not past it");
  const firing = ev(age(121));
  assert.deepEqual(firing.firing, [STALL]);
  const ticket = transitionMessages([], firing.firing)[0]!;
  assert.match(ticket, new RegExp(`ALERT ${STALL} \\(ticket\\):`));
  assert.ok(ticket.includes(runbookUrl(ruleFor(STALL)!)), "ticket carries the runbook deep link");
  const cleared = ev(age(45), firing.state);
  assert.deepEqual(cleared.firing, []);
  assert.match(transitionMessages(firing.firing, cleared.firing)[0]!, new RegExp(`RESOLVED ${STALL}\\.`));
});

test("a healthy guild exposition fires no rule at all", () => {
  const samples = parseAlertScrape(metricsBody());
  assert.ok(samples, "healthy fixture parses");
  assert.deepEqual(evaluateMetrics(samples, EMPTY_STATE, NOW).firing, []);
});

const levelCases = [
  { id: HALTED, series: HALTED_SERIES, fire: "1", clear: "0" },
  { id: NOT_READY, series: NOT_READY_SERIES, fire: "121", clear: "0" },
  { id: STALL, series: STALL_SERIES, fire: "121", clear: "0" },
] as const;

for (const { id, series, fire, clear } of levelCases) {
  const invalidSamples: Record<string, string[]> = {
    "absent family": [],
    "NaN": [line(series, "NaN")],
    "infinity": [line(series, "+Inf")],
    "negative": [line(series, -1)],
    "fractional": [line(series, 1.5)],
    "unsafe integer": [line(series, "9007199254740992")],
    "duplicate row": [line(series, fire), line(series, fire)],
    "extra label": [`${series}{guild="1"} ${fire}`],
    ...(id === HALTED ? { "non-boolean latch": [line(series, 2)] } : {}),
  };
  for (const [name, lines] of Object.entries(invalidSamples)) {
    test(`${id}: invalid sample (${name}) never fires alone and never falsely resolves`, () => {
      assert.deepEqual(ev(lines).firing.filter((k) => k === id), [],
        "missing evidence is not a firing reason");
      const before = ev([line(series, fire)]);
      assert.deepEqual(before.firing, [id]);
      const unknown = ev(lines, before.state);
      assert.deepEqual(unknown.firing, [id], "missing evidence must not clear the incident");
      assert.deepEqual(transitionMessages(before.firing, unknown.firing), []);
      const quiet = ev([line(series, clear)], unknown.state);
      assert.deepEqual(quiet.firing, []);
      assert.match(transitionMessages(unknown.firing, quiet.firing)[0]!, new RegExp(`RESOLVED ${id}\\.`));
    });
  }
}

test("self-heal: a missing sample keeps the baseline but posts no window verdict", () => {
  const heal = (n: number | string) => [line(SELF_HEAL_SERIES, n)];
  const baseline = ev(heal(5));
  const fired = ev(heal(6), baseline.state);
  assert.deepEqual(fired.firing, [SELF_HEAL]);
  // No window, no verdict: a gap carries no evidence either way, so it holds
  // the firing ticket (never a RESOLVED) and the baseline survives it. Only
  // the next valid flat sample proves repairs stopped and resolves once.
  for (const gap of [[], [line(SELF_HEAL_SERIES, "NaN")]] as const) {
    const unknown = ev([...gap], fired.state);
    assert.deepEqual(unknown.firing, [SELF_HEAL], "a gap holds the ticket");
    assert.deepEqual(transitionMessages(fired.firing, unknown.firing), [], "a gap posts no verdict");
    assert.equal(unknown.state.voiceLiveSelfHeal, 6, "the gap must not corrupt the baseline");
    const regrown = ev(heal(7), unknown.state);
    assert.deepEqual(regrown.firing, [SELF_HEAL], "growth after the gap tickets");
    assert.deepEqual(transitionMessages(unknown.firing, regrown.firing), [], "held growth is not a new ALERT");
    const flat = ev(heal(6), unknown.state);
    assert.deepEqual(flat.firing, [], "flat after the gap resolves");
    const resolved = transitionMessages(unknown.firing, flat.firing);
    assert.equal(resolved.length, 1, "the first valid flat sample resolves exactly once");
    assert.match(resolved[0]!, new RegExp(`RESOLVED ${SELF_HEAL}\\.`));
  }
});

test("stall state survives DO reconstruction and reads legacy storage without the new fields", () => {
  const heal = (n: number) => [line(SELF_HEAL_SERIES, n)];
  // A baseline round-tripped through JSON keeps judging windows.
  let state = ev(heal(40)).state;
  state = ev(heal(40), JSON.parse(JSON.stringify(state))).state;
  assert.deepEqual(state.firing, []);
  assert.deepEqual(ev(heal(41), JSON.parse(JSON.stringify(state))).firing, [SELF_HEAL]);
  // Storage written before these rules existed has none of their fields: the
  // first counter sample is a baseline, while level latches judge at once.
  const legacy = {
    firing: [] as string[], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0,
    sendBlocked: 0, sendBlockedStreak: 0, voiceOps: 0, voiceFailures: 0,
    voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {},
  } as MetricsAlertState;
  assert.deepEqual(ev(heal(7), legacy).firing, []);
  assert.deepEqual(ev([line(HALTED_SERIES, 1)], legacy).firing, [HALTED]);
  assert.deepEqual(ev([line(NOT_READY_SERIES, 500)], legacy).firing, [NOT_READY]);
  assert.deepEqual(ev([line(STALL_SERIES, 500)], legacy).firing, [STALL]);
});

test("the new rules do not disturb unrelated rules sharing the same scrape", () => {
  const lines = [
    line(HALTED_SERIES, 1),
    line(NOT_READY_SERIES, 200),
    line(STALL_SERIES, 200),
    `two_bot_job_consecutive_failures{job="counter"} 3`,
  ];
  const base = ev([line(SELF_HEAL_SERIES, 9), ...lines]);
  assert.deepEqual([...base.firing].sort(),
    ["job_consecutive_failures:counter", HALTED, NOT_READY, STALL].sort());
  const fired = ev([line(SELF_HEAL_SERIES, 10), ...lines], base.state);
  assert.deepEqual([...fired.firing].sort(),
    ["job_consecutive_failures:counter", HALTED, SELF_HEAL, NOT_READY, STALL].sort());
});
