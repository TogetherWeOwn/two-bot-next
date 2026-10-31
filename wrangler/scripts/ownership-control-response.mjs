// Successful control responses are untrusted, including audit/debug fields.
const validDeploymentId = (value) => typeof value === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(value);
const record = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const has = (value, key) => Object.hasOwn(value, key);
const OWNER_FIELDS = ["deploymentId", "epoch", "phase", "actor", "timestamp", "oldEpoch", "oldDeploymentId"];

// Keep abbreviated historical records, but validate every supported supplied
// field. Only an active legacy owner may omit epoch (the caller's zero fallback).
export function parseOwnershipResponse(state) {
  if (!record(state) || !validDeploymentId(state.deploymentId) ||
      !has(state, "owner") || typeof state.running !== "boolean") return null;
  const owner = state.owner;
  if (owner === null) return { deploymentId: state.deploymentId, owner: null, running: state.running };
  if (!record(owner) || (owner.phase !== "active" && owner.phase !== "fenced") ||
      !(validDeploymentId(owner.deploymentId) || (owner.phase === "fenced" && owner.deploymentId === null))) return null;
  if (has(owner, "epoch")
      ? !Number.isSafeInteger(owner.epoch) || owner.epoch <= 0
      : owner.phase !== "active") return null;
  if (has(owner, "oldEpoch") && (!Number.isSafeInteger(owner.oldEpoch) || owner.oldEpoch < 0 ||
      (has(owner, "epoch") && owner.oldEpoch !== owner.epoch - 1))) return null;
  if (has(owner, "oldDeploymentId") && owner.oldDeploymentId !== null && !validDeploymentId(owner.oldDeploymentId)) return null;
  if (has(owner, "actor") && (typeof owner.actor !== "string" || !/^[a-zA-Z0-9_.:@/-]{1,128}$/.test(owner.actor))) return null;
  if (has(owner, "timestamp") && (typeof owner.timestamp !== "string" || !Number.isFinite(Date.parse(owner.timestamp)))) return null;
  const projectedOwner = Object.fromEntries(OWNER_FIELDS.filter((key) => has(owner, key)).map((key) => [key, owner[key]]));
  return { deploymentId: state.deploymentId, owner: projectedOwner, running: state.running };
}
