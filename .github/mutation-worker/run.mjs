import { spawn } from "node:child_process";
import { readFileSync, readdirSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parse } from "@babel/parser";

const MAX_MUTANTS = 50;
const RUN_MILLISECONDS = 22 * 60_000;
const TEST_MILLISECONDS = 4 * 60_000;
const reporter = fileURLToPath(new URL("./reporter.mjs", import.meta.url));
const operators = new Map(Object.entries({
  "<": "<=", "<=": "<", ">": ">=", ">=": ">", "===": "!==", "!==": "===",
  "&&": "||", "||": "&&", "+": "-", "-": "+",
}));

export function edits(source, lines) {
  const root = parse(source, { sourceType: "module", plugins: ["typescript"], tokens: true });
  const candidates = [];
  function visit(node, parent) {
    if (!node || typeof node.type !== "string" || node.type === "TSLiteralType") return;
    if (node.type === "BooleanLiteral" && lines.has(node.loc.start.line)) {
      candidates.push({ line: node.loc.start.line, start: node.start, end: node.end,
        replacement: node.value ? "false" : "true" });
    }
    const standaloneLogic = node.type === "LogicalExpression" &&
      ![parent, node.left, node.right].some((operand) => operand?.type === "LogicalExpression");
    if ((node.type === "BinaryExpression" || standaloneLogic) && operators.has(node.operator)) {
      const token = root.tokens.find(({ start, end }) => start >= node.left.end && end <= node.right.start &&
        source.slice(start, end) === node.operator);
      if (token && lines.has(token.loc.start.line)) candidates.push({
        line: token.loc.start.line, start: token.start, end: token.end, replacement: ` ${operators.get(node.operator)} `,
      });
    }
    for (const value of Object.values(node)) {
      if (Array.isArray(value)) value.forEach((child) => visit(child, node));
      else visit(value, node);
    }
  }
  visit(root.program);
  return candidates;
}

function testResult(root, args, timeoutMs) {
  return new Promise((done) => {
    const child = spawn(process.execPath, [`--test-reporter=${reporter}`, ...args], {
      cwd: root, detached: true, stdio: ["ignore", "pipe", "ignore"],
    });
    const summary = new Map();
    let expired = false;
    createInterface({ input: child.stdout }).on("line", (line) => {
      const match = /^# (pass|fail|cancelled|wrapper-fail) (\d+)$/.exec(line);
      if (match) summary.set(match[1], Number(match[2]));
    });
    const timeout = setTimeout(() => {
      expired = true;
      if (child.pid) {
        try { process.kill(-child.pid, "SIGKILL"); }
        catch (error) { if (error.code !== "ESRCH") done("infrastructure_error"); }
      }
    }, timeoutMs);
    child.once("error", () => {
      clearTimeout(timeout);
      done("infrastructure_error");
    });
    child.once("close", (code) => {
      clearTimeout(timeout);
      const pass = summary.get("pass");
      const fail = summary.get("fail");
      if (expired) done("timeout");
      else if (summary.get("cancelled") !== 0 || summary.get("wrapper-fail") !== 0) done("infrastructure_error");
      else if (code === 1 && fail > 0) done("failed");
      else if (code === 0 && fail === 0 && pass > 0) done("passed");
      else if (code === 0 && fail === 0 && pass === 0) done("unsupported");
      else done("infrastructure_error");
    });
  });
}

export async function measure(root, changed, testArgs) {
  const deadline = performance.now() + RUN_MILLISECONDS;
  const results = [];
  const plans = [];
  for (const [file, added] of Object.entries(changed)) {
    const path = resolve(root, file);
    if (!/^wrangler\/src\/[^\r\n]+\.ts$/.test(file) || !path.startsWith(join(root, "wrangler", "src") + "/") ||
        realpathSync(path) !== path || !statSync(path).isFile()) {
      throw new Error("unexpected Worker mutation source path");
    }
    const source = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(readFileSync(path));
    const candidates = /\r(?!\n)|[\u2028\u2029]/u.test(source) ? [] : edits(source, new Set(added));
    if (candidates.length) plans.push({ file, path, source, candidates });
    else results.push({ file, status: "unsupported", reason: "no supported added operator" });
  }
  const count = plans.reduce((total, plan) => total + plan.candidates.length, 0);
  if (count > MAX_MUTANTS) return [{ status: "infrastructure_error", reason: "Worker candidate budget exceeded" }];
  if (count === 0) return results;
  const testRoot = join(root, "wrangler");
  const baseline = await testResult(testRoot, testArgs, TEST_MILLISECONDS);
  if (baseline !== "passed") return [{
    status: baseline === "unsupported" ? baseline : "infrastructure_error",
    reason: { unsupported: "baseline ran zero tests", timeout: "unmutated Worker suite timed out" }[baseline]
      ?? "unmutated Worker suite failed",
  }];
  for (const { file, path, source, candidates } of plans) {
    for (const { line, start, end, replacement } of candidates) {
      const remaining = deadline - performance.now();
      if (remaining <= 0) return [...results, { status: "infrastructure_error", reason: "Worker run budget exceeded" }];
      let test;
      try {
        writeFileSync(path, source.slice(0, start) + replacement + source.slice(end));
        test = await testResult(testRoot, testArgs, Math.min(TEST_MILLISECONDS, remaining));
      } finally {
        writeFileSync(path, source);
      }
      const status = { passed: "survived", failed: "killed" }[test] ?? "infrastructure_error";
      results.push({ file, line, offset: start, status,
        ...(test === "timeout" ? { reason: "mutant test run timed out" } : {}) });
    }
  }
  return results;
}

export function verdict(results, mode) {
  if (mode === "warn") return 0;
  if (results.some(({ status }) => status === "infrastructure_error")) return 2;
  return Number(results.some(({ status }) => status === "survived"));
}

async function main([root, plan, receipt, mode]) {
  if (process.env.GITHUB_ACTIONS !== "true" || !["warn", "hard"].includes(mode)) {
    throw new Error("Worker mutations require GitHub Actions and an explicit mode");
  }
  let results;
  try {
    const files = JSON.parse(readFileSync(plan, "utf8"));
    const tests = readdirSync(join(root, "wrangler/test")).filter((file) => file.endsWith(".test.ts")).sort();
    if (tests.length === 0) throw new Error("Worker test inventory is empty");
    results = await measure(realpathSync(root), files, ["--import", "./test/cloudflare-loader.mjs",
      "--test", ...tests.map((file) => `test/${file}`)]);
  } catch {
    results = [{ status: "infrastructure_error", reason: "Worker scope, engine, or report failed" }];
  }
  writeFileSync(receipt, JSON.stringify({ results }, null, 2) + "\n");
  for (const status of ["killed", "survived", "unsupported", "infrastructure_error"]) {
    console.log(`Worker mutation ${status}=${results.filter((result) => result.status === status).length}`);
  }
  if (verdict(results, "hard")) console.log("::warning::Worker mutation found survivors or incomplete measurements");
  process.exitCode = verdict(results, mode);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main(process.argv.slice(2));
}
