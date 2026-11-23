// Match the existing staging plan's sorted, compact target JSON. The staging
// runtime proof supports the approved direct PlanetScale target. Fail closed on other URL
// shapes, including connection options that can override the effective target.
const REQUEST_FINGERPRINT = /^\?runtime_db_fingerprint=([0-9a-f]{64})$/;

async function stagingDatabaseFingerprint(raw: string | undefined): Promise<string | null> {
  if (!raw || raw.length > 4096 || /[^\x21-\x7e]/.test(raw) ||
      !/^postgres(?:ql)?:\/\/[^/?#]+\/[A-Za-z0-9_.-]{1,63}(?:\?[^#]*)?$/.test(raw)) return null;
  let target: URL;
  let username: string;
  try {
    target = new URL(raw);
    username = decodeURIComponent(target.username);
  } catch { return null; }
  const host = target.hostname.toLowerCase();
  const database = target.pathname.slice(1);
  // On this provider the endpoint alone cannot identify the branch. The last
  // username component is the non-secret branch pin; omit the login role.
  const branch = /^[A-Za-z0-9_.-]{1,63}\.([A-Za-z0-9_-]{1,63})$/.exec(username)?.[1];
  if (!/^[a-z0-9][a-z0-9-]{0,62}\.pg\.psdb\.cloud$/.test(host) ||
      host.includes("-pooler") || host.includes("prod") ||
      (target.port !== "" && target.port !== "5432") ||
      !/^[A-Za-z0-9_.-]{1,63}$/.test(database) || database.toLowerCase().includes("prod") ||
      !branch || !target.password) return null;
  const options = new Set<string>();
  for (const [key, value] of target.searchParams) {
    if (options.has(key)) return null;
    const validOption = (key === "sslmode" && /^(require|verify-ca|verify-full)$/.test(value)) ||
      (key === "channel_binding" && /^(require|prefer|disable)$/.test(value));
    if (!validOption) return null;
    options.add(key);
  }
  const canonical = JSON.stringify({ branch_id: branch, database, host });
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical));
  return Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, "0")).join("");
}

export async function stagingDatabaseEvidence(search: string, raw: string | undefined) {
  const expected = REQUEST_FINGERPRINT.exec(search)?.[1];
  if (!expected) return {};
  const actual = await stagingDatabaseFingerprint(raw);
  // Only publish the approved digest on an exact match. A refusal never
  // exposes the other target's digest or any part of the credential.
  return { runtimeDatabaseFingerprint: actual === expected ? expected : null };
}
