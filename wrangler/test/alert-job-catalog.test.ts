/** Job-catalog drift guard (TOG-12376). `evaluateMetrics` covers jobs without
 * an entry in `JOB_INTERVAL_SECONDS` and outside `TICKER_STALE_JOBS` through
 * the fail-closed `job_unknown_stale` ticket (a coarse 2-hour fallback, never
 * a precise cadence), so every label in the Rust `JOBS` allowlist
 * (`crates/core/src/metrics.rs`) must still have a cadence equal to its
 * registered Rust `*_INTERVAL_MS / 1000`, ticker_stale coverage with a
 * reason, or an explicit, reasoned unknown-stale entry, and `docs/metrics.md`
 * must list every label. The explicit entry is what retires the coarse
 * fallback for that job; unlisted future labels ticket rather than staying
 * silent in the meantime.
 * Reads the Rust sources as text; never compiles or runs anything.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { JOB_INTERVAL_SECONDS, TICKER_STALE_JOBS, TICKER_STALE_SECONDS } from "../src/alert-rules.ts";

const root = new URL("../../", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, root), "utf8");
const metricsRs = read("crates/core/src/metrics.rs");
const websiteJobsRs = read("crates/bot/src/website_jobs.rs");
const communityJobsRs = read("crates/bot/src/community_jobs.rs");
const metricsMd = read("docs/metrics.md");

/** Labels with no fixed cadence, covered by the fail-closed `job_unknown_stale`
 * ticket (`UNKNOWN_JOB_STALE_SECONDS`) instead of `job_stale`/`ticker_stale`. */
