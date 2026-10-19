/** Worker event-name catalog drift guard. `docs/metrics.md` is the stable
 * event-name catalog: it lists the four Rust-side `voice_event` names and the
 * one Worker-side `container_gateway_failure` event with its `phase`/`class`
 * vocabulary. The Worker emits that event from `wrangler/src/index.ts` (phase
 * plus class from the bot's `/readyz` `gateway_failure`), and the class tokens
 * are the `FailureClass` variants in `crates/bot/src/gateway_failure.rs`.
 * Reads the sources as text; never compiles or runs anything.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const root = new URL("../../", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, root), "utf8");
const metricsMd = read("docs/metrics.md");
const gatewayFailureRs = read("crates/bot/src/gateway_failure.rs");
const workerIndex = read("wrangler/src/index.ts");

/** GitHub-style heading slugs for a doc's text (mirrors alert-rules.test.ts). */
function headingAnchors(text: string): Set<string> {
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")));
}

/** `Self::Variant => "token"` arms of `FailureClass::as_str`. */
function failureClasses(source: string): string[] {
  const implStart = source.indexOf("impl FailureClass");
  assert.ok(implStart >= 0, "crates/bot/src/gateway_failure.rs: impl FailureClass not found");
  const body = /fn as_str\(self\) -> &'static str \{([\s\S]*?)\n    \}/.exec(source.slice(implStart));
  assert.ok(body, "crates/bot/src/gateway_failure.rs: FailureClass::as_str not found");
  const classes = [...body[1]!.matchAll(/Self::\w+ => "([a-z0-9_]+)"/g)].map((m) => m[1]!);
  assert.ok(classes.length > 0, "no FailureClass tokens parsed");
  return classes;
}

interface Catalog {
  metricsMd: string;
  classes: readonly string[];
  workerIndex: string;
}

/** Every mismatch between the catalog entry, the Rust vocabulary and the Worker. */
function eventGaps({ metricsMd: md, classes, workerIndex: worker }: Catalog): string[] {
  const gaps: string[] = [];
  if (!md.includes('event="container_gateway_failure"')) {
    gaps.push('docs/metrics.md: missing event="container_gateway_failure" catalog entry');
  }
  if (!md.includes("`durable_gateway`")) {
    gaps.push("docs/metrics.md: missing phase `durable_gateway`");
  }
  for (const token of classes) {
    if (!md.includes(`\`${token}\``)) gaps.push(`docs/metrics.md: missing class \`${token}\``);
  }
  if (!worker.includes('event: "container_gateway_failure"')) {
    gaps.push('wrangler/src/index.ts: missing event: "container_gateway_failure" emission');
  }
  return gaps;
}

const CLASSES = failureClasses(gatewayFailureRs);
const real: Catalog = { metricsMd, classes: CLASSES, workerIndex };

test("parses the full Rust failure-class vocabulary", () => {
  assert.deepEqual(CLASSES, [
    "store_unavailable",
    "gateway_pool_connect_failed",
    "checkpoint_load_failed",
    "onboarding_gates_invalid",
    "onboarding_init_failed",
    "custom_commands_init_failed",
    "milestones_load_failed",
    "automod_config_invalid",
    "automod_executor_failed",
    "raid_executor_failed",
    "gateway_runtime_failed",
    "gateway_task_panicked",
  ]);
});

test("catalog lists the worker event with every phase/class token", () => {
  assert.deepEqual(eventGaps(real), []);
});

test("a new Rust class without a catalog entry fails the catalog", () => {
  assert.deepEqual(
    eventGaps({ ...real, classes: [...CLASSES, "fake_class"] }),
    ["docs/metrics.md: missing class `fake_class`"],
  );
});

test("a missing worker emission fails the catalog", () => {
  const gaps = eventGaps({ ...real, workerIndex: "// no emission here" });
  assert.ok(gaps.some((g) => g.includes("wrangler/src/index.ts")));
});

const DIAGNOSTIC_DOCS = ["metrics.md", "worker-gateway-failure-alerts.md"] as const;

/** Anchors of every `startup-diagnostics.md#…` link in a doc's text. */
function diagnosticAnchors(text: string): string[] {
  return [...text.matchAll(/startup-diagnostics\.md#([A-Za-z0-9_-]+)/g)].map((m) => m[1]!);
}

function brokenAnchors(anchors: readonly string[], slugs: ReadonlySet<string>): string[] {
  return anchors.filter((anchor) => !slugs.has(anchor));
}

test("the diagnostic-doc links resolve to an existing startup-diagnostics heading", () => {
  const slugs = headingAnchors(read("docs/startup-diagnostics.md"));
  for (const doc of DIAGNOSTIC_DOCS) {
    const anchors = diagnosticAnchors(read(`docs/${doc}`));
    assert.ok(anchors.length > 0, `docs/${doc}: no startup-diagnostics.md#anchor link`);
    assert.deepEqual(brokenAnchors(anchors, slugs), [], `docs/${doc}: broken startup-diagnostics.md#anchor link`);
  }
});

test("a startup-diagnostics link that drops the underscore is reported, not skipped", () => {
  const slugs = headingAnchors("## Self-diagnosing gateway failures (`/readyz` `gateway_failure`)\n");
  const stale = "[startup diagnostics](startup-diagnostics.md#self-diagnosing-gateway-failures-readyz-gatewayfailure)";
  const fixed = "[startup diagnostics](startup-diagnostics.md#self-diagnosing-gateway-failures-readyz-gateway_failure)";
  assert.deepEqual(diagnosticAnchors(stale), ["self-diagnosing-gateway-failures-readyz-gatewayfailure"]);
  assert.deepEqual(brokenAnchors(diagnosticAnchors(stale), slugs), ["self-diagnosing-gateway-failures-readyz-gatewayfailure"]);
  assert.deepEqual(diagnosticAnchors(fixed), ["self-diagnosing-gateway-failures-readyz-gateway_failure"]);
  assert.deepEqual(brokenAnchors(diagnosticAnchors(fixed), slugs), []);
});
