/** Job-catalog drift guard (TOG-12376). `evaluateMetrics` covers jobs without
 * an entry in `JOB_INTERVAL_SECONDS`, outside `TICKER_STALE_JOBS`, and outside
 * `UNKNOWN_STALE_EXCLUDED` through the fail-closed `job_unknown_stale` ticket
 * (a coarse 2-hour fallback, never a precise cadence), so every label in the
 * Rust `JOBS` allowlist (`crates/core/src/metrics.rs`) must still have a
 * cadence equal to its registered Rust `*_INTERVAL_MS / 1000`, ticker_stale
 * coverage with a reason, or an explicit, reasoned unknown-stale entry, and
 * `docs/metrics.md` must list every label. Only a cadence, ticker coverage,
 * or event-driven exclusion in `src/alert-rules.ts` retires the coarse
 * fallback; a reasoned unknown-stale note alone does not. Unlisted future
 * labels ticket rather than staying silent in the meantime.
 * Reads the Rust sources as text; never compiles Rust or starts the bot.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { EMPTY_STATE, evaluateMetrics, JOB_INTERVAL_SECONDS, JOB_RUNS_JOBS, parseExposition, TICKER_STALE_JOBS, TICKER_STALE_SECONDS, UNKNOWN_STALE_EXCLUDED } from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { FORWARDED_FLAGS, forwardedFlagVars, NOT_FORWARDED } from "../src/container-env.ts";
import { metricsBody, metricsLines } from "./fixtures/metrics.ts";

const root = new URL("../../", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, root), "utf8");
const metricsRs = read("crates/core/src/metrics.rs");
const websiteJobsRs = read("crates/bot/src/website_jobs.rs");
const communityJobsRs = read("crates/bot/src/community_jobs.rs");
const feedJobsRs = read("crates/bot/src/feed_jobs.rs");
const selfRoleHandlersRs = read("crates/bot/src/self_role_handlers.rs");
const memberRuntimeRs = read("crates/bot/src/member_runtime.rs");
const automodGatewayRs = read("crates/bot/src/automod_gateway.rs");
const pollJobsRs = read("crates/bot/src/poll_jobs.rs");
const jobsRs = read("crates/bot/src/jobs.rs");
const mainRs = read("crates/bot/src/main.rs");
const metricsMd = read("docs/metrics.md");

/** Labels with no fixed Worker cadence. `invite_snapshot`, `other`,
 * `audit_retry`, `feeds`, `self_role_recovery`, `member_unban_sweep` and
 * `automod_expiry` are covered by the fail-closed `job_unknown_stale`
 * ticket (`UNKNOWN_JOB_STALE_SECONDS`) instead of `job_stale`/`ticker_stale`;
 * `session_checkpoint` is event-driven and excluded from the fallback via
 * `UNKNOWN_STALE_EXCLUDED` (still listed here with its reason so the catalog
 * stays explicit). */
