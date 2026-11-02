import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, evaluatePromotionGate, interruptDispatchDrops, packetFilename,
  parseExposition, ruleFor, runbookUrl, transitionMessages,
  EVENTSUB_RECEIVE_GAP_SAMPLES, VOICE_REST_STUCK_SECONDS, VOICE_REST_STUCK_SAMPLES,
  FEED_FETCH_ERROR_CLASSES, EVENTSUB_CLASSIFIED_KINDS, MODERATION_VERBS,
  MODERATION_FAILURE_OUTCOMES,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines, rustLabels } from "./fixtures/metrics.ts";

// Acceptance contract: the eleven emitted domain-outcome and voice-liveness
// series with zero alert-rule hits get pipeline coverage. Seven are consumed
// here, each a ticket: feed-fetch error growth, a sustained EventSub
// receive/classify gap, reconcile prior-failed findings, automod sanction
// refusals, moderation enforcement failures, a stuck voice REST attempt, and
// voice global-backoff caps. Four families stay deliberately unconsumed:
// feed fetch attempts (every non-success pairs 1:1 with an error increment,
// so the errors sum carries the same signal with finer reason codes),
// presence capture (no production producer yet; the family renders zeros
// until the event-store S3 cutover lands), join occupancy (distribution
// telemetry for capacity curves; no bucket is a fault), and reconcile
// actions (successful mutations; failures ticket through the failed-pass
// rule). Each deferral is recorded in the catalog, never silent.

const FEED = "feed_fetch_failures";
const GAP = "eventsub_receive_gap";
const PRIOR = "eventsub_reconcile_prior_failed";
const SANCTION = "automod_sanction_refused";
const MODERATION = "moderation_enforcement_failures";
const STUCK = "voice_rest_stuck";
const BACKOFF = "voice_global_backoff_capped";
const NEW_RULES = [FEED, GAP, PRIOR, SANCTION, MODERATION, STUCK, BACKOFF];
const FEED_SERIES = "two_bot_feed_fetch_errors_total";
const RECEIVED_SERIES = "two_bot_eventsub_received_total";
const CLASSIFIED_SERIES = "two_bot_eventsub_classified_total";
const RECONCILE_ERRORS_SERIES = "two_bot_eventsub_reconcile_errors_total";
const RECONCILE_PASSES_SERIES = "two_bot_eventsub_reconcile_passes_total";
const AUTOMOD_SERIES = "two_bot_automod_decisions_total";
const MODERATION_SERIES = "two_bot_moderation_enforcements_total";
const STUCK_SERIES = "two_bot_voice_rest_in_flight_age_seconds";
const BACKOFF_SERIES = "two_bot_voice_global_backoff_capped_total";
const FETCHES_SERIES = "two_bot_feed_fetches_total";
const PRESENCE_SERIES = "two_bot_presence_capture_total";
const OCCUPANCY_SERIES = "two_bot_voice_join_occupancy_total";
const ACTIONS_SERIES = "two_bot_eventsub_reconcile_actions_total";
const NOW = 1_000_000;

const ev = (lines: string[], state: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(lines.join("\n")), state, NOW);

const feedLines = (counts: Record<string, number>): string[] =>
  [...FEED_FETCH_ERROR_CLASSES].map((c) => `${FEED_SERIES}{error_class="${c}"} ${counts[c] ?? 0}`);

const classifiedLines = (counts: Record<string, number>): string[] =>
  [...EVENTSUB_CLASSIFIED_KINDS].map((k) => `${CLASSIFIED_SERIES}{kind="${k}"} ${counts[k] ?? 0}`);

const moderationLines = (counts: Record<string, number>): string[] => {
  const lines: string[] = [];
  for (const verb of [...MODERATION_VERBS]) {
    for (const outcome of [...MODERATION_FAILURE_OUTCOMES]) {
      lines.push(`${MODERATION_SERIES}{verb="${verb}",outcome="${outcome}"} ${counts[`${verb}/${outcome}`] ?? 0}`);
    }
  }
  return lines;
};

const reconcileWindow = (healthy: number, pending: number, failed: number, prior: number): string[] => [
  `${RECONCILE_PASSES_SERIES}{outcome="healthy"} ${healthy}`,
  `${RECONCILE_PASSES_SERIES}{outcome="pending"} ${pending}`,
  `${RECONCILE_PASSES_SERIES}{outcome="failed"} ${failed}`,
  `${RECONCILE_ERRORS_SERIES}{error_class="reconcile_prior_failed"} ${prior}`,
];

