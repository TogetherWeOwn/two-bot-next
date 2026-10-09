#!/usr/bin/env node
import { readFileSync } from "node:fs";

// Mirrors READYZ_* in scripts/rollback_readiness_probe.py; the drift test in
// test/readiness-gate.test.ts pins these names to crates/bot/src/server.rs.
const STATES = new Set(["ready", "starting", "down"]);
export const READYZ_REQUIRED = ["process", "gateway"];
// 503 is parked, never acceptance: process ready and every not-ready component listed here in that state.
export const READYZ_PARKED = new Map([
  ["gateway", new Set(["down", "starting"])],
  ["database", new Set(["down"])],
  ["token_invalid", new Set(["down"])],
]);

const isComponentRow = (row) =>
  Array.isArray(row) && row.length === 2 && typeof row[0] === "string" && STATES.has(row[1]);

// A fence or proxy refusal is never the bot's component report.
export function checkReadyz(code, report) {
  const components = report?.components;
  if (typeof report !== "object" || report === null || "error" in report ||
      !Array.isArray(components) || !components.every(isComponentRow)) {
    throw new Error("Missing bot readiness component breakdown");
  }
  const state = new Map(components);
  if (state.size !== components.length || !READYZ_REQUIRED.every((name) => state.has(name))) {
    throw new Error("Unexpected bot readiness status or components");
  }
  const notReady = [...state].filter(([, status]) => status !== "ready");
  if (code === "200" && notReady.length === 0) {
    return `all ${state.size} components ready`;
  }
  if (code === "503" && notReady.length > 0 &&
      notReady.every(([name, status]) => READYZ_PARKED.get(name)?.has(status))) {
    const summary = notReady.map(([name, status]) => `${name} ${status}`).join(", ");
    return `process ready, ${summary} (parked, not E2E approval)`;
  }
  throw new Error("Unexpected bot readiness status or components");
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  try {
    console.log(checkReadyz(process.argv[2], JSON.parse(readFileSync(process.argv[3], "utf8"))));
  } catch {
    console.error("Readiness gate failed: expected the bot's component breakdown, never an ownership refusal");
    process.exitCode = 1;
  }
}
