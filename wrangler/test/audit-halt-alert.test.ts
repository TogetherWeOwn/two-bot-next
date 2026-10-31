import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, evaluatePromotionGate, packetFilename,
  parseExposition, ruleFor, runbookUrl, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import {
  ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape,
} from "../src/metrics-scrape.ts";
import { metricsBody } from "./fixtures/metrics.ts";

/**
 * Acceptance contract for the audit-delivery halt alert.
 *
 * Source facts these tests encode:
 * - `two_bot_audit_delivery_halt` is a label-free gauge: `1` while the
 *   persistent audit kill switch is engaged, `0` once cleared, set once per
 *   readable `audit_retry` sweep (docs/metrics.md). An unreadable sweep
 *   produces no new gauge reading, so at this layer it is indistinguishable
 *   from an absent series: it must neither fire nor recover.
 * - An engaged switch left on must raise one ticket instead of waiting for a
 *   human to notice the series by hand; a brief/transient halt stays quiet.
 * - The evaluator's transient filter for streak rules (db pool saturation,
 *   send-admission blocked, receiver refusals, dispatch drops) is three
 *   consecutive qualifying samples at the keepalive cadence; the halt rule
 *   uses the same sustained-run shape and ticket severity.
 * - No audit content, halt-reason text or free-text labels may ever reach a
 *   firing key or a transition message.
 */

const KEY = "audit_delivery_halt";
const SERIES = "two_bot_audit_delivery_halt";
const START = 1_000_000;

const halt = (value: number | string): string[] => [`${SERIES} ${value}`];

/** Feed samples at a 60 s keepalive cadence so streak- and time-shaped
 * evaluators both see a realistic sequence. */
function run(samples: string[][], state: MetricsAlertState = EMPTY_STATE, start = START) {
  const results: ReturnType<typeof evaluateMetrics>[] = [];
  let current = state;
  samples.forEach((lines, i) => {
    const result = evaluateMetrics(parseExposition(lines.join("\n")), current, start + i * 60);
    results.push(result);
    current = result.state;
  });
  return results;
}

function firingState(): MetricsAlertState {
  const results = run([halt(1), halt(1), halt(1)]);
  assert.deepEqual(results.at(-1)!.firing, [KEY], "precondition: sustained halt is firing");
  return results.at(-1)!.state;
}

/** GitHub-style heading slugs for a doc's text; literal underscores survive. */
function headingAnchors(text: string): Set<string> {
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")));
}

