/** Job-catalog drift guard (TOG-12376). `evaluateMetrics` silently skips the
 * `job_stale` rule for any job without an entry in `JOB_INTERVAL_SECONDS`, so
 * every label in the Rust `JOBS` allowlist (`crates/core/src/metrics.rs`) must
 * either have a cadence equal to its registered Rust `*_INTERVAL_MS / 1000` or
 * an explicit, reasoned exemption, and `docs/metrics.md` must list every label.
 * Reads the Rust sources as text; never compiles or runs anything.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { JOB_INTERVAL_SECONDS } from "../src/alert-rules.ts";

const root = new URL("../../", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, root), "utf8");
const metricsRs = read("crates/core/src/metrics.rs");
const websiteJobsRs = read("crates/bot/src/website_jobs.rs");
const communityJobsRs = read("crates/bot/src/community_jobs.rs");
const metricsMd = read("docs/metrics.md");

/** Labels with no fixed cadence, so `job_stale` cannot apply to them. */
const STALE_EXEMPT = new Map<string, string>([
  ["invite_snapshot", "no periodic caller; stays zero until a real caller records a completion"],
  ["session_checkpoint", "event-driven: one success per durable gateway commit, not a scheduled tick"],
  ["other", "catch-all for unknown job names; unrelated jobs share it, so no single cadence exists"],
  ["audit_retry", "supervisor sweep with its own 30 s loop and parked/halt reporting in audit_runtime.rs; not registered through the website/community schedulers this catalog parses"],
  ["scheduled_messages", "15 s ticker registered in scheduled_jobs.rs, not the website/community schedulers this catalog parses; at STALE_INTERVALS=2 a staleness rule would be noisy and needs its own threshold"],
]);