const UNKNOWN_STALE_JOBS = new Map<string, string>([
  ["invite_snapshot", "no periodic caller; stays zero until a real caller records a completion"],
  ["session_checkpoint", "event-driven: one success per durable gateway commit, not a scheduled tick"],
  ["other", "catch-all for unknown job names only; every registered producer now has its own label, so this slot has no single cadence"],
  ["feeds", "covered by job_unknown_stale, not job_stale: boot-configured supervisor cadence (60–86400 s) with a live-configurable inner polling gate; skipped polls return success and reset the consecutive-failure streak, so failure alerting is reliable only while the live interval is at most the boot cadence; neither a fixed Worker cadence nor a last-success timestamp measures feed delivery freshness"],
  ["self_role_recovery", "covered by job_unknown_stale, not job_stale: 30 s recovery sweep with 0–5 s jitter and a 25 s timeout; two intervals are too close to the ~60 s scrape cadence for safe job_stale paging. This label-only slice retains failure-only alerting rather than adding ticker_stale policy; a successful sweep need not settle every repair"],
  ["member_unban_sweep", "covered by job_unknown_stale, not job_stale: 30 s unban sweep (UNBAN_SWEEP_INTERVAL_SECONDS) with startup jitter and a 180 s attempt timeout; two intervals are too close to the ~60 s scrape cadence for safe job_stale paging, and one attempt may outlive its cadence. Failure alerting is per-label job_consecutive_failures at 3; a sweep with no completions is covered only by the coarse 2-hour fallback"],
  ["automod_expiry", "covered by job_unknown_stale, not job_stale: 60 s repeat-history expiry tick (EXPIRY_CADENCE) with startup jitter and a 5 s timeout; a gateway without built activation is a no-op success, and the job only does real work when TWO_AUTOMOD=1. Failure alerting is per-label job_consecutive_failures at 3; staleness uses the coarse 2-hour fallback, not a precise cadence"],
  ["audit_retry", "covered by job_unknown_stale, not ticker_stale: 30 s supervisor sweep with parked/halt reporting in audit_runtime.rs (parked when unconfigured, halted sweeps still record success); a wedged sweep that keeps failing surfaces via job_consecutive_failures, and only a truly wedged sweep tickets here; not registered through the website/community schedulers this catalog parses"],
]);
/** 15 s tickers covered by `ticker_stale` (TICKER_STALE_SECONDS window), not `job_stale`. */
const TICKER_COVERED = new Map<string, string>([
  ["scheduled_messages", "15 s ticker registered in scheduled_jobs.rs; covered by ticker_stale, not the website/community schedulers this catalog parses"],
  ["polls", "15 s ticker registered in poll_jobs.rs with announcements and boot-identity gates; covered by ticker_stale with the shared 600 s window, not a precise job_stale cadence"],
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

/** Direct Job producers outside the website/community cadence catalogs.
 * Understands `name: "literal"`, `name: SOME_CONST` and the one
 * `name: NAMES[0]` indirection used by `member_runtime.rs`
 * (`NAMES = [SWEEP_JOB_NAME]`). */
function directJobNames(source: string, file: string): string[] {
  const constants = new Map([...source.matchAll(/\bconst ([A-Z_]+): &str = "([^"]+)";/g)]
    .map((m) => [m[1]!, m[2]!]));
  const arrays = new Map([...source.matchAll(/\bconst ([A-Z_]+): \[&str; \d+\] = \[([^\]]*)\];/g)]
    .map((m) => [m[1]!, m[2]!.split(",").map((s) => s.trim()).filter(Boolean)]));
  const bodies = source.replace(/->\s*Job\s*\{/g, "");
  const producers = [...bodies.matchAll(/\bJob\s*\{\s*name:\s*("[^"]+"|[A-Z_]+(?:\[\d+\])?)\s*,/g)];
  assert.ok(producers.length > 0, `${file}: no direct Job producers found`);
  assert.equal(producers.length, [...bodies.matchAll(/\bJob\s*\{/g)].length,
    `${file}: unsupported Job producer; extend the catalog parser`);
  return producers.map((m) => {
    const raw = m[1]!;
    if (raw.startsWith('"')) return raw.slice(1, -1);
    const indexed = /^([A-Z_]+)\[(\d+)\]$/.exec(raw);
    if (indexed) {
      const entries = arrays.get(indexed[1]!);
      assert.ok(entries, `${file}: unresolved job array ${indexed[1]}`);
      const element = entries![Number(indexed[2]!)];
      assert.ok(element, `${file}: unresolved job index ${raw}`);
      const resolved = element!.startsWith('"')
        ? element!.slice(1, -1)
        : constants.get(element!);
      assert.ok(resolved, `${file}: unresolved job name ${raw}`);
      return resolved!;
    }
    const name = constants.get(raw);
    assert.ok(name, `${file}: unresolved job name ${raw}`);
    return name!;
  });
}

interface Catalog {
  jobs: readonly string[];
  registered: readonly string[];
  rust: ReadonlyMap<string, number>;
  worker: Readonly<Record<string, number>>;
  ticker: ReadonlyMap<string, string>;
  exempt: ReadonlyMap<string, string>;
}

/** Every mismatch between the Rust allowlist/cadences and the Worker tables. */
function catalogGaps({ jobs, registered, rust, worker, ticker, exempt }: Catalog): string[] {
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
  for (const job of new Set([...rust.keys(), ...registered])) if (!jobs.includes(job)) gaps.push(`${job}: registered job missing from Rust JOBS`);
  return gaps;
}

const JOBS = parseJobs(metricsRs);
const RUST = rustCadenceSeconds();
const REGISTERED = [...new Set([
  ...directJobNames(feedJobsRs, "feed_jobs.rs"),
  ...directJobNames(selfRoleHandlersRs, "self_role_handlers.rs"),
  ...directJobNames(memberRuntimeRs, "member_runtime.rs"),
  ...directJobNames(automodGatewayRs, "automod_gateway.rs"),
  ...directJobNames(pollJobsRs, "poll_jobs.rs"),
])];
const real: Catalog = { jobs: JOBS, registered: REGISTERED, rust: RUST, worker: JOB_INTERVAL_SECONDS, ticker: TICKER_COVERED, exempt: UNKNOWN_STALE_JOBS };

test("parses the full Rust job allowlist and every registered cadence", () => {
  assert.deepEqual(JOBS, [
    "invite_snapshot", "session_checkpoint", "counter", "rank", "scheduled_events",
    "settings", "presence_probe", "community_scorecard", "inactivity", "audit_retry", "scheduled_messages", "polls",
    "feeds", "self_role_recovery", "member_unban_sweep", "automod_expiry", "other",
  ]);
  // Rust *_INTERVAL_MS / 1000: community_snapshots.rs:45,47, scheduled_events.rs:26,
  // presence.rs:36, community.rs:61, inactivity.rs:27.
  assert.deepEqual(Object.fromEntries(RUST), {
    counter: 60, rank: 600, scheduled_events: 600,
    presence_probe: 3600, community_scorecard: 60, inactivity: 3600,
  });
});

test("feed and self-role producers retain their real cadence and explicit unknown-stale entries", () => {
  for (const job of ["feeds", "self_role_recovery"]) {
    assert.ok(REGISTERED.includes(job), `${job}: missing from REGISTERED`);
  }
  assert.match(feedJobsRs, /cadence: Duration::from_secs\(seconds\)/);
  assert.match(selfRoleHandlersRs, /let cadence = Duration::from_secs\(30\);\s*Job\s*\{\s*name: RECOVERY_JOB_NAME,\s*cadence,/);
  for (const job of ["feeds", "self_role_recovery"]) {
    assert.ok(UNKNOWN_STALE_JOBS.get(job)?.trim(), `${job}: missing reasoned unknown-stale entry`);
    assert.ok(!Object.hasOwn(JOB_INTERVAL_SECONDS, job));
    assert.ok(!TICKER_STALE_JOBS.includes(job));
  }
});

test("unban sweep and automod expiry producers retain their real cadence and explicit unknown-stale entries", () => {
  assert.deepEqual(REGISTERED, ["feeds", "self_role_recovery", "member_unban_sweep", "automod_expiry", "polls"]);
  assert.match(memberRuntimeRs, /UNBAN_SWEEP_INTERVAL_SECONDS/);
  assert.match(memberRuntimeRs, /Job\s*\{\s*name:\s*NAMES\[0\],\s*cadence,/);
  assert.match(automodGatewayRs, /const EXPIRY_CADENCE: Duration = Duration::from_secs\(60\);/);
  assert.match(automodGatewayRs, /Job\s*\{\s*name:\s*JOB_NAME,\s*cadence:\s*EXPIRY_CADENCE,/);
  for (const job of ["member_unban_sweep", "automod_expiry"]) {
    assert.ok(UNKNOWN_STALE_JOBS.get(job)?.trim(), `${job}: missing reasoned unknown-stale entry`);
    assert.ok(!Object.hasOwn(JOB_INTERVAL_SECONDS, job));
    assert.ok(!TICKER_STALE_JOBS.includes(job));
  }
});

test("a registered direct producer absent from JOBS fails the catalog", () => {
  for (const [source, file] of [[feedJobsRs, "feed_jobs.rs"], [selfRoleHandlersRs, "self_role_handlers.rs"], [memberRuntimeRs, "member_runtime.rs"], [automodGatewayRs, "automod_gateway.rs"], [pollJobsRs, "poll_jobs.rs"]]) {
    const mutated = source + '\nfn fixture() -> Job { Job { name: "fake_producer", cadence: Duration::from_secs(30), } }';
    const registered = directJobNames(mutated, file!);
    assert.ok(registered.includes("fake_producer"), "fixture did not inject a registered producer");
    assert.deepEqual(catalogGaps({ ...real, registered: [...REGISTERED, ...registered] }), [
      "fake_producer: registered job missing from Rust JOBS",
    ]);
  }
});

test("every Rust JOBS label has a matching staleness cadence, ticker coverage or a reasoned unknown-stale entry", () => {
  assert.deepEqual(catalogGaps(real), []);
});

test("the 15 s tickers are covered by ticker_stale with an explicit 10-minute window", () => {
  assert.equal(TICKER_STALE_SECONDS, 600);
  assert.deepEqual([...TICKER_STALE_JOBS].sort(), ["polls", "scheduled_messages", "settings"]);
  assert.deepEqual([...TICKER_COVERED.keys()].sort(), [...TICKER_STALE_JOBS].sort());
  for (const job of TICKER_STALE_JOBS) {
    assert.ok(JOBS.includes(job), `${job}: ticker_stale covers a label outside Rust JOBS`);
    assert.ok(!Object.hasOwn(JOB_INTERVAL_SECONDS, job), `${job}: covered by both ticker_stale and JOB_INTERVAL_SECONDS`);
    assert.ok(!UNKNOWN_STALE_JOBS.has(job), `${job}: covered by both ticker_stale and an unknown-stale entry`);
  }
  assert.ok(UNKNOWN_STALE_JOBS.has("audit_retry"), "audit_retry keeps its reasoned unknown-stale entry");
});

test("polls register a fixed 15 s producer only behind announcements and the boot identity", () => {
  assert.deepEqual(directJobNames(pollJobsRs, "poll_jobs.rs"), ["polls"]);
  assert.ok(REGISTERED.includes("polls"));
  assert.match(mainRs, /^mod poll_jobs;$/m, "the producer must be compiled into the bot");
  assert.match(pollJobsRs, /fn register\(context: Arc<Context>, activation: &BootActivation\) -> Option<Job>/);
  assert.match(pollJobsRs, /let features = activation\.constrain_features\(FeatureGates::from_env\(\)\.ok\(\)\?\);\s*if !features\.announcements \{\s*return None;\s*\}/,
    "invalid flags or a refused boot identity must park polls, not default them on");
  assert.match(pollJobsRs, /let cadence = Duration::from_secs\(15\);\s*Some\(Job\s*\{\s*name: NAME,\s*cadence,\s*cadence_source: None,/);
  assert.match(pollJobsRs, /runtime\s*\.tick_polls\(\)\s*\.await/,
    "the registered action must execute timed polls, not a no-op success");
});

test("polls join the boot status map and the shared Skip-on-missed-ticks supervisor", () => {
  assert.match(websiteJobsRs, /let activation = BootActivation::from_config\(config\);/);
  assert.match(websiteJobsRs, /if let Some\(job\) = crate::poll_jobs::register\(context\.clone\(\), &activation\) \{\s*registered\.push\(job\);\s*\} else \{\s*parked\.push\(crate::poll_jobs::NAME\);\s*\}/,
    "enabled polls must be supervised; refused polls must remain visibly parked");
  assert.match(websiteJobsRs, /\.chain\(\[crate::poll_jobs::NAME\]\)/);
  assert.match(websiteJobsRs, /serve_jobs\(registered, status, shutdown\.clone\(\), http\)\.await/);
  assert.match(websiteJobsRs, /tokio::spawn\(jobs::supervise\(registered, status, shutdown\.0\.subscribe\(\)\)\)/);
  assert.match(jobsRs, /for job in jobs \{[\s\S]*?tasks\.spawn\(run_job\(\s*job,/);
  assert.match(jobsRs, /let mut cadence = job\.cadence;\s*let mut interval = cadence_interval\(first_deadline, cadence\);/);
  assert.match(jobsRs, /fn cadence_interval\([^)]*\) -> tokio::time::Interval \{\s*let mut interval = tokio::time::interval_at\(first_tick, cadence\);\s*interval\.set_missed_tick_behavior\(MissedTickBehavior::Skip\);\s*interval\s*\}/,
    "missed poll ticks must skip, never burst into catch-up writes");
});

test("poll alert inputs retain the full Rust job vocabulary and reject missing poll rows", () => {
  assert.deepEqual([...JOB_RUNS_JOBS], JOBS);
  for (const name of [
    "two_bot_job_last_success_timestamp_seconds", "two_bot_job_consecutive_failures",
    "two_bot_job_last_error_class", "two_bot_job_runs_total",
  ]) {
    assert.deepEqual(ALERT_INPUT_LABELS[name]?.job, JOBS, `${name}: polls must not be treated as an unknown future label`);
  }
  assert.ok(parseAlertScrape(metricsBody()), "the complete Rust scrape must validate");
  const lines = metricsLines();
  const pollRows = lines.filter((line) => /\{job="polls"[,}]/.test(line));
  assert.equal(pollRows.length, 2 + ALERT_INPUT_LABELS.two_bot_job_last_error_class!.class!.length + 2);
  for (const removed of pollRows) {
    assert.equal(parseAlertScrape(lines.filter((line) => line !== removed).join("\n")), null,
      `missing required poll row must fail the scrape: ${removed}`);
  }
});

test("polls use only the 600 s ticker window, with parked and exact-boundary controls", () => {
  const now = 1_000_000;
  for (const [lastSuccess, expected] of [
    [0, []], [now - 31, []], [now - 599, []], [now - 600, []],
    [now - 601, ["ticker_stale:polls"]], [now - 7201, ["ticker_stale:polls"]],
  ] as const) {
    const samples = parseExposition(`two_bot_job_last_success_timestamp_seconds{job="polls"} ${lastSuccess}\n`);
    assert.deepEqual(evaluateMetrics(samples, EMPTY_STATE, now).firing, expected, `last success ${lastSuccess}`);
  }
  const failures = parseExposition('two_bot_job_consecutive_failures{job="polls"} 3\n');
  assert.deepEqual(evaluateMetrics(failures, EMPTY_STATE, now).firing, ["job_consecutive_failures:polls"]);
});

test("the poll acceptance switch is classified test-only and never forwarded as a runtime flag", () => {
  const name = "TWO_POLL_RUNTIME_ACCEPTANCE";
  assert.match(NOT_FORWARDED[name] ?? "", /test-only/);
  assert.ok(!(FORWARDED_FLAGS as readonly string[]).includes(name));
  for (const value of ["1", "0", "invalid"]) {
    assert.deepEqual(forwardedFlagVars({ [name]: value }), {});
    assert.deepEqual(forwardedFlagVars({ [name]: value, TWO_ANNOUNCEMENTS: "1" }), { TWO_ANNOUNCEMENTS: "1" });
  }
});

test("event-driven exclusions stay inside the Rust allowlist and never overlap cadence or ticker coverage", () => {
  assert.deepEqual([...UNKNOWN_STALE_EXCLUDED].sort(), ["session_checkpoint"]);
  for (const job of UNKNOWN_STALE_EXCLUDED) {
    assert.ok(JOBS.includes(job), `${job}: excluded label outside Rust JOBS`);
    assert.ok(!Object.hasOwn(JOB_INTERVAL_SECONDS, job), `${job}: excluded and in JOB_INTERVAL_SECONDS`);
    assert.ok(!TICKER_STALE_JOBS.includes(job), `${job}: excluded and covered by ticker_stale`);
    assert.ok(UNKNOWN_STALE_JOBS.has(job), `${job}: excluded without a reasoned unknown-stale entry`);
  }
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
