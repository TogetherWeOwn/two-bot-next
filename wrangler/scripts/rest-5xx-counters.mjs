#!/usr/bin/env node
// One authenticated /ops/metrics scrape that records the cumulative Discord
// REST counters since the bot process started: `restTotal` (every judged
// result) and `rest5xx`, read through the same evaluateMetrics() state the
// promotion error-rate gate and the alert rules use, so `transport` stays
// out exactly as in `rest_5xx_rate`. The promotion regression gate
// (scripts/promotion_regression_gate.py) compares staging's soak counters
// (the staging process starts at the candidate's deploy) with production's
// baseline counters.
//
// Same destination fence, bearer floor, redirect refusal, body cap and
// fixed failure vocabulary as wrangler/scripts/promotion-error-gate.mjs,
// whose fetchExposition() this reuses. The bearer comes from
// METRICS_SCRAPE_TOKEN only. A missing or short bearer records
// `available: false` (exit 0): the comparator treats it as no data, never as
// a pass. Scrape failures exit 1 (fail closed). The receipt carries integers
// and fixed tokens only, never the bearer or the exposition body.
import { writeFileSync } from "node:fs";
import { EMPTY_STATE, evaluateMetrics } from "../src/alert-rules.ts";
import {
  allowedOriginsFromEnv,
  fetchExposition,
  isDirectInvocation,
  originOf,
} from "./promotion-error-gate.mjs";

const MIN_SCRAPE_TOKEN_LENGTH = 32;
const TIMEOUT_SECONDS = 15;
const EXIT_USAGE = 2;

export function parseArgs(argv) {
  const config = { metricsUrl: "", out: "", userAgent: "two-bot-next-promotion-regression/1.0" };
  for (let i = 0; i < argv.length; i += 2) {
    const flag = argv[i];
    const value = argv[i + 1];
    if (value === undefined) throw new Error(`flag ${flag} needs a value`);
    if (flag === "--metrics-url") config.metricsUrl = value;
    else if (flag === "--out") config.out = value;
    else if (flag === "--user-agent") config.userAgent = value;
    else throw new Error(`unknown flag ${flag}`);
  }
  if (!config.out) throw new Error("--out is required");
  if (!config.userAgent) throw new Error("--user-agent must not be empty");
  return config;
}

/** Cumulative REST counters from one parsed exposition. */
export function countersFrom(samples) {
  const { state } = evaluateMetrics(samples, EMPTY_STATE, 0);
  return { restTotal: state.restTotal, rest5xx: state.rest5xx ?? 0 };
}

export async function runCli(argv, env, deps = {}) {
  let config;
  try {
    config = parseArgs(argv);
  } catch (error) {
    console.log(`::error::rest 5xx counters: ${error instanceof Error ? error.message : "usage error"}`);
    return EXIT_USAGE;
  }
  const origin = originOf(config.metricsUrl);
  if (!origin || !allowedOriginsFromEnv(env).has(origin)) {
    const receipt = { available: false, code: "invalid_metrics_origin" };
    writeFileSync(config.out, `${JSON.stringify(receipt)}\n`);
    console.log("::error::rest 5xx counters: metrics origin is not a configured staging/production Worker identity");
    return EXIT_USAGE;
  }
  const token = env.METRICS_SCRAPE_TOKEN;
  if (typeof token !== "string" || token.length < MIN_SCRAPE_TOKEN_LENGTH) {
    const receipt = { available: false, code: "token_absent" };
    writeFileSync(config.out, `${JSON.stringify(receipt)}\n`);
    console.log("::warning::rest 5xx counters: metrics scrape token absent; recorded as no data");
    console.log(JSON.stringify(receipt));
    return 0;
  }
  let scraped;
  try {
    scraped = await fetchExposition({
      metricsUrl: origin,
      token,
      timeoutSeconds: TIMEOUT_SECONDS,
      fetchImpl: deps.fetchImpl ?? fetch,
      retrySleepSeconds: deps.retrySleepSeconds,
      userAgent: config.userAgent,
    });
  } catch (error) {
    const code = typeof error?.code === "string" ? error.code : "transport_failed";
    const receipt = { available: false, code };
    writeFileSync(config.out, `${JSON.stringify(receipt)}\n`);
    console.log(`::error::rest 5xx counters: scrape failed (${code})`);
    return 1;
  }
  const receipt = { available: true, code: "scraped", ...countersFrom(scraped.samples) };
  writeFileSync(config.out, `${JSON.stringify(receipt)}\n`);
  console.log(JSON.stringify(receipt));
  return 0;
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  runCli(process.argv.slice(2), process.env).then(
    (code) => process.exit(code),
    () => {
      console.log("::error::rest 5xx counters failed closed: unexpected error");
      process.exit(1);
    },
  );
}