function parseJobs(source: string): string[] {
  const block = /pub const JOBS: &\[&str\] = &\[([\s\S]*?)\];/.exec(source);
  assert.ok(block, "JOBS allowlist not found in crates/core/src/metrics.rs");
  return [...block[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!);
}

/** `pub const NAME: u64 = <integer product>;` across crates/core/src/*.rs. */
function coreIntervalsMs(): Map<string, number> {
  const out = new Map<string, number>();
  const dir = "crates/core/src/";
  for (const file of readdirSync(new URL(dir, root)).filter((f) => f.endsWith(".rs"))) {
    for (const m of read(dir + file).matchAll(/^pub const ([A-Z0-9_]+_INTERVAL_MS): u64 = ([^;]+);/gm)) {
      const expr = m[2]!.trim();
      assert.match(expr, /^[\d_]+(?:\s*\*\s*[\d_]+)*$/, `${file}: ${m[1]} is not a plain integer product`);
      assert.ok(!out.has(m[1]!), `${m[1]} defined twice in crates/core/src`);
      out.set(m[1]!, expr.split("*").reduce((acc, n) => acc * Number(n.trim().replaceAll("_", "")), 1));
    }
  }
  return out;
}

/** `Kind::X => CONST` arms of a registration file's `fn cadence`. */
function cadenceArms(source: string, file: string): Map<string, string> {
  const body = /\nfn cadence\(kind: Kind\) -> Duration \{([\s\S]*?)\n\}/.exec(source);
  assert.ok(body, `${file}: fn cadence not found`);
  const arms = new Map([...body[1]!.matchAll(/Kind::(\w+) => ([A-Z0-9_]+_INTERVAL_MS)\b/g)].map((m) => [m[1]!, m[2]!]));
  assert.ok(arms.size > 0, `${file}: no Kind => *_INTERVAL_MS arms`);
  return arms;
}

/** Registered scheduled-job name -> its Rust cadence in seconds. */
function rustCadenceSeconds(): Map<string, number> {
  const intervals = coreIntervalsMs();
  const pairs: [string, string, Map<string, string>][] = [];

  // website_jobs.rs: `NAMES` zipped with `[Kind::Counter, Kind::Rank, Kind::Events]`.
  const names = /pub const NAMES: \[&str; \d+\] = \[([^\]]*)\];/.exec(websiteJobsRs);
  const kinds = /\.zip\(\[([^\]]*)\]\)/.exec(websiteJobsRs);
  assert.ok(names && kinds, "website_jobs.rs: NAMES/zip registration not found");
  const webNames = [...names[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!);
  const webKinds = [...kinds[1]!.matchAll(/Kind::(\w+)/g)].map((m) => m[1]!);
  assert.equal(webNames.length, webKinds.length, "website_jobs.rs: NAMES and kinds differ in length");
  const webArms = cadenceArms(websiteJobsRs, "website_jobs.rs");
  webNames.forEach((name, i) => pairs.push([name, webKinds[i]!, webArms]));

  // community_jobs.rs: `("presence_probe", Kind::PresenceProbe, gate)` tuples.
  const communityArms = cadenceArms(communityJobsRs, "community_jobs.rs");
  const tuples = [...communityJobsRs.matchAll(/\(\s*"([a-z_]+)",\s*Kind::(\w+),/g)];
  assert.ok(tuples.length > 0, "community_jobs.rs: registration tuples not found");
  for (const m of tuples) pairs.push([m[1]!, m[2]!, communityArms]);

  const out = new Map<string, number>();
  for (const [name, kind, arms] of pairs) {
    const constant = arms.get(kind);
    assert.ok(constant, `${name}: Kind::${kind} has no cadence arm`);
    const ms = intervals.get(constant);
    assert.ok(ms !== undefined, `${name}: ${constant} not defined in crates/core/src`);
    assert.equal(ms % 1000, 0, `${constant} is not a whole number of seconds`);
    assert.ok(!out.has(name), `${name} registered twice`);
    out.set(name, ms / 1000);
  }
  return out;
}

interface Catalog {
  jobs: readonly string[];
  rust: ReadonlyMap<string, number>;
  worker: Readonly<Record<string, number>>;
  exempt: ReadonlyMap<string, string>;
}

/** Every mismatch between the Rust allowlist/cadences and the Worker table. */
function catalogGaps({ jobs, rust, worker, exempt }: Catalog): string[] {
  const gaps: string[] = [];
  const cadence = new Map(Object.entries(worker));
  for (const job of jobs) {
    const reason = exempt.get(job);
    if (reason !== undefined) {
      if (reason.trim() === "") gaps.push(`${job}: exemption has no reason`);
      if (cadence.has(job)) gaps.push(`${job}: both exempt and in JOB_INTERVAL_SECONDS`);
      continue;
    }
    const seconds = cadence.get(job);
    const expected = rust.get(job);
    if (seconds === undefined) gaps.push(`${job}: no JOB_INTERVAL_SECONDS cadence and no exemption, so job_stale never fires`);
    else if (expected === undefined) gaps.push(`${job}: no Rust scheduler registration to take a cadence from`);
    else if (seconds !== expected) gaps.push(`${job}: JOB_INTERVAL_SECONDS ${seconds}s != Rust ${expected}s`);
  }
  for (const job of cadence.keys()) if (!jobs.includes(job)) gaps.push(`${job}: cadence for a label outside Rust JOBS`);
  for (const job of exempt.keys()) if (!jobs.includes(job)) gaps.push(`${job}: exemption for a label outside Rust JOBS`);
  for (const job of rust.keys()) if (!jobs.includes(job)) gaps.push(`${job}: registered job missing from Rust JOBS`);
  return gaps;
}

const JOBS = parseJobs(metricsRs);
const RUST = rustCadenceSeconds();
const real: Catalog = { jobs: JOBS, rust: RUST, worker: JOB_INTERVAL_SECONDS, exempt: STALE_EXEMPT };

test("parses the full Rust job allowlist and every registered cadence", () => {
  assert.deepEqual(JOBS, [
    "invite_snapshot", "session_checkpoint", "counter", "rank", "scheduled_events",
    "presence_probe", "community_scorecard", "inactivity", "audit_retry", "scheduled_messages", "other",
  ]);
  // Rust *_INTERVAL_MS / 1000: community_snapshots.rs:45,47, scheduled_events.rs:26,
  // presence.rs:36, community.rs:61, inactivity.rs:27.
  assert.deepEqual(Object.fromEntries(RUST), {
    counter: 60, rank: 600, scheduled_events: 600,
    presence_probe: 3600, community_scorecard: 60, inactivity: 3600,
  });
});

test("every Rust JOBS label has a matching job_stale cadence or a reasoned exemption", () => {
  assert.deepEqual(catalogGaps(real), []);
});

test("a new Rust job without a cadence fails the catalog", () => {
  const header = "pub const JOBS: &[&str] = &[";
  const jobs = parseJobs(metricsRs.replace(header, `${header}\n    "fake_job",`));
  assert.ok(jobs.includes("fake_job"), "fixture did not inject the fake job");
  assert.deepEqual(catalogGaps({ ...real, jobs }), [
    "fake_job: no JOB_INTERVAL_SECONDS cadence and no exemption, so job_stale never fires",
  ]);
});

test("a cadence drift on either side fails the catalog", () => {
  assert.deepEqual(catalogGaps({ ...real, worker: { ...JOB_INTERVAL_SECONDS, rank: 60 } }), [
    "rank: JOB_INTERVAL_SECONDS 60s != Rust 600s",
  ]);
  assert.deepEqual(catalogGaps({ ...real, rust: new Map([...RUST, ["inactivity", 1800]]) }), [
    "inactivity: JOB_INTERVAL_SECONDS 3600s != Rust 1800s",
  ]);
  const { counter: _, ...withoutCounter } = JOB_INTERVAL_SECONDS;
  assert.deepEqual(catalogGaps({ ...real, worker: withoutCounter }), [
    "counter: no JOB_INTERVAL_SECONDS cadence and no exemption, so job_stale never fires",
  ]);
});

test("exemptions and cadences stay inside the Rust allowlist", () => {
  assert.deepEqual(catalogGaps({ ...real, worker: { ...JOB_INTERVAL_SECONDS, other: 60 } }), [
    "other: both exempt and in JOB_INTERVAL_SECONDS",
  ]);
  assert.deepEqual(catalogGaps({ ...real, exempt: new Map([...STALE_EXEMPT, ["retired", "gone"]]) }), [
    "retired: exemption for a label outside Rust JOBS",
  ]);
  assert.deepEqual(catalogGaps({ ...real, exempt: new Map([...STALE_EXEMPT, ["other", " "]]) }), [
    "other: exemption has no reason",
  ]);
});

test("docs/metrics.md lists every job label", () => {
  const bullet = /^- `two_bot_job_runs_total\{job,outcome\}`[\s\S]*?(?=\n- |\n\n)/m.exec(metricsMd);
  assert.ok(bullet, "docs/metrics.md: job label allowlist bullet not found");
  const listed = bullet[0].slice(bullet[0].indexOf("is one of"), bullet[0].indexOf(";"));
  assert.deepEqual([...listed.matchAll(/`([a-z_]+)`/g)].map((m) => m[1]!), JOBS);
});