const counterCases = [
  { id: FEED, series: FEED_SERIES, build: (n: number) => feedLines({ transport: n }), get: (s: MetricsAlertState) => s.feedFetchErrors },
  { id: PRIOR, series: RECONCILE_ERRORS_SERIES, build: (n: number) => reconcileWindow(n, 0, 0, n), get: (s: MetricsAlertState) => s.reconcilePriorFailed },
  { id: SANCTION, series: AUTOMOD_SERIES, build: (n: number) => [`${AUTOMOD_SERIES}{outcome="sanction_refused"} ${n}`], get: (s: MetricsAlertState) => s.automodSanctionRefused },
  { id: MODERATION, series: MODERATION_SERIES, build: (n: number) => moderationLines({ "kick/discord_error": n }), get: (s: MetricsAlertState) => s.moderationEnforcementFailures },
  { id: BACKOFF, series: BACKOFF_SERIES, build: (n: number) => [`${BACKOFF_SERIES} ${n}`], get: (s: MetricsAlertState) => s.voiceBackoffCapped },
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
  // The 39-rule historical prefix ends with the global-pause ticket,
  // then the four gateway-transport tickets and the three guard-health
  // pages; the seven domain-outcome rules append after all three without
  // displacing anything.
  assert.equal(RULES[28]?.id, "prefix_trigger_verdict_refusals");
  assert.equal(RULES[32]?.id, "voice_queue_stall");
  assert.equal(RULES[38]?.id, "rest_global_pauses");
  NEW_RULES.forEach((id, i) => assert.equal(RULES[46 + i]?.id, id));
  for (const id of NEW_RULES) {
    assert.equal(RULES.filter((r) => r.id === id).length, 1, `${id} is catalogued exactly once`);
    assert.equal(
      packetFilename(id, "2026-10-09T20-11-06Z"),
      `evidence-${id}-2026-10-09T20-11-06Z.json`,
    );
  }
  assert.equal(ruleFor(FEED)?.severity, "ticket", "one failed fetch tickets, never pages");
  assert.equal(ruleFor(GAP)?.severity, "ticket", "a stuck receive gap tickets, never pages");
  assert.equal(ruleFor(PRIOR)?.severity, "ticket", "a repaired failed subscription tickets, never pages");
  assert.equal(ruleFor(SANCTION)?.severity, "ticket", "a refused sanction tickets, never pages");
  assert.equal(ruleFor(MODERATION)?.severity, "ticket", "a failed enforcement tickets, never pages");
  assert.equal(ruleFor(STUCK)?.severity, "ticket", "a stuck REST attempt tickets, never pages");
  assert.equal(ruleFor(BACKOFF)?.severity, "ticket", "a shed backoff wait tickets, never pages");
  // Tickets about domain outcomes must never fail a promotion stage.
  for (const id of NEW_RULES) {
    assert.deepEqual(evaluatePromotionGate([id]), { fail: false, reasons: [] }, `${id} stays out of the gate`);
  }
});

test("rule label vocabularies match the Rust contracts", () => {
  assert.deepEqual([...FEED_FETCH_ERROR_CLASSES], rustLabels("FEED_FETCH_ERROR_CLASSES"));
  assert.deepEqual([...EVENTSUB_CLASSIFIED_KINDS], rustLabels("EVENTSUB_DELIVERY_KINDS"));
  assert.deepEqual([...MODERATION_VERBS], rustLabels("MODERATION_VERBS"));
  assert.deepEqual([...MODERATION_FAILURE_OUTCOMES], ["discord_error", "storage_error"]);
  for (const outcome of MODERATION_FAILURE_OUTCOMES) {
    assert.ok(rustLabels("MODERATION_ENFORCEMENT_OUTCOMES").includes(outcome),
      `${outcome} is a Rust moderation outcome`);
  }
  assert.equal(EVENTSUB_RECEIVE_GAP_SAMPLES, 3);
  assert.equal(VOICE_REST_STUCK_SECONDS, 60);
  assert.equal(VOICE_REST_STUCK_SAMPLES, 3);
});

test("runbook and metrics docs name each rule, its anchor and its next action", () => {
  const cases = [
    { id: FEED, anchor: "alert-feed-fetch-failures", heading: "Alert: feed fetch failures", keyword: /error_class/ },
    { id: GAP, anchor: "alert-eventsub-receive-gap", heading: "Alert: EventSub receive gap", keyword: /unclassified/ },
    { id: PRIOR, anchor: "alert-eventsub-reconcile-prior-failed", heading: "Alert: EventSub reconcile prior failed", keyword: /prior_failed/ },
    { id: SANCTION, anchor: "alert-automod-sanction-refused", heading: "Alert: automod sanction refused", keyword: /sanction_refused/ },
    { id: MODERATION, anchor: "alert-moderation-enforcement-failures", heading: "Alert: moderation enforcement failures", keyword: /discord_error/ },
    { id: STUCK, anchor: "alert-voice-rest-stuck", heading: "Alert: voice REST stuck", keyword: /in-flight/ },
    { id: BACKOFF, anchor: "alert-voice-global-backoff-capped", heading: "Alert: voice global backoff capped", keyword: /GLOBAL_BACKOFF_MAX/ },
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

test("scrape validation: the seven consumed families are optional-when-present with Rust-pinned labels", () => {
  const families: Record<string, Record<string, readonly string[]>> = {
    [FEED_SERIES]: { error_class: rustLabels("FEED_FETCH_ERROR_CLASSES") },
    [RECEIVED_SERIES]: {},
    [RECONCILE_ERRORS_SERIES]: { error_class: rustLabels("EVENTSUB_RECONCILE_ERROR_CLASSES") },
    [AUTOMOD_SERIES]: { outcome: rustLabels("AUTOMOD_DECISION_OUTCOMES") },
    [MODERATION_SERIES]: { verb: rustLabels("MODERATION_VERBS"), outcome: rustLabels("MODERATION_ENFORCEMENT_OUTCOMES") },
    [STUCK_SERIES]: {},
    [BACKOFF_SERIES]: {},
  };
  const names = Object.keys(families);
  for (const [family, axes] of Object.entries(families)) {
    assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, family), false,
      `${family}: older containers without the family must still scrape`);
    assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[family], axes,
      `${family}: optional input pins the Rust label contract`);
    assert.ok(metricsBody().includes(`${family} `) || metricsBody().includes(`${family}{`),
      `fixture emits ${family}`);
  }
  const full = metricsBody();
  assert.ok(parseAlertScrape(full), "current exposition parses");
  const older = metricsLines()
    .filter((l) => !names.some((family) => l.startsWith(`${family} `) || l.startsWith(`${family}{`)))
    .join("\n");
  assert.ok(parseAlertScrape(older), "older exposition without the domain-outcome families still parses");
  // Present but malformed fails the whole scrape: a bad row must never
  // silently hold a rule off or on.
  const poison = (family: string, value: string) => {
    const lines = full.split("\n");
    const i = lines.findIndex((l) => l.startsWith(`${family}{`) || l.startsWith(`${family} `));
    assert.ok(i >= 0, `fixture must emit ${family}`);
    lines[i] = `${lines[i]!.replace(/ 0$/, "")} ${value}`;
    return lines.join("\n");
  };
  for (const value of ["NaN", "+Inf", "-1", "0.5", "garbage", "1e20"]) {
    for (const family of names) {
      assert.equal(parseAlertScrape(poison(family, value)), null, `${family}: ${value}`);
    }
  }
  assert.equal(parseAlertScrape(full.replace(`${BACKOFF_SERIES} 0`, `${BACKOFF_SERIES}{route="other"} 0`)), null,
    "a labeled row is not the label-free counter");
  assert.equal(parseAlertScrape(`${full}${BACKOFF_SERIES} 1\n`), null, "duplicate rows are rejected");
});

