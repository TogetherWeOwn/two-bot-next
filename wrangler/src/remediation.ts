/**
 * Guarded auto-remediation recommendations for voice stall / crash loop.
 *
 * Design constraint: the Worker cannot restart containers. Nothing here
 * mutates the container, the ownership record, or any deployment. The Worker
 * only *recommends*: when a mapped alert firing is present, the keepalive
 * tick emits one public-safe recommendation line (Worker log plus the same
 * optional webhook the alert itself uses) and records the guard state in DO
 * storage. Execution stays inside the existing machine contract: the bot's
 * own in-process creator-map reload (maintenance tick, no host action), then
 * — if the alert is still firing — one Operator host-action card for a
 * same-SHA deployment-takeover via the existing deploy workflow, never a new
 * endpoint, credential, or spend. See docs/runbook.md#auto-remediation-guarded-stall-and-crash-loop-response.
 */

import { RUNBOOK_BASE_URL } from "./alert-rules.ts";

/** Independent remediation domains; each owns its phase/failure guard. */
export type RemediationScope = "voice-stall" | "crash-loop";

/** Remediation steps, weakest first. */
export type RemediationAction = "creator-map-reload" | "same-sha-takeover";

/** At most one recommendation per this window across all scopes. */
export const REMEDIATION_COOLDOWN_SECONDS = 1800;
/** Consecutive failed remediations per scope before the scope goes quiet. */
export const REMEDIATION_MAX_CONSECUTIVE_FAILURES = 2;

export type RemediationPhase = "idle" | "reload-recommended" | "takeover-recommended";

export interface ScopeGuard {
  phase: RemediationPhase;
  consecutiveFailures: number;
  /** True after the failure bound: no further recommendations until recovery. */
  suppressed: boolean;
}

export interface RemediationState {
  /** Seconds since epoch of the last emitted recommendation, 0 when never. */
  lastRecommendedAtSeconds: number;
  scopes: Record<RemediationScope, ScopeGuard>;
}

const IDLE_GUARD: ScopeGuard = { phase: "idle", consecutiveFailures: 0, suppressed: false };

export const EMPTY_REMEDIATION_STATE: RemediationState = {
  lastRecommendedAtSeconds: 0,
  scopes: {
    "voice-stall": { ...IDLE_GUARD },
    "crash-loop": { ...IDLE_GUARD },
  },
};

const REMEDIATION_PHASES: readonly RemediationPhase[] = [
  "idle",
  "reload-recommended",
  "takeover-recommended",
];

function isScopeGuard(value: unknown): value is ScopeGuard {
  if (typeof value !== "object" || value === null) return false;
  const guard = value as Record<string, unknown>;
  return (
    REMEDIATION_PHASES.includes(guard["phase"] as RemediationPhase) &&
    typeof guard["consecutiveFailures"] === "number" &&
    Number.isFinite(guard["consecutiveFailures"]) &&
    typeof guard["suppressed"] === "boolean"
  );
}

/**
 * DO storage outlives any one Worker version, so a partial write or a skew
 * between the stored shape and this code must restart the guard, never wedge
 * it: a missing scope would otherwise spread into `consecutiveFailures: NaN`,
 * which never reaches the failure bound and recommends forever. Anything
 * that is not exactly this shape falls back to `EMPTY_REMEDIATION_STATE`;
 * the caller's `put` then persists the restarted guard.
 */
/** Clock-skew tolerance for a stored recommendation timestamp (seconds). */
const SANITIZE_FUTURE_SKEW_TOLERANCE_SECONDS = 300;

export function sanitizeRemediationState(stored: unknown, nowSeconds: number): RemediationState {
  if (typeof stored !== "object" || stored === null) return EMPTY_REMEDIATION_STATE;
  const state = stored as Record<string, unknown>;
  const lastRecommendedAtSeconds = state["lastRecommendedAtSeconds"];
  // A non-finite timestamp wedges the cooldown check silent (`now - Infinity`
  // is always inside the window), and a far-future one — e.g. a
  // milliseconds-epoch writer against second-epoch readers — does the same
  // for decades. Neither can come from this Worker's own `put`, so both
  // restart the guard. A moderately stale or slightly future timestamp is
  // left alone: at worst it holds or spends one window, never silence.
  if (
    typeof lastRecommendedAtSeconds !== "number" ||
    !Number.isFinite(lastRecommendedAtSeconds) ||
    lastRecommendedAtSeconds > nowSeconds + SANITIZE_FUTURE_SKEW_TOLERANCE_SECONDS
  ) {
    return EMPTY_REMEDIATION_STATE;
  }
  const scopes = state["scopes"];
  if (typeof scopes !== "object" || scopes === null) return EMPTY_REMEDIATION_STATE;
  const guards = scopes as Record<string, unknown>;
  if (!isScopeGuard(guards["voice-stall"]) || !isScopeGuard(guards["crash-loop"])) {
    return EMPTY_REMEDIATION_STATE;
  }
  return stored as RemediationState;
}

export interface TriggerDef {
  scope: RemediationScope;
  /** Exact firing key from `evaluateMetrics` (these rules carry no subject). */
  firingKey: string;
  /** First step for this scope; escalation is always `same-sha-takeover`. */
  firstAction: RemediationAction;
  /** Anchor inside docs/runbook.md. */
  runbook: string;
  summary: string;
}