test("audit halt rule is catalogued as a ticket with the shared spelling, validated scrape input and a real runbook anchor", () => {
  const rule = ruleFor(KEY);
  assert.ok(rule, `RULES has no ${KEY} entry`);
  assert.ok(RULES.some((r) => r.id === KEY));
  assert.equal(rule?.severity, "ticket", "an engaged halt tickets; it never pages");
  assert.match(rule?.summary ?? "", /audit/i);
  // Fired packets share one rule-id spelling with the Rust evidence catalog.
  assert.equal(packetFilename(KEY, "2026-10-09T20-11-06Z"), "evidence-audit_delivery_halt-2026-10-09T20-11-06Z.json");
  const rust = readFileSync(new URL("../../crates/core/src/evidence.rs", import.meta.url), "utf8");
  const ids = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(rust);
  assert.ok(ids);
  assert.ok(
    [...ids[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!).includes(KEY),
    "Rust ALERT_RULE_IDS lacks the shared audit halt spelling",
  );
  // The series is a validated label-free scrape input (required or
  // optional-when-present), so a labeled row cannot smuggle free text in.
  assert.deepEqual(
    ALERT_INPUT_LABELS[SERIES] ?? ALERT_OPTIONAL_INPUT_LABELS[SERIES],
    {},
    `${SERIES} is not registered as a label-free alert input`,
  );
  // The packet runbook link resolves to a checked-in heading (the anchor
  // documents intended-vs-stuck diagnosis and the unhalt procedure).
  const anchors = headingAnchors(readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8"));
  assert.ok(anchors.has(rule!.runbook.split("#")[1]!), `missing runbook heading for ${rule!.runbook}`);
  // A halt ticket is never a promotion rollback trigger.
  assert.deepEqual(evaluatePromotionGate([KEY]), { fail: false, reasons: [] });
});

test("sustained halt tickets on the third consecutive engaged sample and notifies once", () => {
  const [first, second, third, steady] = run([halt(1), halt(1), halt(1), halt(1)]);
  assert.deepEqual(first!.firing, [], "a single engaged sample is not yet a sustained halt");
  assert.deepEqual(second!.firing, []);
  assert.deepEqual(third!.firing, [KEY]);
  assert.deepEqual(steady!.firing, [KEY], "the ticket stays open while the switch stays engaged");

  const rule = ruleFor(KEY);
  assert.ok(rule);
  const fired = transitionMessages(second!.firing, third!.firing);
  assert.equal(fired.length, 1, "exactly one ticket transition");
  assert.match(fired[0]!, new RegExp(`ALERT ${KEY} \\(ticket\\):`));
  assert.ok(fired[0]!.includes(`Runbook: ${runbookUrl(rule)}`), "ticket carries its runbook deep link");
  assert.deepEqual(transitionMessages(third!.firing, steady!.firing), [], "no re-notification while steady");
});

test("transient halt stays quiet", () => {
  // Engaged then cleared inside the sustained bound: never a ticket.
  for (const sequence of [[1, 0], [1, 1, 0], [1, 0, 1, 0]] as const) {
    const results = run(sequence.map(halt));
    assert.deepEqual(results.map((r) => r.firing), sequence.map(() => []), `sequence ${sequence}`);
  }
  // A cleared switch that was never sustained produces no resolved message.
  const quiet = run([halt(1), halt(0)]);
  assert.deepEqual(transitionMessages(quiet[0]!.firing, quiet[1]!.firing), []);
});

test("a cleared sample resets the sustained run", () => {
  // 1,1,0,1,1 is still quiet, and only the third consecutive engaged
  // sample after the break tickets.
  const results = run([1, 1, 0, 1, 1, 1].map(halt));
  assert.deepEqual(results.slice(0, 5).map((r) => r.firing), [[], [], [], [], []]);
  assert.deepEqual(results.at(-1)!.firing, [KEY]);
});

test("recovery resolves once and a later halt needs a fresh sustained run", () => {
  const firing = firingState();
  const [recovered, stillClear] = run([halt(0), halt(0)], firing, START + 180);
  assert.deepEqual(recovered!.firing, [], "gauge back to 0 recovers");
  const resolved = transitionMessages([KEY], recovered!.firing);
  assert.equal(resolved.length, 1, "exactly one recovery transition");
  assert.match(resolved[0]!, new RegExp(`RESOLVED ${KEY}\\.`));
  assert.deepEqual(transitionMessages(recovered!.firing, stillClear!.firing), [], "recovery notifies once only");

  const again = run([halt(1), halt(1)], stillClear!.state, START + 300);
  assert.deepEqual(again.map((r) => r.firing), [[], []], "a re-engaged switch must re-earn the ticket");
  assert.deepEqual(run([halt(1)], again.at(-1)!.state, START + 420)[0]!.firing, [KEY]);
});

test("missing, labeled and out-of-domain samples never fire from a fresh evaluator", () => {
  // Only valid engaged readings count toward a sustained run.
  assert.deepEqual(run([[], [], []]).map((r) => r.firing), [[], [], []]);
  assert.deepEqual(run([halt(2), halt(2), halt(2)]).map((r) => r.firing), [[], [], []], "out-of-domain values are not engagement");
  assert.deepEqual(run([halt(1), [], halt(1)]).map((r) => r.firing), [[], [], []], "an absent sample does not count toward the sustained run");
  // The first sample a fresh evaluator ever sees cannot ticket by itself.
  assert.deepEqual(run([halt(1)])[0]!.firing, []);
});

test("missing or malformed samples never falsely recover a firing ticket", () => {
  const firing = firingState();
  // An unreadable sweep surfaces here as no new reading: absent series,
  // duplicated rows, a labeled row, NaN and an out-of-domain value (the
  // gauge contract is exactly 0 or 1) all hold the active ticket instead
  // of manufacturing a recovery.
  const held: string[][] = [
    [],
    halt("NaN"),
    [...halt(1), ...halt(1)],
    [`${SERIES}{reason="operator"} 0`],
    halt(2),
  ];
  held.forEach((lines, i) => {
    const result = run([lines], firing, START + 180 + i * 60)[0]!;
    assert.deepEqual(result.firing, [KEY], `malformed sample ${JSON.stringify(lines)} must hold the ticket`);
  });
  // A real recovery is still seen after held samples.
  assert.deepEqual(run([halt(0)], firing, START + 600)[0]!.firing, []);
});

test("streak and firing state survive a storage round-trip", () => {
  const partial = run([halt(1), halt(1)]);
  const reconstructed = JSON.parse(JSON.stringify(partial.at(-1)!.state)) as MetricsAlertState;
  const third = run([halt(1)], reconstructed, START + 120)[0]!;
  assert.deepEqual(third.firing, [KEY], "the sustained run persists across reconstruction");
  const rebuiltFiring = JSON.parse(JSON.stringify(third.state)) as MetricsAlertState;
  assert.deepEqual(run([halt(1)], rebuiltFiring, START + 180)[0]!.firing, [KEY]);
  assert.deepEqual(run([halt(0)], rebuiltFiring, START + 180)[0]!.firing, [], "reconstructed state still recovers");
});

test("labels and free text on the halt series never fire and never reach a message", () => {
  const hostile = "example.invalid/member/123?halt=operator-note";
  const labeled = run([
    [`${SERIES}{reason="https://${hostile}"} 1`],
    [`${SERIES}{reason="https://${hostile}"} 1`],
    [`${SERIES}{reason="https://${hostile}"} 1`],
  ]);
  assert.deepEqual(labeled.map((r) => r.firing), [[], [], []], "a labeled row is malformed for a label-free gauge");
  const fired = transitionMessages([], [KEY])[0] ?? "";
  assert.doesNotMatch(fired, /example\.invalid|member\/123|operator-note|reason=/);
});

test("a full validated scrape carries the halt series into the evaluator label-free", () => {
  const base = metricsBody().split("\n").filter((line) => !line.startsWith(SERIES));
  const first = parseAlertScrape([...base, `${SERIES} 1`].join("\n"));
  assert.ok(first, "a complete exposition with the halt gauge parses");
  const haltSamples = first?.filter((s) => s.name === SERIES) ?? [];
  assert.equal(haltSamples.length, 1, "the halt sample reaches the evaluator");
  assert.equal(haltSamples[0]!.value, 1);
  assert.deepEqual(Object.keys(haltSamples[0]!.labels), [], "the halt sample is label-free");
});

test("sustained halt tickets end to end from a real scrape shape and a labeled scrape is rejected whole", () => {
  const hostile = "example.invalid/member/123?halt=operator-note";
  const base = metricsBody().split("\n").filter((line) => !line.startsWith(SERIES));
  // A scrape carrying a labeled halt row is rejected whole, before any
  // state write, rather than evaluated with the label intact.
  assert.equal(parseAlertScrape([...base, `${SERIES}{reason="https://${hostile}"} 1`].join("\n")), null);
  const scrape = (value: number) => parseAlertScrape([...base, `${SERIES} ${value}`].join("\n"));
  let state = EMPTY_STATE;
  const firings: string[][] = [];
  for (let i = 0; i < 3; i++) {
    const result = evaluateMetrics(scrape(1)!, state, START + i * 60);
    firings.push(result.firing.filter((k) => k === KEY));
    state = result.state;
  }
  assert.deepEqual(firings, [[], [], [KEY]], "sustained halt tickets end to end from a real scrape shape");
});

test("halt recovery resolves only the halt key beside an unrelated firing rule", () => {
  const staleRank = `two_bot_job_last_success_timestamp_seconds{job="rank"} ${START - 1201}`;
  const engaged = run([
    [...halt(1), staleRank],
    [...halt(1), staleRank],
    [...halt(1), staleRank],
  ]);
  assert.deepEqual([...engaged.at(-1)!.firing].sort(), [KEY, "job_stale:rank"].sort());
  const recovered = run([[...halt(0), staleRank]], engaged.at(-1)!.state, START + 180)[0]!;
  assert.deepEqual(recovered.firing, ["job_stale:rank"], "clearing the halt leaves unrelated alerts firing");
});