test("feed fetch attempts stay unconsumed: volume and 1:1 redundancy never fire", () => {
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, FETCHES_SERIES), false);
  assert.equal(Object.hasOwn(ALERT_OPTIONAL_INPUT_LABELS, FETCHES_SERIES), false);
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  assert.ok(!rules.includes(`gauge("${FETCHES_SERIES}")`), "no rule may consume the deferred fetches family");
  const attempts = (success: number, invalid: number, refused: number, failure: number) => ([
    `${FETCHES_SERIES}{outcome="success"} ${success}`,
    `${FETCHES_SERIES}{outcome="invalid_source"} ${invalid}`,
    `${FETCHES_SERIES}{outcome="refused_redirect"} ${refused}`,
    `${FETCHES_SERIES}{outcome="failure"} ${failure}`,
  ]);
  let state = ev(attempts(100, 0, 0, 0)).state;
  for (const [s, i, r, f] of [[200, 3, 1, 7], [300, 5, 2, 9]] as const) {
    state = ev(attempts(s, i, r, f), state).state;
    assert.deepEqual(state.firing, [], `attempts ${s}/${i}/${r}/${f} stay silent without the errors family`);
  }
});

test("presence capture stays unconsumed until its producer lands", () => {
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, PRESENCE_SERIES), false);
  assert.equal(Object.hasOwn(ALERT_OPTIONAL_INPUT_LABELS, PRESENCE_SERIES), false);
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  assert.ok(!rules.includes(`gauge("${PRESENCE_SERIES}")`), "no rule may consume the producer-less presence family");
  const capture = (inserted: number, duplicate: number, rejected: number) => ([
    `${PRESENCE_SERIES}{outcome="inserted"} ${inserted}`,
    `${PRESENCE_SERIES}{outcome="duplicate"} ${duplicate}`,
    `${PRESENCE_SERIES}{outcome="rejected"} ${rejected}`,
  ]);
  let state = ev(capture(1000, 10, 0)).state;
  state = ev(capture(2000, 25, 4), state).state;
  assert.deepEqual(state.firing, [], "presence growth stays silent");
});

test("join occupancy stays unconsumed: distribution telemetry never fires", () => {
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, OCCUPANCY_SERIES), false);
  assert.equal(Object.hasOwn(ALERT_OPTIONAL_INPUT_LABELS, OCCUPANCY_SERIES), false);
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  assert.ok(!rules.includes(`gauge("${OCCUPANCY_SERIES}")`), "no rule may consume the deferred occupancy family");
  const occupancy = (counts: number[]) =>
    counts.map((n, i) => `${OCCUPANCY_SERIES}{occupancy="${i === 7 ? "7+" : i}"} ${n}`);
  let state = ev(occupancy([50, 30, 10, 5, 3, 1, 1, 0])).state;
  state = ev(occupancy([60, 35, 12, 6, 4, 2, 1, 3]), state).state;
  assert.deepEqual(state.firing, [], "occupancy shifts stay silent");
});

test("reconcile actions stay unconsumed: successful mutations never fire", () => {
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, ACTIONS_SERIES), false);
  assert.equal(Object.hasOwn(ALERT_OPTIONAL_INPUT_LABELS, ACTIONS_SERIES), false);
  const rules = readFileSync(new URL("../src/alert-rules.ts", import.meta.url), "utf8");
  assert.ok(!rules.includes(`gauge("${ACTIONS_SERIES}")`), "no rule may consume the deferred actions family");
  const actions = (create: number, del: number) => ([
    `${ACTIONS_SERIES}{action="create"} ${create}`,
    `${ACTIONS_SERIES}{action="delete"} ${del}`,
  ]);
  let state = ev(actions(0, 0)).state;
  state = ev(actions(4, 2), state).state;
  assert.deepEqual(state.firing, [], "successful mutations stay silent");
});