export const REMEDIATION_TRIGGERS: readonly TriggerDef[] = [
  {
    scope: "voice-stall",
    firingKey: "voice_creator_map_stale",
    firstAction: "creator-map-reload",
    runbook: "runbook.md#alert-voice-creator-map-stale",
    summary: "creator joins served from a stale map; the bot's maintenance tick retries the merge every 5 minutes",
  },
  {
    scope: "crash-loop",
    firingKey: "gateway_unpaired_disconnect",
    firstAction: "same-sha-takeover",
    runbook: "runbook.md#alert-gateway-unpaired-disconnect",
    summary: "gateway disconnect with no RESUMED or fresh READY; a reload cannot fix a process that never came back",
  },
];

/** Full deep link for a trigger's runbook anchor. */
export function remediationRunbookUrl(trigger: TriggerDef): string {
  return `${RUNBOOK_BASE_URL}${trigger.runbook}`;
}

/** Operator instruction carried by each recommendation; no host action for R1. */
export function operatorInstruction(action: RemediationAction): string {
  return action === "creator-map-reload"
    ? "No host action: the bot's maintenance tick retries the creator-map merge every 5 minutes. File a host-action card only if this alert is still firing past the next tick."
    : "Operator: file one host-action card for a same-SHA deployment-takeover through the existing deploy workflow; one card per host step.";
}

export type RemediationMessageKind = "recommend" | "ceo-page";

export interface RemediationMessage {
  kind: RemediationMessageKind;
  scope: RemediationScope;
  action: RemediationAction | undefined;
  firingKey: string;
  /** Public-safe line; the caller appends the build/log suffix. */
  text: string;
}

function recommendText(trigger: TriggerDef, action: RemediationAction): string {
  return `two-bot-next REMEDIATION ${trigger.scope}: ${action}. Trigger: ${trigger.firingKey} firing (${trigger.summary}). Runbook: ${remediationRunbookUrl(trigger)}. ${operatorInstruction(action)}`;
}

function ceoPageText(trigger: TriggerDef): string {
  return `two-bot-next REMEDIATION STOPPED ${trigger.scope}: ${REMEDIATION_MAX_CONSECUTIVE_FAILURES} consecutive remediations failed for ${trigger.firingKey}. Paging the CEO card per ${RUNBOOK_BASE_URL}runbook.md#auto-remediation-guarded-stall-and-crash-loop-response. No further recommendations until this alert resolves.`;
}

export interface RemediationEvaluation {
  messages: RemediationMessage[];
  state: RemediationState;
}

/**
 * Recommend at most one remediation per cooldown window, escalate R1 to R2
 * while the alert keeps firing, and stop a scope after two consecutive
 * failures with a CEO page. A recommendation is emitted only while its
 * trigger key is in the current firing list, so remediation never speaks
 * without its alert alongside. Triggers are visited in `REMEDIATION_TRIGGERS`
 * table order and the first eligible one consumes the window, so when both
 * scopes fire at once the table order is the priority: the other scope waits
 * for the next window. A resolved trigger resets its scope guard
 * (recovery clears the failure count and the suppression). Callers must pass
 * stored guard state through `sanitizeRemediationState` first.
 */
export function evaluateRemediation(
  firing: string[],
  prev: RemediationState,
  nowSeconds: number,
): RemediationEvaluation {
  const messages: RemediationMessage[] = [];
  const scopes: Record<RemediationScope, ScopeGuard> = {
    "voice-stall": { ...prev.scopes["voice-stall"] },
    "crash-loop": { ...prev.scopes["crash-loop"] },
  };
  let lastRecommendedAtSeconds = prev.lastRecommendedAtSeconds;
  for (const trigger of REMEDIATION_TRIGGERS) {
    const guard = scopes[trigger.scope]!;
    if (!firing.includes(trigger.firingKey)) {
      if (guard.phase !== "idle" || guard.consecutiveFailures !== 0 || guard.suppressed) {
        scopes[trigger.scope] = { ...IDLE_GUARD };
      }
      continue;
    }
    if (guard.suppressed) continue;
    if (nowSeconds - lastRecommendedAtSeconds < REMEDIATION_COOLDOWN_SECONDS) continue;
    const action: RemediationAction =
      guard.phase === "idle" ? trigger.firstAction : "same-sha-takeover";
    const consecutiveFailures =
      guard.phase === "idle" ? guard.consecutiveFailures : guard.consecutiveFailures + 1;
    if (consecutiveFailures >= REMEDIATION_MAX_CONSECUTIVE_FAILURES) {
      scopes[trigger.scope] = {
        phase: "takeover-recommended",
        consecutiveFailures,
        suppressed: true,
      };
      messages.push({
        kind: "ceo-page",
        scope: trigger.scope,
        action: undefined,
        firingKey: trigger.firingKey,
        text: ceoPageText(trigger),
      });
    } else {
      scopes[trigger.scope] = {
        phase: action === "creator-map-reload" ? "reload-recommended" : "takeover-recommended",
        consecutiveFailures,
        suppressed: false,
      };
      messages.push({
        kind: "recommend",
        scope: trigger.scope,
        action,
        firingKey: trigger.firingKey,
        text: recommendText(trigger, action),
      });
    }
    lastRecommendedAtSeconds = nowSeconds;
  }
  return { messages, state: { lastRecommendedAtSeconds, scopes } };
}