const UNKNOWN_STALE_JOBS = new Map<string, string>([
  ["invite_snapshot", "no periodic caller; stays zero until a real caller records a completion"],
  ["session_checkpoint", "event-driven: one success per durable gateway commit, not a scheduled tick"],
  ["other", "catch-all for unknown job names; unrelated jobs share it, so no single cadence exists"],
  ["audit_retry", "covered by job_unknown_stale, not ticker_stale: 30 s supervisor sweep with parked/halt reporting in audit_runtime.rs (parked when unconfigured, halted sweeps still record success); a wedged sweep that keeps failing surfaces via job_consecutive_failures, and only a truly wedged sweep tickets here; not registered through the website/community schedulers this catalog parses"],
]);
/** 15 s tickers covered by `ticker_stale` (TICKER_STALE_SECONDS window), not `job_stale`. */
const TICKER_COVERED = new Map<string, string>([
  ["scheduled_messages", "15 s ticker registered in scheduled_jobs.rs; covered by ticker_stale, not the website/community schedulers this catalog parses"],
  ["settings", "DB-only 15 s version poll registered in settings_jobs.rs, parked without DATABASE_URL; covered by ticker_stale, not the website/community schedulers this catalog parses"],
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
  ticker: ReadonlyMap<string, string>;
  exempt: ReadonlyMap<string, string>;
}

/** Every mismatch between the Rust allowlist/cadences and the Worker tables. */
function catalogGaps({ jobs, rust, worker, ticker, exempt }: Catalog): string[] {
  const gaps: string[] = [];
  const cadence = new Map(Object.entries(worker));
  for (const job of jobs) {
    const reason = exempt.get(job);
    if (reason !== undefined) {
      if (reason.trim() === "") gaps.push(`${job}: unknown-stale entry has no reason`);
      if (cadence.has(job)) gaps.push(`${job}: both unknown-stale covered and in JOB_INTERVAL_SECONDS`);
      if (ticker.has(job)) gaps.push(`${job}: both unknown-stale covered and covered by ticker_stale`);
      continue;
    }
    if (ticker.has(job)) {
      const why = ticker.get(job)!;
      if (why.trim() === "") gaps.push(`${job}: ticker coverage has no reason`);
      if (cadence.has(job)) gaps.push(`${job}: both covered by ticker_stale and in JOB_INTERVAL_SECONDS`);
      continue;
    }
    const seconds = cadence.get(job);
    const expected = rust.get(job);
    if (seconds === undefined) gaps.push(`${job}: no JOB_INTERVAL_SECONDS cadence, no ticker_stale coverage and no unknown-stale entry, so only the coarse fallback ticket covers it`);
    else if (expected === undefined) gaps.push(`${job}: no Rust scheduler registration to take a cadence from`);
    else if (seconds !== expected) gaps.push(`${job}: JOB_INTERVAL_SECONDS ${seconds}s != Rust ${expected}s`);
  }
  for (const job of cadence.keys()) if (!jobs.includes(job)) gaps.push(`${job}: cadence for a label outside Rust JOBS`);
  for (const job of ticker.keys()) if (!jobs.includes(job)) gaps.push(`${job}: ticker_stale coverage for a label outside Rust JOBS`);
  for (const job of exempt.keys()) if (!jobs.includes(job)) gaps.push(`${job}: unknown-stale entry for a label outside Rust JOBS`);
  for (const job of rust.keys()) if (!jobs.includes(job)) gaps.push(`${job}: registered job missing from Rust JOBS`);
  return gaps;
}

const JOBS = parseJobs(metricsRs);
const RUST = rustCadenceSeconds();
const real: Catalog = { jobs: JOBS, rust: RUST, worker: JOB_INTERVAL_SECONDS, ticker: TICKER_COVERED, exempt: UNKNOWN_STALE_JOBS };

test("parses the full Rust job allowlist and every registered cadence", () => {
  assert.deepEqual(JOBS, [
    "invite_snapshot", "session_checkpoint", "counter", "rank", "scheduled_events",
    "settings", "presence_probe", "community_scorecard", "inactivity", "audit_retry", "scheduled_messages", "other",
  ]);
  // Rust *_INTERVAL_MS / 1000: community_snapshots.rs:45,47, scheduled_events.rs:26,
  // presence.rs:36, community.rs:61, inactivity.rs:27.
  assert.deepEqual(Object.fromEntries(RUST), {
    counter: 60, rank: 600, scheduled_events: 600,
    presence_probe: 3600, community_scorecard: 60, inactivity: 3600,
  });
});

test("every Rust JOBS label has a matching staleness cadence, ticker coverage or a reasoned unknown-stale entry", () => {
  assert.deepEqual(catalogGaps(real), []);
});

test("the 15 s tickers are covered by ticker_stale with an explicit 10-minute window", () => {
  assert.equal(TICKER_STALE_SECONDS, 600);
  assert.deepEqual([...TICKER_STALE_JOBS].sort(), ["scheduled_messages", "settings"]);
  assert.deepEqual([...TICKER_COVERED.keys()].sort(), [...TICKER_STALE_JOBS].sort());
  for (const job of TICKER_STALE_JOBS) {
    assert.ok(JOBS.includes(job), `${job}: ticker_stale covers a label outside Rust JOBS`);
    assert.ok(!Object.hasOwn(JOB_INTERVAL_SECONDS, job), `${job}: covered by both ticker_stale and JOB_INTERVAL_SECONDS`);
    assert.ok(!UNKNOWN_STALE_JOBS.has(job), `${job}: covered by both ticker_stale and an unknown-stale entry`);
  }
  assert.ok(UNKNOWN_STALE_JOBS.has("audit_retry"), "audit_retry keeps its reasoned unknown-stale entry");
});

test("a new Rust job without a cadence fails the catalog", () => {
  const header = "pub const JOBS: &[&str] = &[";
  const jobs = parseJobs(metricsRs.replace(header, `${header}\n    "fake_job",`));
  assert.ok(jobs.includes("fake_job"), "fixture did not inject the fake job");
  assert.deepEqual(catalogGaps({ ...real, jobs }), [
    "fake_job: no JOB_INTERVAL_SECONDS cadence, no ticker_stale coverage and no unknown-stale entry, so only the coarse fallback ticket covers it",
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
    "counter: no JOB_INTERVAL_SECONDS cadence, no ticker_stale coverage and no unknown-stale entry, so only the coarse fallback ticket covers it",
  ]);
});

test("unknown-stale entries, ticker coverage and cadences stay inside the Rust allowlist", () => {
  assert.deepEqual(catalogGaps({ ...real, worker: { ...JOB_INTERVAL_SECONDS, other: 60 } }), [
    "other: both unknown-stale covered and in JOB_INTERVAL_SECONDS",
  ]);
  assert.deepEqual(catalogGaps({ ...real, exempt: new Map([...UNKNOWN_STALE_JOBS, ["retired", "gone"]]) }), [
    "retired: unknown-stale entry for a label outside Rust JOBS",
  ]);
  assert.deepEqual(catalogGaps({ ...real, exempt: new Map([...UNKNOWN_STALE_JOBS, ["other", " "]]) }), [
    "other: unknown-stale entry has no reason",
  ]);
  assert.deepEqual(catalogGaps({ ...real, ticker: new Map([...TICKER_COVERED, ["retired", "gone"]]) }), [
    "retired: ticker_stale coverage for a label outside Rust JOBS",
  ]);
  assert.deepEqual(catalogGaps({ ...real, exempt: new Map([...UNKNOWN_STALE_JOBS, ["settings", "double-covered"]]) }), [
    "settings: both unknown-stale covered and covered by ticker_stale",
  ]);
  assert.deepEqual(catalogGaps({ ...real, worker: { ...JOB_INTERVAL_SECONDS, settings: 15 } }), [
    "settings: both covered by ticker_stale and in JOB_INTERVAL_SECONDS",
  ]);
});

test("docs/metrics.md lists every job label", () => {
  const bullet = /^- `two_bot_job_runs_total\{job,outcome\}`[\s\S]*?(?=\n- |\n\n)/m.exec(metricsMd);
  assert.ok(bullet, "docs/metrics.md: job label allowlist bullet not found");
  const listed = bullet[0].slice(bullet[0].indexOf("is one of"), bullet[0].indexOf(";"));
  assert.deepEqual([...listed.matchAll(/`([a-z_]+)`/g)].map((m) => m[1]!), JOBS);
});