test("non-prior reconcile errors stay quiet without a failed pass", () => {
  const errors = (failed: number, action: number, unhealthy: number, other: number) => ([
    `${RECONCILE_ERRORS_SERIES}{error_class="reconcile_failed"} ${failed}`,
    `${RECONCILE_ERRORS_SERIES}{error_class="reconcile_action_failed"} ${action}`,
    `${RECONCILE_ERRORS_SERIES}{error_class="reconcile_unhealthy"} ${unhealthy}`,
    `${RECONCILE_ERRORS_SERIES}{error_class="reconcile_prior_failed"} 0`,
    `${RECONCILE_ERRORS_SERIES}{error_class="other"} ${other}`,
  ]);
  let state = ev(errors(0, 0, 0, 0)).state;
  state = ev(errors(3, 2, 1, 4), state).state;
  assert.deepEqual(state.firing, [], "paired/unknown error sites stay silent without pass movement");
});

test("healthy automod outcomes never fire without a sanction refusal", () => {
  const others = ["accepted", "already_processed", "dry_run", "protected", "deleted", "warned", "timed_out"];
  const decisions = (n: number) => [
    ...others.map((o) => `${AUTOMOD_SERIES}{outcome="${o}"} ${n}`),
    `${AUTOMOD_SERIES}{outcome="sanction_refused"} 0`,
  ];
  let state = ev(decisions(10)).state;
  state = ev(decisions(50), state).state;
  assert.deepEqual(state.firing, [], "policy outcomes and landed ladders stay silent");
});

test("healthy moderation outcomes never fire without an enforcement failure", () => {
  const verbs = [...MODERATION_VERBS];
  const quiet = (n: number) => {
    const lines = moderationLines({});
    for (const verb of verbs) {
      for (const outcome of ["success", "refused", "other"]) {
        lines.push(`${MODERATION_SERIES}{verb="${verb}",outcome="${outcome}"} ${n}`);
      }
    }
    return lines;
  };
  let state = ev(quiet(10)).state;
  state = ev(quiet(40), state).state;
  assert.deepEqual(state.firing, [], "landed/refused/unknown outcomes stay silent");
});

for (const { id, build, get } of counterCases) {
  test(`${id} tickets on any increase, never on the first sample or a reset`, () => {
    // First sample only stores the baseline, even with a large nonzero count.
    assert.deepEqual(ev(build(0)).firing, []);
    const baseline = ev(build(10_000));
    assert.deepEqual(baseline.firing, []);
    assert.equal(get(baseline.state), 10_000);
    // Any increase tickets, however small; each increase is one window verdict.
    const fired = ev(build(10_001), baseline.state);
    assert.deepEqual(fired.firing, [id]);
    const ticket = transitionMessages(baseline.firing, fired.firing)[0]!;
    assert.match(ticket, new RegExp(`ALERT ${id} \\(ticket\\):`));
    assert.ok(ticket.includes(runbookUrl(ruleFor(id)!)), "ticket carries the runbook deep link");
    const flat = ev(build(10_001), fired.state);
    assert.deepEqual(flat.firing, [], "a flat window is a quiet window, not a held ticket");
    assert.match(transitionMessages(fired.firing, flat.firing)[0]!, new RegExp(`RESOLVED ${id}\\.`));
    assert.deepEqual(ev(build(10_005), flat.state).firing, [id], "the next increase tickets again");
    // A counter that went backwards means the process restarted: re-baseline silently.
    const restart = ev(build(2), fired.state);
    assert.deepEqual(restart.firing, []);
    assert.equal(get(restart.state), 2);
    assert.deepEqual(ev(build(3), restart.state).firing, [id], "post-restart growth tickets");
  });
}

test("feed failures sum every error class, including unknown collapses", () => {
  const baseline = ev(feedLines({}));
  assert.deepEqual(baseline.firing, []);
  // Growth isolated to `other` still tickets: the errors family records
  // failures only, so an unknown class is a failed fetch with a lost reason.
  assert.deepEqual(ev(feedLines({ other: 1 }), baseline.state).firing, [FEED]);
  const http = ev(feedLines({ http_error: 4, deadline: 2 }));
  assert.deepEqual(ev(feedLines({ http_error: 4, deadline: 3 }), http.state).firing, [FEED]);
  // A hostile extra row is ignored, never a subject: the fixed twelve still judge.
  const hostile = [...feedLines({ transport: 1 }), `${FEED_SERIES}{error_class="__proto__"} 99`];
  assert.deepEqual(ev(hostile, baseline.state).firing, [FEED]);
});

test("prior-failed findings stay silent on windows where a failed pass grew", () => {
  const window = reconcileWindow;
  const baseline = ev(window(10, 2, 7, 3));
  assert.deepEqual(baseline.firing, []);
  // The pass found prior failures and failed to converge: the failed-pass
  // ticket already names this window, so the finding stays silent.
  const failed = ev(window(10, 2, 8, 5), baseline.state);
  assert.deepEqual(failed.firing, ["eventsub_reconcile_failed"], "one failed pass raises exactly one ticket");
  assert.deepEqual(ev(window(11, 3, 8, 5), baseline.state).firing, ["eventsub_reconcile_failed"],
    "failed growth suppresses findings even beside healthy and pending growth");
  // The same finding beside healthy growth tickets on its own: the repair
  // landed and no other rule names the window.
  const repaired = ev(window(11, 2, 8, 6), failed.state);
  assert.deepEqual(repaired.firing, [PRIOR]);
  // A finding without the failed-pass row is no window: suppression cannot
  // be judged, so the baseline survives it.
  const noPasses = ev([`${RECONCILE_ERRORS_SERIES}{error_class="reconcile_prior_failed"} 9`], repaired.state);
  assert.deepEqual(noPasses.firing, []);
  assert.deepEqual(ev(window(12, 2, 8, 10), noPasses.state).firing, [PRIOR], "growth after the gap tickets");
});

