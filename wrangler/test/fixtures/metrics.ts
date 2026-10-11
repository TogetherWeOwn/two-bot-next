import { readFileSync } from "node:fs";

const rust = readFileSync(new URL("../../../crates/core/src/metrics.rs", import.meta.url), "utf8");

export function rustLabels(name: string): string[] {
  const list = new RegExp(`pub const ${name}: &\\[&str\\] = &\\[([\\s\\S]*?)\\];`).exec(rust);
  if (!list) throw new Error(`missing Rust metric dimension ${name}`);
  return [...list[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!);
}

export const RUST_ALERT_AXES: Record<string, Record<string, string>> = {
  two_bot_job_last_success_timestamp_seconds: { job: "JOBS" },
  two_bot_job_consecutive_failures: { job: "JOBS" },
  two_bot_rest_requests_total: { route: "REST_ROUTES", result: "RESULTS" },
  two_bot_db_pool_configured: {},
  two_bot_db_pool_connections: {},
  two_bot_db_pool_idle_connections: {},
  two_bot_db_pool_max_connections: {},
  two_bot_db_errors_total: { op: "DB_ERROR_OPS" },
  two_bot_send_admissions_total: { outcome: "SEND_ADMISSION_OUTCOMES" },
  two_bot_voice_operations_total: { op: "VOICE_OPERATIONS", outcome: "VOICE_OUTCOMES" },
  two_bot_voice_dead_letters_total: { action: "VOICE_DEAD_ACTIONS" },
  two_bot_voice_orphans_total: {},
  two_bot_gateway_missed_events_total: {},
  two_bot_dispatch_drops_total: { lane: "DISPATCH_LANES" },
  two_bot_internal_actions_total: { family: "INTERNAL_ACTION_FAMILIES", outcome: "INTERNAL_ACTION_OUTCOMES" },
};

/** Complete consumed-input fixture generated from the Rust label contract, not the validator. */
export function metricsLines(rankFailures = 0): string[] {
  const lines: string[] = [];
  for (const [name, axes] of Object.entries(RUST_ALERT_AXES)) {
    let rows: Record<string, string>[] = [{}];
    for (const [label, dimension] of Object.entries(axes)) {
      rows = rows.flatMap((row) => rustLabels(dimension).map((value) => ({ ...row, [label]: value })));
    }
    for (const labels of rows) {
      const fields = Object.entries(labels).map(([k, v]) => `${k}="${v}"`).join(",");
      const value = name === "two_bot_job_consecutive_failures" && labels.job === "rank" ? rankFailures : 0;
      lines.push(`${name}${fields ? `{${fields}}` : ""} ${value}`);
    }
  }
  return lines;
}

export function metricsBody(rankFailures = 0): string {
  return [...metricsLines(rankFailures), "two_bot_gateway_latency_seconds NaN", ""].join("\n");
}
