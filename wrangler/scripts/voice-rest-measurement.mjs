#!/usr/bin/env node
// Inert until called by a staging workflow. Persist only validated receipts.
import { readFileSync, writeFileSync } from "node:fs";
import { originOf, fetchExposition, isDirectInvocation } from "./promotion-error-gate.mjs";
import { buildReceipt, compareReceipts, validateMetadata } from "./voice-rest-receipt.mjs";

export async function runCli(argv, env, deps = {}) {
  try {
    const [mode, ...args] = argv;
    const options = Object.create(null);
    if (args.length % 2) throw new Error();
    for (let i = 0; i < args.length; i += 2) {
      if (!/^--[a-z-]+$/.test(args[i]) || Object.hasOwn(options, args[i])) throw new Error();
      options[args[i]] = args[i + 1];
    }
    const keys = mode === "capture" ? ["--metadata", "--metrics-url", "--out"]
      : mode === "compare" ? ["--before", "--after", "--out"] : [];
    if (keys.length !== Object.keys(options).length || !keys.every((key) => options[key])) throw new Error();
    const read = (path) => JSON.parse(readFileSync(path, "utf8"));
    let result;
    if (mode === "capture") {
      const metadata = validateMetadata(read(options["--metadata"]));
      if (Date.now() >= Date.parse(metadata.expiresAt)) throw new Error();
      const origin = originOf(options["--metrics-url"]);
      const staging = [env.STAGING_WORKER_URL, env.STAGING_URL].map(originOf).filter(Boolean);
      const production = [env.PRODUCTION_WORKER_URL, env.PRODUCTION_URL].map(originOf).filter(Boolean);
      if (!origin || !staging.includes(origin) || production.includes(origin)) throw new Error();
      const token = env.METRICS_SCRAPE_TOKEN;
      if (typeof token !== "string" || token.length < 32) throw new Error();
      const { samples } = await fetchExposition({
        metricsUrl: origin, token, timeoutSeconds: 15, fetchImpl: deps.fetchImpl ?? fetch,
        retrySleepSeconds: deps.retrySleepSeconds, userAgent: "two-bot-next-voice-measurement/1.0",
      });
      result = buildReceipt({ samples, metadata });
    } else {
      result = compareReceipts(read(options["--before"]), read(options["--after"]));
    }
    writeFileSync(options["--out"], `${JSON.stringify(result)}\n`, { flag: "wx", mode: 0o600 });
    return 0;
  } catch {
    console.error("voice REST measurement refused");
    return 1;
  }
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  runCli(process.argv.slice(2), process.env).then((code) => { process.exitCode = code; });
}