test("prior-failed findings wait for a completed pass across mid-pass scrapes", () => {
  const baseline = ev(reconcileWindow(5, 0, 0, 0));
  // The Rust producer records the finding before awaiting the repair and
  // verification calls. Repeated mid-pass scrapes must not consume it.
  let state = baseline.state;
  for (let i = 0; i < 2; i++) {
    const midPass = ev(reconcileWindow(5, 0, 0, 1), state);
    assert.deepEqual(midPass.firing, [], "an in-progress repair is not a landed repair");
    assert.equal(midPass.state.reconcilePriorFailed, 0, "retain the unjudged finding delta");
    state = JSON.parse(JSON.stringify(midPass.state));
  }
  const failed = ev(reconcileWindow(5, 0, 1, 1), state);
  assert.deepEqual(failed.firing, ["eventsub_reconcile_failed"], "one failed pass, one ticket");
  assert.equal(failed.state.reconcilePriorFailed, 1, "failed completion consumes the finding");
  assert.deepEqual(ev(reconcileWindow(6, 0, 1, 1), failed.state).firing, [],
    "later recovery cannot re-ticket the failed pass's finding");
  // A completed non-failed outcome judges the retained finding exactly once.
  for (const completed of [reconcileWindow(6, 0, 0, 1), reconcileWindow(5, 1, 0, 1)]) {
    const repaired = ev(completed, state);
    assert.deepEqual(repaired.firing, [PRIOR], "healthy and pending completions both ticket");
    assert.deepEqual(ev(completed, repaired.state).firing, [], "flat completed counts stay quiet");
  }
});

test("prior-failed suppression re-baselines when any companion counter goes backwards", () => {
  const baseline = ev(reconcileWindow(5, 2, 2, 0));
  for (const [healthy, pending, failed] of [
    [5, 2, 1], // failed counter reset, prior counter grew
    [4, 3, 2], // healthy counter reset
    [6, 1, 2], // pending counter reset
  ] as const) {
    const restarted = ev(reconcileWindow(healthy, pending, failed, 1), baseline.state);
    assert.deepEqual(restarted.firing, ["eventsub_reconcile_failed"],
      "only the failed-pass rule judges a failed startup pass");
    assert.equal(restarted.state.reconcilePriorFailed, 1, "reset consumes the startup finding");
    assert.deepEqual(ev(reconcileWindow(healthy + 1, pending, failed, 1), restarted.state).firing, [],
      "recovery does not replay a startup finding");
  }
  const priorReset = ev(reconcileWindow(5, 0, 0, 3));
  const restarted = ev(reconcileWindow(6, 0, 0, 1), priorReset.state);
  assert.deepEqual(restarted.firing, [], "a prior-only reset also re-baselines silently");
  assert.deepEqual(ev(reconcileWindow(7, 0, 0, 2), restarted.state).firing, [PRIOR]);
});

test("prior-failed windows require all pass outcomes and preserve baselines across missing rows", () => {
  const baseline = ev(reconcileWindow(5, 2, 0, 0));
  for (let missing = 0; missing < 4; missing++) {
    const partial = reconcileWindow(6, 2, 0, 1).filter((_, i) => i !== missing);
    const unknown = ev(partial, baseline.state);
    assert.deepEqual(unknown.firing, [], "a partial outcome family cannot prove completion");
    assert.equal(unknown.state.reconcilePriorFailed, 0, "retain the finding baseline");
    assert.deepEqual(ev(reconcileWindow(6, 2, 0, 1), unknown.state).firing, [PRIOR],
      "the complete return sample judges against the last complete window");
  }
});

test("prior-failed windows reject invalid companion outcomes without consuming findings", () => {
  const baseline = ev(reconcileWindow(5, 2, 0, 0));
  for (let outcome = 0; outcome < 3; outcome++) {
    for (const value of ["NaN", "-1", "1.5"]) {
      const lines = reconcileWindow(6, 2, 0, 1);
      lines[outcome] = lines[outcome]!.replace(/ \d+$/, ` ${value}`);
      const unknown = ev(lines, baseline.state);
      assert.deepEqual(unknown.firing, []);
      assert.equal(unknown.state.reconcilePriorFailed, 0);
      assert.deepEqual(ev(reconcileWindow(6, 2, 0, 1), unknown.state).firing, [PRIOR]);
    }
  }
});

test("prior-failed legacy suppression state safely initializes new companion baselines", () => {
  const legacy: MetricsAlertState = {
    ...EMPTY_STATE, reconcilePriorFailed: 0, reconcilePriorFailedPassFailed: 0, reconcilePriorFailedSeen: true,
  };
  delete legacy.reconcilePriorFailedPassHealthy;
  delete legacy.reconcilePriorFailedPassPending;
  const initialized = ev(reconcileWindow(5, 2, 0, 1), legacy);
  assert.deepEqual(initialized.firing, [], "incomplete legacy baselines cannot judge a window");
  assert.equal(initialized.state.reconcilePriorFailed, 1);
  assert.equal(initialized.state.reconcilePriorFailedPassHealthy, 5);
  assert.equal(initialized.state.reconcilePriorFailedPassPending, 2);
  assert.deepEqual(ev(reconcileWindow(6, 2, 0, 2), initialized.state).firing, [PRIOR]);
});

