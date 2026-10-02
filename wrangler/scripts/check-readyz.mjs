#!/usr/bin/env node
import { readFileSync } from "node:fs";

// Preserve the existing scaffold gate, but never mistake a fence/proxy refusal
// for the Rust server's process + gateway readiness report.
export function checkReadyz(code, report) {
  const components = report?.components;
  if (report?.error || !Array.isArray(components) || components.length !== 2 ||
      !components.every((row) => Array.isArray(row) && row.length === 2)) {
    throw new Error("Missing bot readiness component breakdown");
  }
  const state = new Map(components);
  if (state.size !== 2 || state.get("process") !== "ready" ||
      !(code === "200" && state.get("gateway") === "ready" ||
        code === "503" && ["down", "starting"].includes(state.get("gateway")))) {
    throw new Error("Unexpected bot readiness status or components");
  }
  return code === "200" ? "gateway ready" : "gateway not ready; scaffold gate only, not E2E approval";
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  try {
    console.log(checkReadyz(process.argv[2], JSON.parse(readFileSync(process.argv[3], "utf8"))));
  } catch {
    console.error("Readiness gate failed: expected the bot's process/gateway breakdown, never an ownership refusal");
    process.exitCode = 1;
  }
}
