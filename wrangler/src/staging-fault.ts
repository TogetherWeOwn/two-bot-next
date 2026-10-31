/**
 * Staging-only fault injector for the guarded auto-remediation soak.
 *
 * The Worker cannot stall itself, so the soak arms synthetic trigger input
 * through the generated staging deploy config only. While armed, the
 * keepalive tick holds `two_bot_voice_creator_map_generation` live with an
 * age past the two-interval staleness bound, and/or advances
 * `two_bot_gateway_disconnects_total` with no recovery so the unpaired rule
 * sees an outstanding disconnect. The overlay runs between the scrape and
 * `evaluateMetrics`, so the alert rules and the remediation guard run
 * unmodified; disarming restores the scraped samples and the triggers
 * resolve on their own rules.
 *
 * Machine-contract boundaries (mirroring the gateway-readiness drill
 * switch): staging fence marker plus exact `"1"`, Worker-only (never
 * forwarded to the container, never logged), never a committed
 * `wrangler.toml` var (see `wrangler/scripts/check-env-bindings.py`) — only
 * the generated per-deploy staging config from a manual drill dispatch may
 * carry these switches. No new endpoint, credential, spend, or authority.
 */

import { CREATOR_MAP_STALE_SECONDS, type MetricsAlertState, type Sample } from "./alert-rules.ts";

/** Staging drill switch holding a stale creator map. Generated config only. */
export const VOICE_STALL_FAULT_VAR = "TWO_VOICE_STALL_FAULT_INJECTION";
/** Staging drill switch holding an unpaired gateway disconnect. Generated config only. */
export const UNPAIRED_FAULT_VAR = "TWO_GATEWAY_UNPAIRED_FAULT_INJECTION";

/** Held generation: any landed merge counts as a live map, so 1 is the smallest live value. */
export const FAULT_CREATOR_MAP_GENERATION = 1;
/** Held age: one minute past the two-interval staleness bound, with margin against tick jitter. */
export const FAULT_CREATOR_MAP_AGE_SECONDS = CREATOR_MAP_STALE_SECONDS + 60;

/** The only env the injector reads: the staging fence marker plus the two drill switches. */
export interface StagingFaultEnv {
  TWO_DEPLOY_ENV?: string;
  TWO_VOICE_STALL_FAULT_INJECTION?: string;
  TWO_GATEWAY_UNPAIRED_FAULT_INJECTION?: string;
}

/** Which drills are armed. Anything but staging plus exact `"1"` stays dark. */
export function stagingFaultArmed(env: StagingFaultEnv): {
  voiceStall: boolean;
  unpairedDisconnect: boolean;
} {
  const staging = env.TWO_DEPLOY_ENV === "staging";
  return {
    voiceStall: staging && env.TWO_VOICE_STALL_FAULT_INJECTION === "1",
    unpairedDisconnect: staging && env.TWO_GATEWAY_UNPAIRED_FAULT_INJECTION === "1",
  };
}

function upsertGauge(samples: Sample[], name: string, value: number): Sample[] {
  const row: Sample = { name, labels: {}, value };
  const idx = samples.findIndex((s) => s.name === name && Object.keys(s.labels).length === 0);
  if (idx === -1) return [...samples, row];
  return samples.map((s, i) => (i === idx ? row : s));
}

function disconnectValue(samples: Sample[]): number | undefined {
  const rows = samples.filter(
    (s) => s.name === "two_bot_gateway_disconnects_total" && Object.keys(s.labels).length === 0,
  );
  if (rows.length !== 1) return undefined;
  const value = rows[0]!.value;
  return Number.isSafeInteger(value) && value >= 0 ? value : undefined;
}

/**
 * Overlay synthetic drill input onto scraped samples. Returns the input
 * unchanged (same reference) while dark. While armed, the voice drill holds
 * the creator-map gauges past the staleness bound, and the unpaired drill
 * advances the disconnect counter past its stored baseline with no recovery
 * — a flat container then reads as an outstanding disconnect every tick, and
 * a real RESUMED or fresh READY still clears it. The stored baseline ratchets
 * with the drill, so the first disarmed tick holds (apparent restart) and
 * the next resolves; the remediation guard resets on that resolve.
 */
export function applyStagingFaultInjection(
  samples: Sample[],
  prev: MetricsAlertState,
  env: StagingFaultEnv,
): Sample[] {
  const armed = stagingFaultArmed(env);
  if (!armed.voiceStall && !armed.unpairedDisconnect) return samples;
  let out = samples;
  if (armed.voiceStall) {
    out = upsertGauge(out, "two_bot_voice_creator_map_generation", FAULT_CREATOR_MAP_GENERATION);
    out = upsertGauge(out, "two_bot_voice_creator_map_age_seconds", FAULT_CREATOR_MAP_AGE_SECONDS);
  }
  if (armed.unpairedDisconnect) {
    const scraped = disconnectValue(out);
    if (scraped !== undefined) {
      const advance = prev.gatewayUnpairedSeen ? 1 : 0;
      const faulted = Math.max(scraped, prev.gatewayDisconnects + advance);
      if (faulted !== scraped) {
        out = upsertGauge(out, "two_bot_gateway_disconnects_total", faulted);
      }
    }
  }
  return out;
}