test("moderation failures sum every verb and both failure outcomes", () => {
  const baseline = ev(moderationLines({}));
  assert.deepEqual(baseline.firing, []);
  assert.deepEqual(ev(moderationLines({ "ban/storage_error": 1 }), baseline.state).firing, [MODERATION]);
  const timeout = ev(moderationLines({ "timeout/discord_error": 4 }));
  assert.deepEqual(ev(moderationLines({ "timeout/discord_error": 4, "warn/discord_error": 1 }), timeout.state).firing,
    [MODERATION]);
});

test("a sustained receive gap tickets on the third consecutive sample", () => {
  const gap = (received: number, classified: number) =>
    [`${RECEIVED_SERIES} ${received}`, ...classifiedLines({ notification: classified })];
  assert.deepEqual(ev(gap(100, 100)).firing, [], "paired counts are silent");
  // One or two gapped samples stay quiet: a scrape can land mid-delivery.
  let state = ev(gap(100, 100)).state;
  state = ev(gap(101, 100), state).state;
  assert.deepEqual(state.firing, []);
  state = ev(gap(102, 100), state).state;
  assert.deepEqual(state.firing, [], "two gapped samples stay quiet");
  const fired = ev(gap(103, 100), state);
  assert.deepEqual(fired.firing, [GAP]);
  const ticket = transitionMessages(state.firing, fired.firing)[0]!;
  assert.match(ticket, new RegExp(`ALERT ${GAP} \\(ticket\\):`));
  assert.ok(ticket.includes(runbookUrl(ruleFor(GAP)!)), "ticket carries the runbook deep link");
  // A closed gap resolves; the ticket does not re-notify while held open.
  const closed = ev(gap(110, 110), fired.state);
  assert.deepEqual(closed.firing, []);
  assert.match(transitionMessages(fired.firing, closed.firing)[0]!, new RegExp(`RESOLVED ${GAP}\\.`));
  assert.deepEqual(transitionMessages(fired.firing, ev(gap(104, 100), fired.state).firing), [],
    "further gapped samples must not re-notify");
});

test("receive gap includes every classify-error variant in the classified sum", () => {
  for (const kind of rustLabels("EVENTSUB_DELIVERY_KINDS").slice(5, -1)) {
    let state = EMPTY_STATE;
    for (let received = 1; received <= EVENTSUB_RECEIVE_GAP_SAMPLES; received++) {
      state = ev([
        `${RECEIVED_SERIES} ${received}`,
        ...classifiedLines({ [kind]: received }),
      ], state).state;
      assert.deepEqual(state.firing, [], `${kind} is classified, not a receive gap`);
    }
  }
});

test("receive gap: partial families and negative gaps are no window", () => {
  const full = (received: number, classified: number) =>
    [`${RECEIVED_SERIES} ${received}`, ...classifiedLines({ notification: classified })];
  const baseline = ev(full(50, 50)).state;
  // Missing received row, missing classified family, or one kind row short:
  // no window, baseline untouched, never a firing reason.
  const partialKinds = classifiedLines({ notification: 50 }).slice(1);
  for (const lines of [[`${RECEIVED_SERIES} 51`], classifiedLines({ notification: 50 }), [`${RECEIVED_SERIES} 51`, ...partialKinds]] as const) {
    assert.deepEqual(ev([...lines], baseline).firing, [], "missing evidence is not a firing reason");
  }
  // Classified ahead of received cannot happen in one process (every
  // classification was received first); a corrupt sample holds, never judges.
  assert.deepEqual(ev(full(50, 55), baseline).firing, [], "a negative gap is no window");
  // A process restart (both counters reset together) closes the gap: the
  // wedge died with the old process, so the ticket resolves.
  let firing = ev(full(50, 50)).state;
  firing = ev(full(55, 50), firing).state;
  firing = ev(full(56, 50), firing).state;
  const fired = ev(full(57, 50), firing);
  assert.deepEqual(fired.firing, [GAP]);
  assert.deepEqual(ev(full(2, 2), fired.state).firing, [], "a joint reset resolves");
  // A gap (missing family) breaks the streak but preserves a firing ticket.
  const held = ev([`${RECEIVED_SERIES} 60`], fired.state);
  assert.deepEqual(held.firing, [GAP]);
  assert.deepEqual(ev(full(60, 60), held.state).firing, [], "the return sample can still resolve");
});

