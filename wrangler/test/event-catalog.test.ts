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

/** Heading-slug index for one checked-in doc (mirrors alert-rules.test.ts). */
function docAnchors(doc: string): Set<string> {
  const text = read(`docs/${doc}`);
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 -]/g, "").replace(/ /g, "-")));
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

test("the catalog entry links to an existing startup-diagnostics heading", () => {
  const slugs = docAnchors("startup-diagnostics.md");
  const links = [...metricsMd.matchAll(/startup-diagnostics\.md#([A-Za-z0-9-]+)/g)].map((m) => m[1]!);
  assert.ok(links.length > 0, "docs/metrics.md: no startup-diagnostics.md#anchor link");
  for (const anchor of links) {
    assert.ok(slugs.has(anchor), `docs/metrics.md: missing startup-diagnostics heading for #${anchor}`);
  }
});