test("a failed scrape breaks the gap and stuck streaks without firing", () => {
  const gap = (received: number, classified: number) =>
    [`${RECEIVED_SERIES} ${received}`, ...classifiedLines({ notification: classified })];
  // Two gapped samples, a failed scrape, then two more: the streak restarts,
  // so the fourth gapped sample stays quiet and only the third consecutive
  // post-scrape sample tickets.
  let state = ev(gap(100, 100)).state;
  state = ev(gap(101, 100), state).state;
  state = ev(gap(102, 100), state).state;
  assert.deepEqual(state.firing, []);
  const interrupted = interruptDispatchDrops(state);
  assert.equal(interrupted.eventsubReceiveGapStreak, 0);
  assert.equal(interrupted.voiceRestStuckStreak, 0);
  let resumed = ev(gap(103, 100), interrupted).state;
  resumed = ev(gap(104, 100), resumed).state;
  assert.deepEqual(resumed.firing, [], "two post-scrape gaps stay quiet");
  assert.deepEqual(ev(gap(105, 100), resumed).firing, [GAP]);
  // Same for the stuck gauge: two stuck samples, a failed scrape, one more.
  const age = (n: number) => [`${STUCK_SERIES} ${n}`];
  let stuck = ev(age(61), ev(age(61)).state).state;
  stuck = ev(age(61), interruptDispatchDrops(stuck)).state;
  assert.deepEqual(stuck.firing, [], "the failed scrape broke the stuck streak");
  stuck = ev(age(61), stuck).state;
  assert.deepEqual(ev(age(61), stuck).firing, [STUCK]);
});

test("a stuck voice REST attempt tickets on the third consecutive sample", () => {
  const age = (n: number | string) => [`${STUCK_SERIES} ${n}`];
  assert.deepEqual(ev(age(0)).firing, []);
  let state = ev(age(0)).state;
  state = ev(age(61), state).state;
  assert.deepEqual(state.firing, [], "one stuck sample stays quiet");
  state = ev(age(75), state).state;
  assert.deepEqual(state.firing, [], "two stuck samples stay quiet");
  const fired = ev(age(90), state);
  assert.deepEqual(fired.firing, [STUCK]);
  const ticket = transitionMessages(state.firing, fired.firing)[0]!;
  assert.match(ticket, new RegExp(`ALERT ${STUCK} \\(ticket\\):`));
  assert.ok(ticket.includes(runbookUrl(ruleFor(STUCK)!)), "ticket carries the runbook deep link");
  // At or below the bound resolves; further stuck samples do not re-notify.
  const cleared = ev(age(60), fired.state);
  assert.deepEqual(cleared.firing, []);
  assert.match(transitionMessages(fired.firing, cleared.firing)[0]!, new RegExp(`RESOLVED ${STUCK}\\.`));
  assert.deepEqual(transitionMessages(fired.firing, ev(age(120), fired.state).firing), [],
    "further stuck samples must not re-notify");
  // A healthy ceiling never fires, however often it is sampled.
  let ceiling = ev(age(10)).state;
  for (const n of [10, 9, 10, 60, 0]) ceiling = ev(age(n), ceiling).state;
  assert.deepEqual(ceiling.firing, [], "at-or-below-bound reads stay silent");
});

test("stuck REST: invalid samples break the streak but preserve a firing ticket", () => {
  const age = (n: number | string) => [`${STUCK_SERIES} ${n}`];
  const baseline = ev(age(0)).state;
  for (const lines of [[], [age("NaN")[0]!], [age("+Inf")[0]!], [age(-1)[0]!], [age(1.5)[0]!], [`${STUCK_SERIES} 7`, `${STUCK_SERIES} 7`], [`${STUCK_SERIES}{guild="1"} 7`]] as const) {
    assert.deepEqual(ev([...lines], baseline).firing, [], "missing evidence is not a firing reason");
  }
  let firing = ev(age(61)).state;
  firing = ev(age(61), firing).state;
  const fired = ev(age(61), firing);
  assert.deepEqual(fired.firing, [STUCK]);
  for (const lines of [[], [age("NaN")[0]!], [`${STUCK_SERIES}{guild="1"} 7`]] as const) {
    assert.deepEqual(ev([...lines], fired.state).firing, [STUCK], "a gap holds the firing ticket");
  }
  assert.deepEqual(ev(age(0), fired.state).firing, [], "the return sample can still resolve");
});

for (const { id, bad, good, streak } of [
  {
    id: GAP,
    bad: [`${RECEIVED_SERIES} 61`, ...classifiedLines({ notification: 50 })],
    good: [`${RECEIVED_SERIES} 61`, ...classifiedLines({ notification: 61 })],
    streak: (state: MetricsAlertState) => state.eventsubReceiveGapStreak,
  },
  {
    id: STUCK,
    bad: [`${STUCK_SERIES} 120`],
    good: [`${STUCK_SERIES} 0`],
    streak: (state: MetricsAlertState) => state.voiceRestStuckStreak,
  },
]) {
  for (const interruption of ["failed scrape", "missing sample", "invalid sample"]) {
    test(`${id}: still-bad evidence after a ${interruption} holds the active ticket`, () => {
      let state = ev(good).state;
      for (let i = 0; i < 3; i++) state = ev(bad, state).state;
      assert.deepEqual(state.firing, [id]);
      const held = interruption === "failed scrape" ? interruptDispatchDrops(state)
        : ev(interruption === "missing sample" ? [] : bad.map((row) => row.replace(/ \d+$/, " NaN")), state).state;
      assert.equal(streak(held), 0, "the interruption breaks consecutive evidence");
      assert.deepEqual(held.firing, [id], "missing evidence cannot resolve");
      assert.deepEqual(transitionMessages(state.firing, held.firing), []);
      const resumed = ev(bad, JSON.parse(JSON.stringify(held)));
      assert.equal(streak(resumed.state), 1, "one fresh sample is not a new three-sample streak");
      assert.deepEqual(resumed.firing, [id], "an ongoing incident must not falsely resolve");
      assert.deepEqual(transitionMessages(held.firing, resumed.firing), [], "no resolve or re-ticket");
      const cleared = ev(good, resumed.state);
      assert.deepEqual(cleared.firing, []);
      assert.match(transitionMessages(resumed.firing, cleared.firing)[0]!, new RegExp(`RESOLVED ${id}\\.`));
      assert.deepEqual(transitionMessages(cleared.firing, ev(good, cleared.state).firing), [],
        "valid resolving evidence emits exactly one resolution");
    });
  }
}

test("a healthy guild exposition fires no rule at all", () => {
  const samples = parseAlertScrape(metricsBody());
  assert.ok(samples, "healthy fixture parses");
  assert.deepEqual(evaluateMetrics(samples, EMPTY_STATE, NOW).firing, []);
});

for (const { id, series, build, get } of counterCases) {
  test(`${id}: a missing sample keeps the baseline but posts no window verdict`, () => {
    const baseline = ev(build(5));
    const fired = ev(build(6), baseline.state);
    assert.deepEqual(fired.firing, [id]);
    // No window, no verdict: the ticket lifecycle is per increase, so a gap
    // carries no evidence either way and the baseline survives it.
    for (const gap of [[], [`${series} NaN`]] as const) {
      const unknown = ev([...gap], fired.state);
      assert.deepEqual(unknown.firing, []);
      assert.equal(get(unknown.state), 6, "the gap must not corrupt the baseline");
      assert.deepEqual(ev(build(7), unknown.state).firing, [id], "growth after the gap tickets");
      assert.deepEqual(ev(build(6), unknown.state).firing, [], "flat after the gap stays silent");
    }
  });
}

for (const { id, series, build, get } of counterCases) {
  const invalidSamples: Record<string, string[]> = {
    "absent family": [],
    "NaN": [`${series} NaN`],
    "infinity": [`${series} +Inf`],
    "negative": [`${series} -1`],
    "fractional": [`${series} 1.5`],
    "unsafe integer": [`${series} 9007199254740992`],
    "extra label": [`${series}{guild="1"} 7`],
  };
  for (const [name, lines] of Object.entries(invalidSamples)) {
    test(`${id}: invalid sample (${name}) never fires and never corrupts the baseline`, () => {
      assert.deepEqual(ev(lines).firing.filter((k) => k === id), [],
        "missing evidence is not a firing reason");
      const fired = ev(build(7), ev(build(6)).state);
      assert.deepEqual(fired.firing, [id]);
      const unknown = ev(lines, fired.state);
      assert.deepEqual(unknown.firing.filter((k) => k === id), [],
        "missing evidence posts no window verdict");
      assert.equal(get(unknown.state), 7, "the gap must not corrupt the baseline");
    });
  }
  test(`${id}: a partial family is no window`, () => {
    const fired = ev(build(7), ev(build(6)).state);
    assert.deepEqual(fired.firing, [id]);
    // Every fixed series must be present: a partial family keeps the baseline.
    const partial = build(9).slice(1);
    const unknown = ev(partial.length > 0 ? partial : [`${series}{other="x"} 9`], fired.state);
    assert.deepEqual(unknown.firing.filter((k) => k === id), [],
      "a partial family posts no window verdict");
    assert.equal(get(unknown.state), 7, "the gap must not corrupt the baseline");
  });
}

test("domain-outcome state survives DO reconstruction and reads legacy storage without the new fields", () => {
  const count = (n: number) => [`${BACKOFF_SERIES} ${n}`];
  // A baseline round-tripped through JSON keeps judging windows.
  let state = ev(count(40)).state;
  state = ev(count(40), JSON.parse(JSON.stringify(state))).state;
  assert.deepEqual(state.firing, []);
  assert.deepEqual(ev(count(41), JSON.parse(JSON.stringify(state))).firing, [BACKOFF]);
  // Storage written before these rules existed has none of their fields: the
  // first counter sample is a baseline, never a firing.
  const legacy = {
    firing: [] as string[], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0,
    sendBlocked: 0, sendBlockedStreak: 0, voiceOps: 0, voiceFailures: 0,
    voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {},
  } as MetricsAlertState;
  for (const { id, build } of counterCases) {
    assert.deepEqual(ev(build(7), legacy).firing.filter((k) => k === id), [],
      `${id}: legacy storage baselines without firing`);
  }
  assert.deepEqual(ev([`${RECEIVED_SERIES} 5`, ...classifiedLines({ notification: 3 })], legacy).firing
    .filter((k) => k === GAP), [], "gap: legacy storage starts no streak");
  assert.deepEqual(ev([`${STUCK_SERIES} 61`], legacy).firing.filter((k) => k === STUCK), [],
    "stuck: legacy storage starts no streak");
});

test("the new rules do not disturb unrelated rules sharing the same scrape", () => {
  const unrelated = [`two_bot_job_consecutive_failures{job="counter"} 3`];
  const base = ev(
    [`${BACKOFF_SERIES} 9`, `${RECONCILE_ERRORS_SERIES}{error_class="reconcile_prior_failed"} 4`, ...unrelated],
  );
  assert.deepEqual([...base.firing].sort(), ["job_consecutive_failures:counter"].sort());
  const fired = ev(
    [`${BACKOFF_SERIES} 10`, `${RECONCILE_ERRORS_SERIES}{error_class="reconcile_prior_failed"} 4`, ...unrelated],
    base.state,
  );
  assert.deepEqual([...fired.firing].sort(),
    ["job_consecutive_failures:counter", BACKOFF].sort());
});
