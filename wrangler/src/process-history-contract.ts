/**
 * Pure offline contract for the private whole-process history (V1).
 *
 * This module defines the closed wire schemas, exact integer arithmetic,
 * failure vocabulary and a bounded in-memory writer/reader model for a
 * 120-slot process-observation history. It is a *contract and proof model*
 * only: nothing here reads the OS, the clock, the environment, the network or
 * any storage. Callers supply every input (observations, time strings, hash
 * function, persisted-byte map), and nothing in the Worker entrypoint imports
 * this file. See docs/process-history-contract.md for the normative text.
 *
 * Structural completeness is never resource health: `acceptance` stays
 * UNDECIDED, 120 snapshots hold one baseline plus 119 adjacent CPU deltas,
 * and a synthetic write/readback proves no installed principal or backend.
 */

/** SHA-256 of an ASCII/UTF-8 string, as 64 lowercase hex characters (injected). */
export type Sha256Hex = (utf8: string) => string;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

export const U64_MAX = (1n << 64n) - 1n;
export const SLOT_COUNT = 120;
export const FIRST_OFFSET_SECONDS = 7200;
export const STEP_SECONDS = 60;
export const END_EXCLUSIVE_OFFSET_SECONDS = 14400;
export const INCIDENT_KEY_COUNT = 32;
/** Ordinals 0..30 are ordinary; ordinal 31 is reserved for the terminal stop. */
export const ORDINARY_INCIDENT_LIMIT = 31;
export const TERMINAL_INCIDENT_ORDINAL = 31;

/** Canonical byte caps per object class (UTF-8 bytes of the canonical form). */
export const CAPS = {
  descriptor: 4096,
  record: 4096,
  incident: 1024,
  seal: 4096,
  manifest: 32768,
  key: 128,
  fixture: 262144,
  procStatus: 8192,
  procStat: 4096,
  scratch: 131072,
} as const;

export const LOGICAL_BUDGET_BYTES = 1048576;
export const MAX_KEYS = 1 + SLOT_COUNT + INCIDENT_KEY_COUNT + 1 + 2 + 1;
export const MAX_KEY_BYTES = MAX_KEYS * CAPS.key;
export const MAX_VALUE_BYTES =
  CAPS.descriptor + SLOT_COUNT * CAPS.record + INCIDENT_KEY_COUNT * CAPS.incident + CAPS.seal + 2 * CAPS.manifest + CAPS.seal;
export const MAX_ACCOUNTED_BYTES = MAX_KEY_BYTES + MAX_VALUE_BYTES;

export const OPERATION_TIMEOUT_MS = 2000;
export const WRITE_ATTEMPTS_PER_KEY = 2;
export const READ_BUDGET_MS = 300_000;
export const DRAIN_BUDGET_MS = 30_000;
export const INSPECTION_LEASE_DAYS = 14;

/** Logical working-set composition behind the 131,072-byte scratch cap. */
export function scratchRequirementBytes(): number {
  return CAPS.procStatus + CAPS.procStat + CAPS.record + CAPS.manifest + (SLOT_COUNT + INCIDENT_KEY_COUNT) * 64;
}

// ---------------------------------------------------------------------------
// Failure vocabulary: 32 fixed unsigned bits
// ---------------------------------------------------------------------------

export const FLAG_NAMES = [
  "MISSING", "DUPLICATE", "OUT_OF_ORDER", "CPU_BASELINE_ONLY", "COUNTER_RESET_OR_WRAP", "IDENTITY_UNKNOWN",
  "IDENTITY_CHANGE", "CLOCK_UNKNOWN", "CLOCK_RESET", "CLOCK_DISAGREEMENT", "OUTSIDE_SLOT", "JITTER_UNACCEPTED",
  "SOURCE_UNAVAILABLE", "FAILED_WRITE", "WRITE_OUTCOME_UNKNOWN", "FAILED_READ", "READ_MISMATCH", "TRUNCATED",
  "COLLECTOR_STOP", "BYTE_BUDGET", "ROW_BUDGET", "EVENT_BUDGET", "CONCURRENT_WRITER", "INVALID_SCHEMA",
  "PRIVACY_REJECTED", "CPU_HZ_UNKNOWN", "CPU_WIDTH_UNKNOWN", "VCPU_UNKNOWN", "NONADJACENT_CPU", "DEADLINE",
  "AUTHORITY_MISSING", "OPEN_OR_UNSEALED",
] as const;
export type FlagName = (typeof FLAG_NAMES)[number];

/** Single-bit masks. Bit 31 is 2147483648: masks are unsigned, never `1 << 31`. */
export const F = Object.fromEntries(FLAG_NAMES.map((name, bit) => [name, 2 ** bit])) as Record<FlagName, number>;
export const ALL_FLAGS = 4294967295;

export function orMask(...masks: number[]): number {
  let out = 0;
  for (const m of masks) out = (out | m) >>> 0;
  return out;
}

export function hasFlag(mask: number, flag: number): boolean {
  return ((mask & flag) >>> 0) !== 0;
}

export function isMask(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= ALL_FLAGS;
}

export function maskNames(mask: number): FlagName[] {
  const out: FlagName[] = [];
  FLAG_NAMES.forEach((name, bit) => {
    if (Math.floor(mask / 2 ** bit) % 2 === 1) out.push(name);
  });
  return out;
}

/**
 * Flags a structurally complete run may still carry: the baseline slot, the
 * undefined jitter acceptance, and qualification gaps that only null a rate.
 * Everything else (loss, mismatch, identity/clock/stop/budget/privacy) blocks it.
 */
export const STRUCTURAL_TOLERATED_FLAGS = orMask(
  F.CPU_BASELINE_ONLY, F.JITTER_UNACCEPTED, F.CPU_HZ_UNKNOWN, F.CPU_WIDTH_UNKNOWN, F.VCPU_UNKNOWN,
);

/** Flags only the writer's arrival order or neighbouring record can add. */
const CONTEXT_FLAGS = orMask(
  F.OUT_OF_ORDER, F.NONADJACENT_CPU, F.CPU_BASELINE_ONLY, F.COUNTER_RESET_OR_WRAP, F.CLOCK_RESET,
  F.CLOCK_DISAGREEMENT, F.IDENTITY_CHANGE, F.INVALID_SCHEMA,
);

/** Adverse facts only the writer observed; the reader carries a claimed one into its own flags, mismatch or not. */
const WRITER_FACT_FLAGS = orMask(
  F.DUPLICATE, F.FAILED_WRITE, F.WRITE_OUTCOME_UNKNOWN, F.COLLECTOR_STOP, F.CONCURRENT_WRITER, F.EVENT_BUDGET,
  F.BYTE_BUDGET, F.ROW_BUDGET, F.PRIVACY_REJECTED,
);

export type Checked<T> = { ok: true; value: T } | { ok: false; flags: number };

function fail(...flags: number[]): { ok: false; flags: number } {
  return { ok: false, flags: orMask(...flags) };
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

const RE_U64 = /^(?:0|[1-9][0-9]{0,19})$/;
const RE_UTC = /^([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})\.([0-9]{3})Z$/;
const RE_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const RE_BUILD = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
const RE_HEX = { 32: /^[0-9a-f]{32}$/, 40: /^[0-9a-f]{40}$/, 64: /^[0-9a-f]{64}$/ } as const;

export function isU64(value: unknown): value is string {
  return typeof value === "string" && RE_U64.test(value) && BigInt(value) <= U64_MAX;
}

function isLeap(year: number): boolean {
  return (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;
}

function daysInMonth(year: number, month: number): number {
  return month === 2 ? (isLeap(year) ? 29 : 28) : [4, 6, 9, 11].includes(month) ? 30 : 31;
}

function daysFromCivil(year: number, month: number, day: number): number {
  const y = month <= 2 ? year - 1 : year;
  const era = Math.floor(y / 400);
  const yoe = y - era * 400;
  const doy = Math.floor((153 * (month + (month > 2 ? -3 : 9)) + 2) / 5) + day - 1;
  const doe = yoe * 365 + Math.floor(yoe / 4) - Math.floor(yoe / 100) + doy;
  return era * 146097 + doe - 719468;
}

function civilFromDays(days: number): [number, number, number] {
  const z = days + 719468;
  const era = Math.floor(z / 146097);
  const doe = z - era * 146097;
  const yoe = Math.floor((doe - Math.floor(doe / 1460) + Math.floor(doe / 36524) - Math.floor(doe / 146096)) / 365);
  const doy = doe - (365 * yoe + Math.floor(yoe / 4) - Math.floor(yoe / 100));
  const mp = Math.floor((5 * doy + 2) / 153);
  const day = doy - Math.floor((153 * mp + 2) / 5) + 1;
  const month = mp + (mp < 10 ? 3 : -9);
  return [yoe + era * 400 + (month <= 2 ? 1 : 0), month, day];
}

/** Epoch milliseconds of a strict `YYYY-MM-DDTHH:mm:ss.sssZ` string, or null. */
export function parseUtcMs(value: unknown): number | null {
  if (typeof value !== "string") return null;
  const m = RE_UTC.exec(value);
  if (!m) return null;
  const [year, month, day, hour, minute, second, milli] = m.slice(1).map(Number) as [number, number, number, number, number, number, number];
  if (month < 1 || month > 12 || day < 1 || day > daysInMonth(year, month)) return null;
  if (hour > 23 || minute > 59 || second > 59) return null;
  return (((daysFromCivil(year, month, day) * 24 + hour) * 60 + minute) * 60 + second) * 1000 + milli;
}

/** Inverse of `parseUtcMs`; null outside years 0000..9999. */
export function formatUtc(epochMs: number): string | null {
  if (!Number.isSafeInteger(epochMs)) return null;
  const days = Math.floor(epochMs / 86_400_000);
  const rest = epochMs - days * 86_400_000;
  const [year, month, day] = civilFromDays(days);
  if (year < 0 || year > 9999) return null;
  const pad = (n: number, w: number): string => String(n).padStart(w, "0");
  return `${pad(year, 4)}-${pad(month, 2)}-${pad(day, 2)}T${pad(Math.floor(rest / 3_600_000), 2)}:${pad(Math.floor(rest / 60_000) % 60, 2)}:${pad(Math.floor(rest / 1000) % 60, 2)}.${pad(rest % 1000, 3)}Z`;
}

/** UTF-8 byte length without allocating. */
export function utf8Length(text: string): number {
  let n = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c < 0x80) n += 1;
    else if (c < 0x800) n += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < text.length && (text.charCodeAt(i + 1) & 0xfc00) === 0xdc00) { n += 4; i++; }
    else n += 3;
  }
  return n;
}

// ---------------------------------------------------------------------------
// Strict JSON and canonical bytes
// ---------------------------------------------------------------------------

class ParseError extends Error {}

const MAX_JSON_DEPTH = 6;

/**
 * Parse the closed subset of JSON the contract uses: objects, arrays, ASCII
 * strings without escapes, non-negative safe integers, true/false/null and no
 * whitespace. Duplicate keys and anything else are errors, not repairs.
 */
export function parseStrictJson(text: string): Checked<unknown> {
  let p = 0;
  const err = (): never => { throw new ParseError(); };
  const value = (depth: number): unknown => {
    const c = text[p];
    if (c === "{" || c === "[") {
      if (depth >= MAX_JSON_DEPTH) err();
      return c === "{" ? object(depth) : array(depth);
    }
    if (c === "\"") return string();
    if (text.startsWith("true", p)) { p += 4; return true; }
    if (text.startsWith("false", p)) { p += 5; return false; }
    if (text.startsWith("null", p)) { p += 4; return null; }
    return number();
  };
  const number = (): number => {
    const m = /^(?:0|[1-9][0-9]{0,15})/.exec(text.slice(p, p + 17));
    if (!m) return err();
    p += m[0].length;
    const n = Number(m[0]);
    return Number.isSafeInteger(n) ? n : err();
  };
  const string = (): string => {
    p++;
    const start = p;
    for (;;) {
      const code = text.charCodeAt(p);
      if (Number.isNaN(code) || code < 0x20 || code > 0x7e || code === 0x5c) err();
      if (code === 0x22) break;
      p++;
    }
    const s = text.slice(start, p);
    p++;
    return s;
  };
  const object = (depth: number): unknown => {
    p++;
    const out: Record<string, unknown> = {};
    if (text[p] === "}") { p++; return out; }
    for (;;) {
      if (text[p] !== "\"") err();
      const key = string();
      if (Object.hasOwn(out, key) || text[p] !== ":") err();
      p++;
      // defineProperty keeps a "__proto__" key an ordinary own property instead of a prototype write.
      Object.defineProperty(out, key, { value: value(depth + 1), enumerable: true, writable: true, configurable: true });
      if (text[p] === ",") { p++; continue; }
      if (text[p] === "}") { p++; return out; }
      err();
    }
  };
  const array = (depth: number): unknown => {
    p++;
    const out: unknown[] = [];
    if (text[p] === "]") { p++; return out; }
    for (;;) {
      out.push(value(depth + 1));
      if (text[p] === ",") { p++; continue; }
      if (text[p] === "]") { p++; return out; }
      err();
    }
  };
  try {
    const v = value(0);
    return p === text.length ? { ok: true, value: v } : fail(F.INVALID_SCHEMA);
  } catch (e) {
    if (e instanceof ParseError) return fail(F.INVALID_SCHEMA);
    throw e;
  }
}

/** Compact UTF-8 JSON: keys sorted, no whitespace, integers only. Throws on non-contract values. */
export function canonicalize(value: unknown): string {
  if (value === null) return "null";
  switch (typeof value) {
    case "boolean": return value ? "true" : "false";
    case "number":
      if (!Number.isSafeInteger(value) || value < 0) throw new RangeError("non-canonical number");
      return String(value);
    case "string": return JSON.stringify(value);
    case "object": {
      if (Array.isArray(value)) return `[${value.map(canonicalize).join(",")}]`;
      const obj = value as Record<string, unknown>;
      return `{${Object.keys(obj).sort().map((k) => `${JSON.stringify(k)}:${canonicalize(obj[k])}`).join(",")}}`;
    }
    default: throw new TypeError("non-canonical value");
  }
}

// ---------------------------------------------------------------------------
// Closed-schema rules
// ---------------------------------------------------------------------------

type Rule = (v: unknown) => number;
const OK = 0;
const BAD = F.INVALID_SCHEMA;

const FORBIDDEN_KEY_PARTS = [
  "pid", "comm", "boot", "cmdline", "argv", "env", "token", "secret", "password", "credential", "authorization",
  "url", "uri", "host", "path", "sql", "query", "message", "error", "reason", "detail", "content", "body",
  "discord", "guild", "channel", "snowflake", "user_id", "userid", "member", "process", "api_key", "apikey", "text",
];

function forbiddenKey(key: string): boolean {
  const k = key.toLowerCase();
  return FORBIDDEN_KEY_PARTS.some((part) => k.includes(part));
}

const PRIVATE_VALUE = /[:/\\@]|\s|bearer|token|secret|password|authorization|select|insert|delete|update|drop|^[0-9]{17,20}$/i;

function bad(v: unknown): number {
  return typeof v === "string" && PRIVATE_VALUE.test(v) ? orMask(BAD, F.PRIVACY_REJECTED) : BAD;
}

function isRecordLike(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

const rStr = (re: RegExp): Rule => (v) => (typeof v === "string" && re.test(v) ? OK : bad(v));
const rHex = (n: 32 | 40 | 64): Rule => rStr(RE_HEX[n]);
const rU64: Rule = (v) => (isU64(v) ? OK : bad(v));
const rUtc: Rule = (v) => (parseUtcMs(v) !== null ? OK : bad(v));
const rInt = (lo: number, hi: number): Rule => (v) => (typeof v === "number" && Number.isInteger(v) && v >= lo && v <= hi ? OK : BAD);
const rLit = (x: string | number | boolean): Rule => (v) => (v === x ? OK : bad(v));
const rEnum = (xs: readonly string[]): Rule => (v) => (typeof v === "string" && xs.includes(v) ? OK : bad(v));
const nullable = (r: Rule): Rule => (v) => (v === null ? OK : r(v));
const rMask: Rule = (v) => (isMask(v) ? OK : BAD);
const rNull: Rule = (v) => (v === null ? OK : BAD);

const rShape = (shape: Record<string, Rule>): Rule => (v) => {
  if (!isRecordLike(v)) return BAD;
  let flags = OK;
  for (const k of Object.keys(v)) {
    if (!Object.hasOwn(shape, k)) flags = orMask(flags, BAD, forbiddenKey(k) ? F.PRIVACY_REJECTED : 0);
  }
  for (const [k, rule] of Object.entries(shape)) {
    flags = orMask(flags, Object.hasOwn(v, k) ? rule(v[k]) : BAD);
  }
  return flags;
};

const IDENTITY_SHAPE: Record<string, Rule> = {
  bot_source_sha: nullable(rHex(40)),
  worker_source_sha: nullable(rHex(40)),
  bot_build_id: nullable((v) => (typeof v === "string" && v !== "unknown" && RE_BUILD.test(v) ? OK : bad(v))),
  container_image_sha256: nullable(rHex(64)),
  worker_version_id: nullable(rStr(RE_UUID)),
  process_alias: nullable(rHex(32)),
  process_start_ticks: nullable(rU64),
  startup_observed_utc: nullable(rUtc),
  startup_mono_ns: nullable(rU64),
  clock_kind: rEnum(["linux_clock_monotonic", "unavailable"]),
  cpu_counter_width_bits: nullable(rInt(1, 1024)),
  cpu_ticks_per_second: nullable(rInt(1, 1_000_000)),
  allocated_vcpu_milli: nullable(rInt(1, 1_000_000)),
  limits_receipt_sha256: nullable(rHex(64)),
};
export const IDENTITY_FIELDS = Object.keys(IDENTITY_SHAPE);

/** Provenance every qualified record must carry; null in any one means unknown. */
export const PROVENANCE_FIELDS = [
  "bot_source_sha", "worker_source_sha", "bot_build_id", "container_image_sha256", "worker_version_id",
  "process_alias", "process_start_ticks", "startup_observed_utc", "startup_mono_ns",
] as const;

const OBSERVATION_SHAPE: Record<string, Rule> = {
  utc_start: rUtc,
  utc_end: rUtc,
  mono_start_ns: rU64,
  mono_end_ns: rU64,
  cpu_anchor_mono_ns: nullable(rU64),
  rss_bytes: nullable(rU64),
  cpu_user_ticks: nullable(rU64),
  cpu_system_ticks: nullable(rU64),
  source: rEnum(["linux_proc_self_v1", "unavailable"]),
};

const INTERVAL_SHAPE: Record<string, Rule> = {
  previous_i: nullable(rInt(0, SLOT_COUNT - 2)),
  start_anchor_mono_ns: nullable(rU64),
  end_anchor_mono_ns: nullable(rU64),
  delta_mono_ns: nullable(rU64),
  delta_cpu_ticks: nullable(rU64),
  one_core_micropercent: nullable(rU64),
  allocated_micropercent: nullable(rU64),
};

const RECORD_SHAPE: Record<string, Rule> = {
  schema: rLit("b2proc.record.v1"),
  run_alias: rHex(32),
  fixture_alias: rHex(32),
  writer_alias: rHex(32),
  i: rInt(0, SLOT_COUNT - 1),
  scheduled_offset_seconds: rInt(0, END_EXCLUSIVE_OFFSET_SECONDS),
  scheduled_utc: nullable(rUtc),
  writer_received_utc: nullable(rUtc),
  write_seq: nullable(rInt(0, SLOT_COUNT - 1)),
  identity: rShape(IDENTITY_SHAPE),
  observation: nullable(rShape(OBSERVATION_SHAPE)),
  cpu_interval: rShape(INTERVAL_SHAPE),
  flags_mask: rMask,
};

const AUTHORITY_KEYS = [
  "technical_decision", "security_disposition", "implementation_admission", "runtime_admission", "storage_use",
  "source_read", "independent_readback", "fixture_and_observer", "T_decision",
] as const;

const DESCRIPTOR_SHAPE: Record<string, Rule> = {
  schema: rLit("b2proc.run.v1"),
  contract_sha256: rHex(64),
  mode: rEnum(["offline", "staging"]),
  run_alias: rHex(32),
  fixture_alias: rHex(32),
  writer_alias: rHex(32),
  reader_alias: rHex(32),
  T_utc: nullable(rUtc),
  T_evidence_sha256: nullable(rHex(64)),
  schedule: rShape({
    first_offset_seconds: rLit(FIRST_OFFSET_SECONDS),
    step_seconds: rLit(STEP_SECONDS),
    count: rLit(SLOT_COUNT),
    end_exclusive_offset_seconds: rLit(END_EXCLUSIVE_OFFSET_SECONDS),
  }),
  fixture_bounds: rShape({
    window_seconds: rLit(900),
    expectation_cap: rLit(20),
    receipt_cap: rLit(60),
    full_receipt_cap_is_truncated: rLit(true),
  }),
  authority: rShape(Object.fromEntries(AUTHORITY_KEYS.map((k) => [k, nullable(rHex(64))]))),
  initial_identity: rShape(IDENTITY_SHAPE),
  logical_budget_bytes: rLit(LOGICAL_BUDGET_BYTES),
  created_utc: rUtc,
};

export const INCIDENT_KINDS = [
  "duplicate_retry", "duplicate_conflict", "out_of_order", "source_failure", "write_failure", "write_unknown",
  "read_failure", "read_mismatch", "identity_change", "stop", "budget", "schema", "privacy", "authority",
  "concurrency", "deadline",
] as const;
export type IncidentKind = (typeof INCIDENT_KINDS)[number];

const INCIDENT_SHAPE: Record<string, Rule> = {
  schema: rLit("b2proc.incident.v1"),
  run_alias: rHex(32),
  ordinal: rInt(0, INCIDENT_KEY_COUNT - 1),
  i: nullable(rInt(0, SLOT_COUNT - 1)),
  observed_utc: nullable(rUtc),
  principal_alias: rHex(32),
  kind: rEnum(INCIDENT_KINDS),
  flags_mask: rMask,
  candidate_sha256: nullable(rHex(64)),
  persisted_sha256: nullable(rHex(64)),
};

const SEAL_SHAPE: Record<string, Rule> = {
  schema: rEnum(["b2proc.writer-seal.v1", "b2proc.reader-seal.v1"]),
  run_alias: rHex(32),
  principal_alias: rHex(32),
  last_i: nullable(rInt(0, SLOT_COUNT - 1)),
  outcome: rEnum(["finished", "stopped", "unknown"]),
  stopped_utc: nullable(rUtc),
  flags_mask: rMask,
  last_committed_record_sha256: nullable(rHex(64)),
  receipt_sha256: nullable(rHex(64)),
};

const WRITE_STATUSES = ["committed", "failed", "unknown", "not_attempted"] as const;
const READ_STATUSES = ["verified", "missing", "failed", "mismatch", "not_attempted"] as const;
export type WriteStatus = (typeof WRITE_STATUSES)[number];
export type ReadStatus = (typeof READ_STATUSES)[number];

const ENTRY_SHAPE: Record<string, Rule> = {
  i: rInt(0, SLOT_COUNT - 1),
  record_sha256: nullable(rHex(64)),
  write_status: rEnum(WRITE_STATUSES),
  read_status: rEnum(READ_STATUSES),
  flags_mask: rMask,
};

const MANIFEST_SHAPE: Record<string, Rule> = {
  schema: rLit("b2proc.manifest.v1"),
  kind: rEnum(["writer_claim", "independent_readback"]),
  run_alias: rHex(32),
  fixture_alias: rHex(32),
  principal_alias: rHex(32),
  contract_sha256: rHex(64),
  descriptor_sha256: rHex(64),
  T_utc: nullable(rUtc),
  created_utc: rUtc,
  read_started_utc: nullable(rUtc),
  read_finished_utc: nullable(rUtc),
  writer_seal_sha256: nullable(rHex(64)),
  entries: (v) => {
    if (!Array.isArray(v) || v.length !== SLOT_COUNT) return BAD;
    return orMask(...v.map(rShape(ENTRY_SHAPE)));
  },
  counts: rShape({
    present: rInt(0, SLOT_COUNT), missing: rInt(0, SLOT_COUNT), read_failed: rInt(0, SLOT_COUNT),
    verified: rInt(0, SLOT_COUNT), unknown: rInt(0, SLOT_COUNT), qualified_rss: rInt(0, SLOT_COUNT),
    qualified_adjacent_cpu: rInt(0, SLOT_COUNT - 1), incident_keys_present: rInt(0, INCIDENT_KEY_COUNT),
  }),
  budget: rShape({
    keys_read: rInt(0, MAX_KEYS), key_bytes: rInt(0, MAX_KEY_BYTES), value_bytes: rInt(0, MAX_VALUE_BYTES),
    accounted_bytes: rInt(0, LOGICAL_BUDGET_BYTES), physical_storage_bytes: rNull,
  }),
  flags_mask: rMask,
  coverage: rEnum(["complete_structural", "incomplete", "unknown"]),
  cpu_interval_boundary: rLit("baseline_0_adjacent_1_to_119"),
  acceptance: rLit("UNDECIDED"),
};

// ---------------------------------------------------------------------------
// Typed views of the wire objects
// ---------------------------------------------------------------------------

export interface IdentityV1 {
  bot_source_sha: string | null;
  worker_source_sha: string | null;
  bot_build_id: string | null;
  container_image_sha256: string | null;
  worker_version_id: string | null;
  process_alias: string | null;
  process_start_ticks: string | null;
  startup_observed_utc: string | null;
  startup_mono_ns: string | null;
  clock_kind: "linux_clock_monotonic" | "unavailable";
  cpu_counter_width_bits: number | null;
  cpu_ticks_per_second: number | null;
  allocated_vcpu_milli: number | null;
  limits_receipt_sha256: string | null;
}

export interface ObservationV1 {
  utc_start: string;
  utc_end: string;
  mono_start_ns: string;
  mono_end_ns: string;
  cpu_anchor_mono_ns: string | null;
  rss_bytes: string | null;
  cpu_user_ticks: string | null;
  cpu_system_ticks: string | null;
  source: "linux_proc_self_v1" | "unavailable";
}

export interface CpuIntervalV1 {
  previous_i: number | null;
  start_anchor_mono_ns: string | null;
  end_anchor_mono_ns: string | null;
  delta_mono_ns: string | null;
  delta_cpu_ticks: string | null;
  one_core_micropercent: string | null;
  allocated_micropercent: string | null;
}

export interface RecordV1 {
  schema: "b2proc.record.v1";
  run_alias: string;
  fixture_alias: string;
  writer_alias: string;
  i: number;
  scheduled_offset_seconds: number;
  scheduled_utc: string | null;
  writer_received_utc: string | null;
  write_seq: number | null;
  identity: IdentityV1;
  observation: ObservationV1 | null;
  cpu_interval: CpuIntervalV1;
  flags_mask: number;
}

export interface RunDescriptorV1 {
  schema: "b2proc.run.v1";
  contract_sha256: string;
  mode: "offline" | "staging";
  run_alias: string;
  fixture_alias: string;
  writer_alias: string;
  reader_alias: string;
  T_utc: string | null;
  T_evidence_sha256: string | null;
  schedule: { first_offset_seconds: 7200; step_seconds: 60; count: 120; end_exclusive_offset_seconds: 14400 };
  fixture_bounds: { window_seconds: 900; expectation_cap: 20; receipt_cap: 60; full_receipt_cap_is_truncated: true };
  authority: Record<(typeof AUTHORITY_KEYS)[number], string | null>;
  initial_identity: IdentityV1;
  logical_budget_bytes: 1048576;
  created_utc: string;
}

export interface IncidentV1 {
  schema: "b2proc.incident.v1";
  run_alias: string;
  ordinal: number;
  i: number | null;
  observed_utc: string | null;
  principal_alias: string;
  kind: IncidentKind;
  flags_mask: number;
  candidate_sha256: string | null;
  persisted_sha256: string | null;
}

export interface SealV1 {
  schema: "b2proc.writer-seal.v1" | "b2proc.reader-seal.v1";
  run_alias: string;
  principal_alias: string;
  last_i: number | null;
  outcome: "finished" | "stopped" | "unknown";
  stopped_utc: string | null;
  flags_mask: number;
  last_committed_record_sha256: string | null;
  receipt_sha256: string | null;
}

export interface ManifestEntryV1 {
  i: number;
  record_sha256: string | null;
  write_status: WriteStatus;
  read_status: ReadStatus;
  flags_mask: number;
}

export type Coverage = "complete_structural" | "incomplete" | "unknown";

export interface ManifestV1 {
  schema: "b2proc.manifest.v1";
  kind: "writer_claim" | "independent_readback";
  run_alias: string;
  fixture_alias: string;
  principal_alias: string;
  contract_sha256: string;
  descriptor_sha256: string;
  T_utc: string | null;
  created_utc: string;
  read_started_utc: string | null;
  read_finished_utc: string | null;
  writer_seal_sha256: string | null;
  entries: ManifestEntryV1[];
  counts: {
    present: number; missing: number; read_failed: number; verified: number; unknown: number;
    qualified_rss: number; qualified_adjacent_cpu: number; incident_keys_present: number;
  };
  budget: { keys_read: number; key_bytes: number; value_bytes: number; accounted_bytes: number; physical_storage_bytes: null };
  flags_mask: number;
  coverage: Coverage;
  cpu_interval_boundary: "baseline_0_adjacent_1_to_119";
  acceptance: "UNDECIDED";
}

// ---------------------------------------------------------------------------
// Validators (shape + cross-field rules) and canonical encode/decode
// ---------------------------------------------------------------------------

function check<T>(rule: Rule, v: unknown, extra: (t: T) => number): Checked<T> {
  const shapeFlags = rule(v);
  if (shapeFlags !== OK) return fail(shapeFlags);
  const crossFlags = extra(v as T);
  return crossFlags === OK ? { ok: true, value: v as T } : fail(crossFlags);
}

const identityRule = rShape(IDENTITY_SHAPE);
const observationRule = rShape(OBSERVATION_SHAPE);

export function identityEquals(a: IdentityV1, b: IdentityV1): boolean {
  return IDENTITY_FIELDS.every((k) => a[k as keyof IdentityV1] === b[k as keyof IdentityV1]);
}

export function validateIdentity(v: unknown): Checked<IdentityV1> {
  return check<IdentityV1>(identityRule, v, () => OK);
}

export function validateObservation(v: unknown): Checked<ObservationV1> {
  return check<ObservationV1>(observationRule, v, (o) => {
    const start = BigInt(o.mono_start_ns);
    if (BigInt(o.mono_end_ns) < start) return BAD;
    if (o.cpu_anchor_mono_ns !== null) {
      const anchor = BigInt(o.cpu_anchor_mono_ns);
      if (anchor < start || anchor > BigInt(o.mono_end_ns)) return BAD;
    }
    const quantities = [o.rss_bytes, o.cpu_user_ticks, o.cpu_system_ticks, o.cpu_anchor_mono_ns];
    return o.source === "unavailable" && quantities.some((q) => q !== null) ? BAD : OK;
  });
}

function intervalProblem(r: RecordV1): number {
  const c = r.cpu_interval;
  const fields = [c.previous_i, c.start_anchor_mono_ns, c.end_anchor_mono_ns, c.delta_mono_ns, c.delta_cpu_ticks, c.one_core_micropercent, c.allocated_micropercent];
  if (fields.every((x) => x === null)) return OK;
  if (r.i === 0 || r.observation === null || c.previous_i !== r.i - 1) return BAD;
  if (c.start_anchor_mono_ns === null || c.end_anchor_mono_ns === null || c.delta_mono_ns === null || c.delta_cpu_ticks === null) return BAD;
  if (c.allocated_micropercent !== null && c.one_core_micropercent === null) return BAD;
  const start = BigInt(c.start_anchor_mono_ns);
  const end = BigInt(c.end_anchor_mono_ns);
  if (end <= start || BigInt(c.delta_mono_ns) !== end - start || r.observation.cpu_anchor_mono_ns !== c.end_anchor_mono_ns) return BAD;
  return OK;
}

export function validateRecord(v: unknown): Checked<RecordV1> {
  return check<RecordV1>(rShape(RECORD_SHAPE), v, (r) => {
    let f = OK;
    if (r.scheduled_offset_seconds !== FIRST_OFFSET_SECONDS + STEP_SECONDS * r.i) f = BAD;
    if (hasFlag(r.flags_mask, F.CPU_BASELINE_ONLY) !== (r.i === 0)) f = BAD;
    if (r.observation === null && !hasFlag(r.flags_mask, F.SOURCE_UNAVAILABLE)) f = BAD;
    if (r.observation !== null && !validateObservation(r.observation).ok) f = BAD;
    return orMask(f, intervalProblem(r));
  });
}

export function validateDescriptor(v: unknown): Checked<RunDescriptorV1> {
  return check<RunDescriptorV1>(rShape(DESCRIPTOR_SHAPE), v, (d) => (d.writer_alias === d.reader_alias ? BAD : OK));
}

export function validateIncident(v: unknown): Checked<IncidentV1> {
  return check<IncidentV1>(rShape(INCIDENT_SHAPE), v, (e) => {
    const terminal = e.ordinal === TERMINAL_INCIDENT_ORDINAL;
    return (terminal && e.kind !== "stop" && e.kind !== "budget") || (!terminal && e.kind === "stop") ? BAD : OK;
  });
}

export function validateSeal(v: unknown): Checked<SealV1> {
  return check<SealV1>(rShape(SEAL_SHAPE), v, (s) => {
    if ((s.last_i === null) !== (s.last_committed_record_sha256 === null)) return BAD;
    // Only a writer finishes at slot 119; a reader "finishes" when its read pass ran to the end.
    return s.schema === "b2proc.writer-seal.v1" && s.outcome === "finished" && s.last_i !== SLOT_COUNT - 1 ? BAD : OK;
  });
}

/** Count fields derived from entries alone; qualified_* need the records. */
export function deriveCounts(kind: ManifestV1["kind"], entries: readonly ManifestEntryV1[]):
  Pick<ManifestV1["counts"], "present" | "missing" | "read_failed" | "verified" | "unknown"> {
  const n = (pred: (e: ManifestEntryV1) => boolean): number => entries.filter(pred).length;
  return kind === "writer_claim"
    ? {
      present: n((e) => e.write_status === "committed"),
      missing: n((e) => e.write_status === "failed" || e.write_status === "not_attempted"),
      read_failed: 0, verified: 0,
      unknown: n((e) => e.write_status === "unknown"),
    }
    : {
      present: n((e) => e.read_status === "verified" || e.read_status === "mismatch"),
      missing: n((e) => e.read_status === "missing"),
      read_failed: n((e) => e.read_status === "failed"),
      verified: n((e) => e.read_status === "verified"),
      unknown: n((e) => e.read_status === "not_attempted"),
    };
}

export function validateManifest(v: unknown): Checked<ManifestV1> {
  return check<ManifestV1>(rShape(MANIFEST_SHAPE), v, (m) => {
    const entryFlags = orMask(...m.entries.map((e) => e.flags_mask));
    if (m.entries.some((e, idx) => e.i !== idx)) return BAD;
    const derived = deriveCounts(m.kind, m.entries);
    if (Object.entries(derived).some(([k, n]) => m.counts[k as keyof typeof derived] !== n)) return BAD;
    if (m.counts.qualified_rss > m.counts.present || m.counts.qualified_adjacent_cpu > m.counts.present) return BAD;
    if (m.budget.accounted_bytes !== m.budget.key_bytes + m.budget.value_bytes) return BAD;
    if (orMask(m.flags_mask, entryFlags) !== m.flags_mask) return BAD;
    const readTimes = m.read_started_utc !== null && m.read_finished_utc !== null;
    if (readTimes && (parseUtcMs(m.read_finished_utc) ?? 0) < (parseUtcMs(m.read_started_utc) ?? 0)) return BAD;
    if (m.kind === "writer_claim") {
      if (m.entries.some((e) => e.read_status !== "not_attempted") || m.read_started_utc !== null || m.read_finished_utc !== null) return BAD;
      if (m.coverage === "complete_structural") return BAD;
    }
    if (m.coverage === "complete_structural") {
      const clean = m.flags_mask === orMask(m.flags_mask & STRUCTURAL_TOLERATED_FLAGS);
      if (!clean || m.entries.some((e) => e.read_status !== "verified") || m.writer_seal_sha256 === null || m.T_utc === null) return BAD;
    }
    return OK;
  });
}

function decode<T>(text: unknown, cap: number, validate: (v: unknown) => Checked<T>): Checked<T> {
  if (typeof text !== "string") return fail(BAD);
  if (utf8Length(text) > cap) return fail(F.BYTE_BUDGET);
  const parsed = parseStrictJson(text);
  if (!parsed.ok) return parsed;
  const valid = validate(parsed.value);
  if (!valid.ok) return valid;
  return canonicalize(valid.value) === text ? valid : fail(F.INVALID_SCHEMA);
}

function encode(value: unknown, cap: number, validate: (v: unknown) => Checked<unknown>): Checked<string> {
  const valid = validate(value);
  if (!valid.ok) return valid;
  const text = canonicalize(valid.value);
  return utf8Length(text) > cap ? fail(F.BYTE_BUDGET) : { ok: true, value: text };
}

export const decodeDescriptor = (text: unknown) => decode(text, CAPS.descriptor, validateDescriptor);
export const decodeRecord = (text: unknown) => decode(text, CAPS.record, validateRecord);
export const decodeIncident = (text: unknown) => decode(text, CAPS.incident, validateIncident);
export const decodeSeal = (text: unknown) => decode(text, CAPS.seal, validateSeal);
export const decodeManifest = (text: unknown) => decode(text, CAPS.manifest, validateManifest);
export const encodeDescriptor = (v: unknown) => encode(v, CAPS.descriptor, validateDescriptor);
export const encodeRecord = (v: unknown) => encode(v, CAPS.record, validateRecord);
export const encodeIncident = (v: unknown) => encode(v, CAPS.incident, validateIncident);
export const encodeSeal = (v: unknown) => encode(v, CAPS.seal, validateSeal);
export const encodeManifest = (v: unknown) => encode(v, CAPS.manifest, validateManifest);

/** Inclusive byte-cap check for a class of object, used by the byte-budget proofs. */
export function withinCap(cap: keyof typeof CAPS, bytes: number): boolean {
  return Number.isInteger(bytes) && bytes >= 0 && bytes <= CAPS[cap];
}

/** Total logical accounting: key bytes plus value bytes must stay below the run budget. */
export function accountedWithinBudget(keyBytes: number, valueBytes: number): boolean {
  return keyBytes >= 0 && valueBytes >= 0 && keyBytes + valueBytes <= LOGICAL_BUDGET_BYTES;
}

// ---------------------------------------------------------------------------
// Process input parsers (supplied text only; nothing is read from the OS)
// ---------------------------------------------------------------------------

/** VmRSS in bytes (kB x 1024, checked) from supplied `/proc/<pid>/status` text. */
export function parseVmRss(status: string): Checked<string> {
  if (utf8Length(status) > CAPS.procStatus) return fail(F.BYTE_BUDGET);
  const lines = status.split("\n").filter((line) => line.startsWith("VmRSS:"));
  if (lines.length === 0) return fail(F.SOURCE_UNAVAILABLE);
  const m = lines.length === 1 ? /^VmRSS:[ \t]+(0|[1-9][0-9]{0,19}) kB$/.exec(lines[0]!) : null;
  if (!m) return fail(F.INVALID_SCHEMA);
  const bytes = BigInt(m[1]!) * 1024n;
  return bytes <= U64_MAX ? { ok: true, value: bytes.toString() } : fail(F.INVALID_SCHEMA);
}

export interface ProcStatFields {
  cpu_user_ticks: string;
  cpu_system_ticks: string;
  process_start_ticks: string;
}

/**
 * Fields 14 (utime), 15 (stime) and 22 (starttime) of supplied
 * `/proc/<pid>/stat` text. The process id and `comm` are consumed to find the
 * field boundary and never returned: `comm` may hold spaces and parentheses,
 * so the field list starts after the *last* closing parenthesis.
 */
export function parseProcStat(line: string): Checked<ProcStatFields> {
  if (utf8Length(line) > CAPS.procStat) return fail(F.BYTE_BUDGET);
  const text = line.endsWith("\n") ? line.slice(0, -1) : line;
  const open = text.indexOf("(");
  const close = text.lastIndexOf(")");
  if (text.includes("\n") || !/^[0-9]+ $/.test(text.slice(0, Math.max(open, 0))) || open < 0 || close < open) return fail(F.INVALID_SCHEMA);
  const rest = text.slice(close + 1);
  if (!rest.startsWith(" ")) return fail(F.INVALID_SCHEMA);
  const fields = rest.slice(1).split(" ");
  // fields[0] is proc field 3; utime/stime/starttime are fields 14, 15, 22.
  const head = fields.slice(0, 20);
  if (head.length < 20 || head.some((f) => f === "")) return fail(F.INVALID_SCHEMA);
  const utime = fields[11]!;
  const stime = fields[12]!;
  const start = fields[19]!;
  if (![utime, stime, start].every(isU64)) return fail(F.INVALID_SCHEMA);
  return { ok: true, value: { cpu_user_ticks: utime, cpu_system_ticks: stime, process_start_ticks: start } };
}

// ---------------------------------------------------------------------------
// Exact CPU arithmetic
// ---------------------------------------------------------------------------

const MICROPERCENT_PER_UNIT = 100_000_000n;
const NS_PER_SECOND = 1_000_000_000n;

export interface CpuRates {
  one_core: bigint | null;
  allocated: bigint | null;
  /** True when a rate that was requested does not fit a u64 (it is then null, never saturated). */
  overflow: boolean;
}

/**
 * Micropercent (100% = 100000000) of one core and of the allocated vCPUs from
 * checked wide integers and the *full* numerator, floored once. A rate that
 * does not fit a u64 is null and sets `overflow`; it is never saturated. An
 * overflowing allocated rate leaves a valid one-core rate alone (the record
 * schema, though, keeps an allocated rate only next to a one-core rate).
 * Null for unusable inputs.
 */
export function cpuRates(deltaTicks: bigint, deltaNs: bigint, hz: number, allocatedMilli: number | null): CpuRates | null {
  if (deltaTicks < 0n || deltaNs <= 0n || !Number.isInteger(hz) || hz <= 0) return null;
  if (allocatedMilli !== null && (!Number.isInteger(allocatedMilli) || allocatedMilli <= 0)) return null;
  const numerator = MICROPERCENT_PER_UNIT * deltaTicks * NS_PER_SECOND;
  const oneCore = numerator / (BigInt(hz) * deltaNs);
  const allocated = allocatedMilli === null ? null : (numerator * 1000n) / (BigInt(hz) * deltaNs * BigInt(allocatedMilli));
  const oneFits = oneCore <= U64_MAX;
  const allocFits = allocated === null || allocated <= U64_MAX;
  return { one_core: oneFits ? oneCore : null, allocated: allocFits ? allocated : null, overflow: !oneFits || !allocFits };
}

// ---------------------------------------------------------------------------
// Schedule and qualification
// ---------------------------------------------------------------------------

export interface ScheduleSlot {
  i: number;
  offset_seconds: number;
  scheduled_utc: string | null;
}

/** All 120 slot offsets; scheduled times exist only for a qualified T. */
export function scheduleVector(tUtc: string | null): ScheduleSlot[] {
  const t = tUtc === null ? null : parseUtcMs(tUtc);
  return Array.from({ length: SLOT_COUNT }, (_, i) => {
    const offset = FIRST_OFFSET_SECONDS + STEP_SECONDS * i;
    return { i, offset_seconds: offset, scheduled_utc: t === null ? null : formatUtc(t + offset * 1000) };
  });
}

/** A run is armed only with a verified T (time and evidence) and, for `staging`, every authority reference. */
export function authorityFlags(d: RunDescriptorV1): number {
  if (d.T_utc === null || d.T_evidence_sha256 === null) return F.AUTHORITY_MISSING;
  if (d.mode === "staging" && AUTHORITY_KEYS.some((k) => d.authority[k] === null)) return F.AUTHORITY_MISSING;
  return OK;
}

function provenanceKnown(id: IdentityV1): boolean {
  return PROVENANCE_FIELDS.every((k) => id[k] !== null);
}

function widthQualified(id: IdentityV1): boolean {
  return id.cpu_counter_width_bits === 64 && id.limits_receipt_sha256 !== null;
}

/** Flags that follow from one record's own content and the frozen descriptor. */
export function intrinsicFlags(d: RunDescriptorV1, i: number, id: IdentityV1, obs: ObservationV1 | null): number {
  let f = OK;
  if (!provenanceKnown(id)) f = orMask(f, F.IDENTITY_UNKNOWN);
  if (!identityEquals(id, d.initial_identity)) f = orMask(f, F.IDENTITY_CHANGE);
  if (id.cpu_ticks_per_second === null || id.limits_receipt_sha256 === null) f = orMask(f, F.CPU_HZ_UNKNOWN);
  if (!widthQualified(id)) f = orMask(f, F.CPU_WIDTH_UNKNOWN);
  if (id.allocated_vcpu_milli === null || id.limits_receipt_sha256 === null) f = orMask(f, F.VCPU_UNKNOWN);
  if (id.clock_kind === "unavailable") f = orMask(f, F.CLOCK_UNKNOWN);
  if (obs === null || obs.source === "unavailable") return orMask(f, F.SOURCE_UNAVAILABLE);
  if ([obs.rss_bytes, obs.cpu_user_ticks, obs.cpu_system_ticks, obs.cpu_anchor_mono_ns].some((x) => x === null)) f = orMask(f, F.SOURCE_UNAVAILABLE);
  if (obs.cpu_anchor_mono_ns === null || BigInt(obs.cpu_anchor_mono_ns) === 0n) f = orMask(f, F.CLOCK_UNKNOWN);
  if (id.clock_kind === "linux_clock_monotonic" && obs.cpu_anchor_mono_ns !== null && id.startup_mono_ns !== null
    && BigInt(obs.cpu_anchor_mono_ns) < BigInt(id.startup_mono_ns)) f = orMask(f, F.CLOCK_RESET);
  const start = parseUtcMs(obs.utc_start)!;
  const end = parseUtcMs(obs.utc_end)!;
  if (end < start) f = orMask(f, F.CLOCK_DISAGREEMENT);
  const t = d.T_utc === null ? null : parseUtcMs(d.T_utc);
  if (t !== null) {
    const due = t + (FIRST_OFFSET_SECONDS + STEP_SECONDS * i) * 1000;
    if (start !== due) f = orMask(f, F.JITTER_UNACCEPTED);
    if (start < due || end >= due + STEP_SECONDS * 1000) f = orMask(f, F.OUTSIDE_SLOT);
    if (start >= t + END_EXCLUSIVE_OFFSET_SECONDS * 1000 || end >= t + END_EXCLUSIVE_OFFSET_SECONDS * 1000) f = orMask(f, F.DEADLINE);
  }
  return f;
}

function emptyInterval(): CpuIntervalV1 {
  return {
    previous_i: null, start_anchor_mono_ns: null, end_anchor_mono_ns: null, delta_mono_ns: null,
    delta_cpu_ticks: null, one_core_micropercent: null, allocated_micropercent: null,
  };
}

type CpuInput = { identity: IdentityV1; observation: ObservationV1 | null };

/** Everything a pair needs: known provenance, Linux clock, 64-bit width receipt, counters and a positive anchor. */
function cpuUsable(x: CpuInput): x is { identity: IdentityV1; observation: ObservationV1 } {
  const { identity: id, observation: o } = x;
  return o !== null && o.source === "linux_proc_self_v1" && provenanceKnown(id) && id.clock_kind === "linux_clock_monotonic"
    && widthQualified(id) && o.cpu_user_ticks !== null && o.cpu_system_ticks !== null
    && o.cpu_anchor_mono_ns !== null && BigInt(o.cpu_anchor_mono_ns) > 0n;
}

/**
 * CPU interval between exactly slot i-1 and slot i. Never uses i-2, a nearest
 * time, a modulo wrap, a clamp or a nominal 60 seconds.
 */
export function computeCpuInterval(i: number, cur: CpuInput, prev: CpuInput | undefined): { interval: CpuIntervalV1; flags: number } {
  const none = emptyInterval();
  if (i === 0) return { interval: none, flags: F.CPU_BASELINE_ONLY };
  if (!cpuUsable(cur)) return { interval: none, flags: OK };
  if (prev === undefined || !cpuUsable(prev)) return { interval: none, flags: F.NONADJACENT_CPU };
  if (!identityEquals(prev.identity, cur.identity)) return { interval: none, flags: F.IDENTITY_CHANGE };
  const a0 = BigInt(prev.observation.cpu_anchor_mono_ns!);
  const a1 = BigInt(cur.observation.cpu_anchor_mono_ns!);
  if (a1 <= a0) return { interval: none, flags: F.CLOCK_RESET };
  const du = BigInt(cur.observation.cpu_user_ticks!) - BigInt(prev.observation.cpu_user_ticks!);
  const ds = BigInt(cur.observation.cpu_system_ticks!) - BigInt(prev.observation.cpu_system_ticks!);
  if (du < 0n || ds < 0n || du + ds > U64_MAX) return { interval: none, flags: F.COUNTER_RESET_OR_WRAP };
  const interval: CpuIntervalV1 = {
    previous_i: i - 1,
    start_anchor_mono_ns: a0.toString(),
    end_anchor_mono_ns: a1.toString(),
    delta_mono_ns: (a1 - a0).toString(),
    delta_cpu_ticks: (du + ds).toString(),
    one_core_micropercent: null,
    allocated_micropercent: null,
  };
  const hz = cur.identity.cpu_ticks_per_second;
  if (hz === null) return { interval, flags: OK };
  const rates = cpuRates(du + ds, a1 - a0, hz, cur.identity.allocated_vcpu_milli);
  if (rates === null) return { interval, flags: F.INVALID_SCHEMA };
  // A one-core rate is kept even when the allocated rate overflows; the allocated rate needs the one-core rate.
  interval.one_core_micropercent = rates.one_core === null ? null : rates.one_core.toString();
  interval.allocated_micropercent = rates.one_core === null || rates.allocated === null ? null : rates.allocated.toString();
  return { interval, flags: rates.overflow ? F.INVALID_SCHEMA : OK };
}

/** Pure bound check for the single fixture the run may cover. */
export function checkFixtureBounds(input: { window_seconds: number; expectations: number; receipts: number }):
  { accepted: boolean; truncated: boolean; complete: boolean; flags: number } {
  const refuse = (...flags: number[]) => ({ accepted: false, truncated: false, complete: false, flags: orMask(...flags) });
  const ints = [input.window_seconds, input.expectations, input.receipts];
  if (!ints.every((n) => Number.isInteger(n) && n >= 0)) return refuse(F.INVALID_SCHEMA);
  if (input.window_seconds > 900) return refuse(F.INVALID_SCHEMA);
  if (input.expectations > 20) return refuse(F.ROW_BUDGET);
  if (input.receipts > 60) return refuse(F.ROW_BUDGET, F.TRUNCATED);
  if (input.receipts === 60) return { accepted: true, truncated: true, complete: false, flags: F.TRUNCATED };
  return { accepted: true, truncated: false, complete: true, flags: OK };
}

/** The 14-day inspection lease only ends eligibility for new work; bytes are never deleted. */
export function leaseState(sealedUtc: string, nowUtc: string): "active" | "frozen" | "invalid" {
  const sealed = parseUtcMs(sealedUtc);
  const now = parseUtcMs(nowUtc);
  if (sealed === null || now === null) return "invalid";
  return now >= sealed + INSPECTION_LEASE_DAYS * 86_400_000 ? "frozen" : "active";
}

// ---------------------------------------------------------------------------
// Fixed key space and the bounded, append-only persisted map
// ---------------------------------------------------------------------------

export type KeyKind = "descriptor" | "record" | "event" | "writer-seal" | "manifest:writer" | "manifest:reader" | "reader-seal";

const KEY_PREFIX = "two-bot:b2proc:v1:";
const RE_KEY = /^two-bot:b2proc:v1:([0-9a-f]{32}):(descriptor|writer-seal|reader-seal|manifest:writer|manifest:reader|record:([0-9]{3})|event:([0-9]{2}))$/;

export function keyFor(alias: string, kind: KeyKind, index?: number): string {
  if (kind === "record") return `${KEY_PREFIX}${alias}:record:${String(index).padStart(3, "0")}`;
  if (kind === "event") return `${KEY_PREFIX}${alias}:event:${String(index).padStart(2, "0")}`;
  return `${KEY_PREFIX}${alias}:${kind}`;
}

export function parseKey(key: string): { alias: string; kind: KeyKind; index: number | null } | null {
  const m = RE_KEY.exec(key);
  if (!m) return null;
  if (m[3] !== undefined) return Number(m[3]) < SLOT_COUNT ? { alias: m[1]!, kind: "record", index: Number(m[3]) } : null;
  if (m[4] !== undefined) return Number(m[4]) < INCIDENT_KEY_COUNT ? { alias: m[1]!, kind: "event", index: Number(m[4]) } : null;
  return { alias: m[1]!, kind: m[2] as KeyKind, index: null };
}

function capForKind(kind: KeyKind): number {
  switch (kind) {
    case "descriptor": return CAPS.descriptor;
    case "record": return CAPS.record;
    case "event": return CAPS.incident;
    case "manifest:writer":
    case "manifest:reader": return CAPS.manifest;
    default: return CAPS.seal;
  }
}

export type CreateOutcome =
  | { kind: "created"; elapsed_ms: number }
  | { kind: "exists"; elapsed_ms: number }
  | { kind: "rejected"; elapsed_ms: number; flags?: number }
  | { kind: "timeout"; elapsed_ms: number }
  | { kind: "denied" };

export type ReadOutcome =
  | { kind: "value"; value: string; elapsed_ms: number }
  | { kind: "absent"; elapsed_ms: number }
  | { kind: "error"; elapsed_ms: number }
  | { kind: "denied" };

/** The only storage surface the model sees: create-if-absent and read. No delete, overwrite or list. */
export interface PersistedStore {
  create(key: string, value: string): CreateOutcome;
  read(key: string): ReadOutcome;
}

/**
 * In-memory stand-in for the future private backend. It enforces the fixed
 * key set, one run, per-class value caps and the logical budget, so a model
 * that tried to spill, page or open a second run is refused here too.
 */
export class PersistedMap implements PersistedStore {
  private readonly entries = new Map<string, string>();
  private runAlias: string | null = null;
  private keyBytes = 0;
  private valueBytes = 0;

  create(key: string, value: string): CreateOutcome {
    const refuse = (flags: number): CreateOutcome => ({ kind: "rejected", elapsed_ms: 0, flags });
    if (utf8Length(key) > CAPS.key) return refuse(F.BYTE_BUDGET);
    const parsed = parseKey(key);
    if (parsed === null) return refuse(F.ROW_BUDGET);
    if (this.runAlias !== null && parsed.alias !== this.runAlias) return refuse(F.CONCURRENT_WRITER);
    if (this.entries.has(key)) return { kind: "exists", elapsed_ms: 0 };
    const bytes = utf8Length(value);
    if (bytes > capForKind(parsed.kind) || !accountedWithinBudget(this.keyBytes + utf8Length(key), this.valueBytes + bytes)) {
      return refuse(F.BYTE_BUDGET);
    }
    this.runAlias ??= parsed.alias;
    this.entries.set(key, value);
    this.keyBytes += utf8Length(key);
    this.valueBytes += bytes;
    return { kind: "created", elapsed_ms: 0 };
  }

  read(key: string): ReadOutcome {
    const value = this.entries.get(key);
    return value === undefined ? { kind: "absent", elapsed_ms: 0 } : { kind: "value", value, elapsed_ms: 0 };
  }

  get size(): number {
    return this.entries.size;
  }

  accountedBytes(): number {
    return this.keyBytes + this.valueBytes;
  }

  /** Serialized persisted bytes: what a distinct reader is allowed to see. */
  snapshot(): string {
    return JSON.stringify([...this.entries.entries()].sort(([a], [b]) => (a < b ? -1 : 1)));
  }

  /** Rebuild from `snapshot()` text; null if any pair breaks the bounds. */
  static fromSnapshot(text: string): PersistedMap | null {
    let pairs: unknown;
    try { pairs = JSON.parse(text); } catch { return null; }
    if (!Array.isArray(pairs)) return null;
    const map = new PersistedMap();
    for (const pair of pairs) {
      if (!Array.isArray(pair) || pair.length !== 2 || typeof pair[0] !== "string" || typeof pair[1] !== "string") return null;
      if (map.create(pair[0], pair[1]).kind !== "created") return null;
    }
    return map;
  }
}

// ---------------------------------------------------------------------------
// Bounded persistence: two same-payload attempts, one authorized reconciliation
// ---------------------------------------------------------------------------

export type PersistResult =
  | { status: "committed"; reconciled: boolean }
  | { status: "exists" }
  | { status: "conflict" }
  | { status: "failed"; flags: number }
  | { status: "unknown" }
  | { status: "denied" };

function timedOut(elapsed: number): boolean {
  return !Number.isFinite(elapsed) || elapsed < 0 || elapsed > OPERATION_TIMEOUT_MS;
}

/**
 * Create one key. A first permission denial is terminal (no retry). Rejections
 * and timeouts get at most one more identical attempt; an unresolved timeout
 * may be reconciled by one read if the caller holds read authority, and stays
 * UNKNOWN otherwise. Never overwrites.
 */
export function persistBytes(store: PersistedStore, key: string, bytes: string, reconcileAuthorized: boolean): PersistResult {
  let ambiguous = false;
  let rejected = 0;
  for (let attempt = 0; attempt < WRITE_ATTEMPTS_PER_KEY; attempt++) {
    const out = store.create(key, bytes);
    if (out.kind === "denied") return { status: "denied" };
    if (timedOut(out.elapsed_ms) || out.kind === "timeout") { ambiguous = true; continue; }
    if (out.kind === "created") return { status: "committed", reconciled: false };
    if (out.kind === "exists") {
      if (!ambiguous) return { status: "exists" };
      break;
    }
    rejected = orMask(rejected, out.flags ?? 0);
  }
  if (!ambiguous) return { status: "failed", flags: orMask(F.FAILED_WRITE, rejected) };
  if (!reconcileAuthorized) return { status: "unknown" };
  const seen = store.read(key);
  if (seen.kind === "denied") return { status: "denied" };
  if (seen.kind === "value" && !timedOut(seen.elapsed_ms)) {
    return seen.value === bytes ? { status: "committed", reconciled: true } : { status: "conflict" };
  }
  return { status: "unknown" };
}

// ---------------------------------------------------------------------------
// Coverage and manifest assembly
// ---------------------------------------------------------------------------

/**
 * Offline structural status only. `unknown` without authority or a seal,
 * `incomplete` on any loss or non-tolerated flag, `complete_structural` only
 * for an independently verified, sealed, fully present run. A writer's own
 * claim can never be complete.
 */
export function classifyCoverage(
  kind: ManifestV1["kind"], flags: number, entries: readonly ManifestEntryV1[], sealSha: string | null, tUtc: string | null,
): Coverage {
  if (hasFlag(flags, F.AUTHORITY_MISSING) || tUtc === null) return "unknown";
  if (kind === "writer_claim") return entries.every((e) => e.write_status === "committed") ? "unknown" : "incomplete";
  const blocking = (flags & ~orMask(STRUCTURAL_TOLERATED_FLAGS, F.OPEN_OR_UNSEALED)) >>> 0;
  if (blocking !== 0 || entries.some((e) => e.read_status !== "verified")) return "incomplete";
  if (hasFlag(flags, F.OPEN_OR_UNSEALED) || sealSha === null) return "unknown";
  return "complete_structural";
}

interface ManifestArgs {
  kind: ManifestV1["kind"];
  descriptor: RunDescriptorV1;
  descriptorSha: string;
  principal: string;
  created_utc: string;
  read_started_utc: string | null;
  read_finished_utc: string | null;
  writer_seal_sha256: string | null;
  entries: ManifestEntryV1[];
  qualified_rss: number;
  qualified_adjacent_cpu: number;
  incident_keys_present: number;
  keys_read: number;
  key_bytes: number;
  value_bytes: number;
  run_flags: number;
}

function assembleManifest(a: ManifestArgs): ManifestV1 {
  const flags = orMask(a.run_flags, ...a.entries.map((e) => e.flags_mask));
  return {
    schema: "b2proc.manifest.v1",
    kind: a.kind,
    run_alias: a.descriptor.run_alias,
    fixture_alias: a.descriptor.fixture_alias,
    principal_alias: a.principal,
    contract_sha256: a.descriptor.contract_sha256,
    descriptor_sha256: a.descriptorSha,
    T_utc: a.descriptor.T_utc,
    created_utc: a.created_utc,
    read_started_utc: a.read_started_utc,
    read_finished_utc: a.read_finished_utc,
    writer_seal_sha256: a.writer_seal_sha256,
    entries: a.entries,
    counts: {
      ...deriveCounts(a.kind, a.entries),
      qualified_rss: a.qualified_rss,
      qualified_adjacent_cpu: a.qualified_adjacent_cpu,
      incident_keys_present: a.incident_keys_present,
    },
    budget: {
      keys_read: a.keys_read, key_bytes: a.key_bytes, value_bytes: a.value_bytes,
      accounted_bytes: a.key_bytes + a.value_bytes, physical_storage_bytes: null,
    },
    flags_mask: flags,
    coverage: classifyCoverage(a.kind, flags, a.entries, a.writer_seal_sha256, a.descriptor.T_utc),
    cpu_interval_boundary: "baseline_0_adjacent_1_to_119",
    acceptance: "UNDECIDED",
  };
}

function untouchedEntries(flags: number, read: ReadStatus = "not_attempted"): ManifestEntryV1[] {
  return Array.from({ length: SLOT_COUNT }, (_, i) => ({ i, record_sha256: null, write_status: "not_attempted" as const, read_status: read, flags_mask: flags }));
}

function qualifiedRss(r: RecordV1): boolean {
  return r.observation !== null && r.observation.rss_bytes !== null
    && !hasFlag(r.flags_mask, orMask(F.IDENTITY_UNKNOWN, F.IDENTITY_CHANGE, F.SOURCE_UNAVAILABLE));
}

/** Manifest for a run that never armed: all 120 slots survive as not attempted, no storage touched. */
export function buildUnarmedManifest(
  d: RunDescriptorV1, sha256: Sha256Hex,
  p: { kind: ManifestV1["kind"]; principal: string; created_utc: string },
): ManifestV1 {
  const descriptorText = canonicalize(d);
  return assembleManifest({
    kind: p.kind, descriptor: d, descriptorSha: sha256(descriptorText), principal: p.principal, created_utc: p.created_utc,
    read_started_utc: null, read_finished_utc: null, writer_seal_sha256: null,
    entries: untouchedEntries(F.AUTHORITY_MISSING), qualified_rss: 0, qualified_adjacent_cpu: 0, incident_keys_present: 0,
    keys_read: 0, key_bytes: 0, value_bytes: 0, run_flags: orMask(F.AUTHORITY_MISSING, authorityFlags(d)),
  });
}

// ---------------------------------------------------------------------------
// Serial writer model
// ---------------------------------------------------------------------------

export interface WriterDeps {
  store: PersistedStore;
  sha256: Sha256Hex;
  /** Whether one same-key reconciliation read is separately permitted after an ambiguous write. */
  reconcile_read_authorized: boolean;
}

export type SlotStatus = "committed" | "duplicate_retry" | "rejected" | "refused" | "failed" | "unknown" | "denied";

export interface SlotResult {
  status: SlotStatus;
  i: number | null;
  flags: number;
  record_sha256: string | null;
  /** True when the collector is stopped after this call. */
  stopped: boolean;
}

interface Committed {
  record: RecordV1;
  bytes: string;
  sha: string;
  measure: string;
}

interface SlotOutcome {
  status: WriteStatus;
  flags: number;
  sha: string | null;
}

const SLOT_RULE = rShape({
  i: (v) => (typeof v === "number" && Number.isInteger(v) && v >= 0 ? (v < SLOT_COUNT ? OK : F.ROW_BUDGET) : BAD),
  writer_received_utc: rUtc,
  identity: identityRule,
  observation: nullable(observationRule),
});

export interface SlotInput {
  i: number;
  writer_received_utc: string;
  identity: IdentityV1;
  observation: ObservationV1 | null;
}

export interface FinishResult {
  outcome: SealV1["outcome"];
  seal: SealV1 | null;
  manifest: ManifestV1;
  seal_persisted: boolean;
  manifest_persisted: boolean;
  terminal_incident_persisted: boolean;
}

export type WriterPhase = "new" | "refused" | "open" | "stopped" | "denied" | "finished";

/**
 * One run, one serial writer. It validates before any source or storage
 * access, persists exact canonical bytes before reporting a commit, never
 * overwrites, and latches shut on the first stop trigger. A first permission
 * denial additionally forbids every later store call.
 */
export class ProcessHistoryWriter {
  private readonly deps: WriterDeps;
  private descriptor: RunDescriptorV1 | null = null;
  private descriptorText = "";
  private phaseValue: WriterPhase = "new";
  private readonly committed = new Map<number, Committed>();
  private readonly outcomes = new Map<number, SlotOutcome>();
  private readonly dupFlags = new Map<number, number>();
  private lastCommitted: number | null = null;
  private ordinary = 0;
  private eventBudget = false;
  private stopKind: IncidentKind | null = null;
  private stopFlags = 0;
  private unknownWrite = false;
  private readonly persisted: IncidentV1[] = [];
  private readonly local: IncidentV1[] = [];
  private keyBytes = 0;
  private valueBytes = 0;
  private lastReceived: string | null = null;
  private openFlags = 0;
  private result: FinishResult | null = null;

  constructor(deps: WriterDeps) {
    this.deps = deps;
  }

  get phase(): WriterPhase {
    return this.phaseValue;
  }

  get committedCount(): number {
    return this.committed.size;
  }

  get stopMask(): number {
    return this.stopFlags;
  }

  persistedIncidents(): readonly IncidentV1[] {
    return this.persisted;
  }

  localOnlyIncidents(): readonly IncidentV1[] {
    return this.local;
  }

  /** Validate the descriptor and arm. Unarmed or unauthorized descriptors cause zero store calls. */
  open(candidate: unknown): { opened: boolean; flags: number } {
    if (this.stopKind !== null) return { opened: false, flags: F.COLLECTOR_STOP };
    if (this.phaseValue !== "new") return { opened: false, flags: F.CONCURRENT_WRITER };
    const refuse = (flags: number): { opened: false; flags: number } => {
      if (this.phaseValue !== "denied") this.phaseValue = "refused";
      this.openFlags = flags;
      return { opened: false, flags };
    };
    const valid = validateDescriptor(candidate);
    if (!valid.ok) return refuse(valid.flags);
    const encoded = encodeDescriptor(valid.value);
    if (!encoded.ok) return refuse(encoded.flags);
    this.descriptor = JSON.parse(encoded.value) as RunDescriptorV1;
    this.descriptorText = encoded.value;
    const authority = authorityFlags(this.descriptor);
    if (authority !== OK) return refuse(authority);
    const result = this.persist(keyFor(this.descriptor.run_alias, "descriptor"), encoded.value, true);
    switch (result.status) {
      case "committed":
        this.phaseValue = "open";
        return { opened: true, flags: OK };
      case "exists":
      case "conflict":
        return refuse(F.CONCURRENT_WRITER);
      case "denied":
        return refuse(orMask(F.FAILED_WRITE, F.AUTHORITY_MISSING));
      case "failed":
        return refuse(result.flags);
      default:
        return refuse(F.WRITE_OUTCOME_UNKNOWN);
    }
  }

  /**
   * Stop the collector. Idempotent; committed bytes are untouched and no
   * catch-up exists. Once all 120 slots are committed nothing remains to stop.
   */
  stop(): void {
    if (this.committed.size === SLOT_COUNT) return;
    this.markStop("stop", F.COLLECTOR_STOP);
  }

  /** Submit one slot. At most one create per record key (two attempts), never a second sample. */
  record(raw: unknown): SlotResult {
    if (this.stopKind !== null || this.phaseValue === "denied" || this.phaseValue === "stopped" || this.phaseValue === "finished") {
      return { status: "refused", i: null, flags: F.COLLECTOR_STOP, record_sha256: null, stopped: true };
    }
    const d = this.descriptor;
    if (this.phaseValue !== "open" || d === null) {
      return { status: "refused", i: null, flags: F.AUTHORITY_MISSING, record_sha256: null, stopped: false };
    }
    const normalized = normalizeSlot(raw);
    const shape = SLOT_RULE(normalized);
    const slotIndex = isRecordLike(normalized) && typeof normalized.i === "number" && Number.isInteger(normalized.i) && normalized.i >= 0 && normalized.i < SLOT_COUNT ? normalized.i : null;
    const cross = shape === OK && isRecordLike(normalized) && normalized.observation !== null ? validateObservation(normalized.observation) : null;
    if (shape !== OK || (cross !== null && !cross.ok)) {
      return this.reject(slotIndex, orMask(shape, cross !== null && !cross.ok ? cross.flags : 0));
    }
    const slot = JSON.parse(canonicalize(normalized)) as SlotInput;
    const { i, identity, observation } = slot;
    this.lastReceived = slot.writer_received_utc;

    const measure = this.deps.sha256(canonicalize({ identity, observation }));
    const existing = this.committed.get(i);
    if (existing) {
      this.dupFlags.set(i, orMask(this.dupFlags.get(i) ?? 0, F.DUPLICATE));
      if (existing.measure === measure) {
        this.appendIncident("duplicate_retry", i, F.DUPLICATE, measure, existing.sha);
        return { status: "duplicate_retry", i, flags: F.DUPLICATE, record_sha256: existing.sha, stopped: this.phaseValue !== "open" };
      }
      const flags = orMask(F.DUPLICATE, F.COLLECTOR_STOP);
      this.appendIncident("duplicate_conflict", i, flags, measure, existing.sha);
      this.markStop("duplicate_conflict", flags);
      return { status: "rejected", i, flags, record_sha256: existing.sha, stopped: true };
    }

    const prev = this.committed.get(i - 1);
    const { interval, flags: contextFlags } = computeCpuInterval(
      i, { identity, observation }, prev && { identity: prev.record.identity, observation: prev.record.observation },
    );
    let flags = orMask(intrinsicFlags(d, i, identity, observation), contextFlags);
    // Arrival order: slot 0 first, then strictly increasing. A skipped slot is a gap, not disorder.
    if (this.lastCommitted === null ? i !== 0 : i < this.lastCommitted) flags = orMask(flags, F.OUT_OF_ORDER);
    if (prev?.record.observation && observation && (parseUtcMs(observation.utc_start) ?? 0) < (parseUtcMs(prev.record.observation.utc_start) ?? 0)) {
      flags = orMask(flags, F.CLOCK_DISAGREEMENT);
    }
    const offset = FIRST_OFFSET_SECONDS + STEP_SECONDS * i;
    const t = parseUtcMs(d.T_utc);
    const record: RecordV1 = {
      schema: "b2proc.record.v1", run_alias: d.run_alias, fixture_alias: d.fixture_alias, writer_alias: d.writer_alias, i,
      scheduled_offset_seconds: offset, scheduled_utc: t === null ? null : formatUtc(t + offset * 1000),
      writer_received_utc: slot.writer_received_utc, write_seq: this.committed.size, identity, observation,
      cpu_interval: interval, flags_mask: flags,
    };
    const encoded = encodeRecord(record);
    if (!encoded.ok) return this.reject(i, encoded.flags);
    const sha = this.deps.sha256(encoded.value);

    const result = this.persist(keyFor(d.run_alias, "record", i), encoded.value, true);
    switch (result.status) {
      case "committed": {
        this.committed.set(i, { record, bytes: encoded.value, sha, measure });
        this.outcomes.set(i, { status: "committed", flags, sha });
        this.lastCommitted = i;
        if (result.reconciled) {
          this.dupFlags.set(i, F.DUPLICATE);
          this.appendIncident("duplicate_retry", i, F.DUPLICATE, measure, sha);
        }
        if (hasFlag(flags, F.OUT_OF_ORDER)) this.appendIncident("out_of_order", i, F.OUT_OF_ORDER, measure, sha);
        this.stopOnTriggers(i, flags, measure, sha);
        return { status: "committed", i, flags, record_sha256: sha, stopped: this.phaseValue !== "open" };
      }
      case "denied": {
        const f = orMask(F.FAILED_WRITE, F.AUTHORITY_MISSING);
        this.outcomes.set(i, { status: "failed", flags: f, sha: null });
        this.appendIncident("authority", i, f, measure, null);
        this.markStop("authority", orMask(f, F.COLLECTOR_STOP));
        return { status: "denied", i, flags: f, record_sha256: null, stopped: true };
      }
      case "exists":
      case "conflict": {
        const f = orMask(F.CONCURRENT_WRITER, result.status === "conflict" ? F.DUPLICATE : 0);
        this.outcomes.set(i, { status: "failed", flags: f, sha: null });
        this.appendIncident(result.status === "conflict" ? "duplicate_conflict" : "concurrency", i, f, measure, null);
        this.markStop("concurrency", orMask(f, F.COLLECTOR_STOP));
        return { status: "rejected", i, flags: f, record_sha256: null, stopped: true };
      }
      case "failed": {
        this.outcomes.set(i, { status: "failed", flags: result.flags, sha: null });
        this.appendIncident("write_failure", i, result.flags, measure, null);
        this.markStop("write_failure", orMask(result.flags, F.COLLECTOR_STOP));
        return { status: "failed", i, flags: result.flags, record_sha256: null, stopped: true };
      }
      default: {
        this.unknownWrite = true;
        this.outcomes.set(i, { status: "unknown", flags: F.WRITE_OUTCOME_UNKNOWN, sha: null });
        this.appendIncident("write_unknown", i, F.WRITE_OUTCOME_UNKNOWN, measure, null);
        this.markStop("write_unknown", orMask(F.WRITE_OUTCOME_UNKNOWN, F.COLLECTOR_STOP));
        return { status: "unknown", i, flags: F.WRITE_OUTCOME_UNKNOWN, record_sha256: null, stopped: true };
      }
    }
  }

  /**
   * Seal once; a second call returns the first result without touching the
   * store. After a non-denial stop, writes the reserved terminal incident, the
   * writer seal and the writer manifest; after a denial it writes nothing.
   */
  finish(p: { created_utc: string; stopped_utc: string | null }): FinishResult | null {
    const d = this.descriptor;
    if (d === null) return null;
    if (this.result !== null) return this.result;
    const descriptorSha = this.deps.sha256(this.descriptorText);
    if (this.phaseValue === "new" || this.phaseValue === "refused") {
      const manifest = this.writerManifest(d, descriptorSha, p.created_utc, null, OK);
      this.result = { outcome: "unknown", seal: null, manifest, seal_persisted: false, manifest_persisted: false, terminal_incident_persisted: false };
      return this.result;
    }
    if (this.phaseValue === "open" && this.committed.size < SLOT_COUNT) this.markStop("stop", F.COLLECTOR_STOP);
    if (this.phaseValue === "open") this.phaseValue = "finished";

    const finished = this.committed.size === SLOT_COUNT && this.stopKind === null && this.phaseValue !== "denied";
    let terminal = false;
    if (!finished && this.phaseValue !== "denied") {
      const flags = orMask(this.stopFlags, F.COLLECTOR_STOP, this.eventBudget ? F.EVENT_BUDGET : 0);
      terminal = this.writeIncident({
        schema: "b2proc.incident.v1", run_alias: d.run_alias, ordinal: TERMINAL_INCIDENT_ORDINAL, i: this.highestCommitted(),
        observed_utc: p.stopped_utc, principal_alias: d.writer_alias, kind: this.eventBudget ? "budget" : "stop",
        flags_mask: flags, candidate_sha256: null, persisted_sha256: null,
      });
    }
    const outcome: SealV1["outcome"] = this.phaseValue === "denied" || this.unknownWrite ? "unknown" : finished ? "finished" : "stopped";

    const highest = this.highestCommitted();
    const last = highest === null ? undefined : this.committed.get(highest);
    const seal: SealV1 = {
      schema: "b2proc.writer-seal.v1", run_alias: d.run_alias, principal_alias: d.writer_alias,
      last_i: last ? last.record.i : null, outcome, stopped_utc: finished ? null : p.stopped_utc,
      flags_mask: finished ? OK : orMask(this.stopFlags, F.COLLECTOR_STOP, this.eventBudget ? F.EVENT_BUDGET : 0),
      last_committed_record_sha256: last ? last.sha : null, receipt_sha256: null,
    };
    const sealText = encodeSeal(seal);
    let sealSha: string | null = null;
    if (sealText.ok && this.persist(keyFor(d.run_alias, "writer-seal"), sealText.value, true).status === "committed") {
      sealSha = this.deps.sha256(sealText.value);
    }
    // A denial on the seal write itself means nothing about this run is sealed.
    const finalOutcome: SealV1["outcome"] = this.phaseValue === "denied" ? "unknown" : outcome;
    const reported: SealV1 = finalOutcome === seal.outcome ? seal : { ...seal, outcome: finalOutcome };
    const manifest = this.writerManifest(d, descriptorSha, p.created_utc, sealSha, reported.flags_mask);
    const manifestText = encodeManifest(manifest);
    const manifestPersisted = manifestText.ok
      && this.persist(keyFor(d.run_alias, "manifest:writer"), manifestText.value, true, false).status === "committed";
    this.result = {
      outcome: finalOutcome, seal: reported, manifest, seal_persisted: sealSha !== null, manifest_persisted: manifestPersisted,
      terminal_incident_persisted: terminal,
    };
    return this.result;
  }

  // -- internals ------------------------------------------------------------

  private persist(key: string, bytes: string, reconcile: boolean, account = true): PersistResult {
    if (this.phaseValue === "denied") return { status: "denied" };
    const result = persistBytes(this.deps.store, key, bytes, reconcile && this.deps.reconcile_read_authorized);
    if (result.status === "denied") {
      this.phaseValue = "denied";
      this.stopFlags = orMask(this.stopFlags, F.COLLECTOR_STOP, F.AUTHORITY_MISSING);
    } else if (result.status === "committed" && account) {
      this.keyBytes += utf8Length(key);
      this.valueBytes += utf8Length(bytes);
    }
    return result;
  }

  private highestCommitted(): number | null {
    let highest: number | null = null;
    for (const i of this.committed.keys()) if (highest === null || i > highest) highest = i;
    return highest;
  }

  private markStop(kind: IncidentKind, flags: number): void {
    if (this.phaseValue === "open") this.phaseValue = "stopped";
    if (this.stopKind === null) this.stopKind = kind;
    this.stopFlags = orMask(this.stopFlags, flags, F.COLLECTOR_STOP);
  }

  private reject(i: number | null, flags: number): SlotResult {
    const kind: IncidentKind = hasFlag(flags, F.PRIVACY_REJECTED) ? "privacy"
      : hasFlag(flags, orMask(F.ROW_BUDGET, F.BYTE_BUDGET)) ? "budget" : "schema";
    this.appendIncident(kind, null, flags, null, null);
    this.markStop(kind, orMask(flags, F.COLLECTOR_STOP));
    return { status: "rejected", i, flags, record_sha256: null, stopped: true };
  }

  private stopOnTriggers(i: number, flags: number, measure: string, sha: string): void {
    if (hasFlag(flags, orMask(F.IDENTITY_CHANGE, F.CLOCK_RESET))) {
      this.appendIncident("identity_change", i, flags, measure, sha);
      this.markStop("identity_change", orMask(flags & orMask(F.IDENTITY_CHANGE, F.CLOCK_RESET), F.COLLECTOR_STOP));
    } else if (hasFlag(flags, F.SOURCE_UNAVAILABLE)) {
      this.appendIncident("source_failure", i, flags, measure, sha);
      this.markStop("source_failure", orMask(F.SOURCE_UNAVAILABLE, F.COLLECTOR_STOP));
    } else if (hasFlag(flags, F.DEADLINE)) {
      this.appendIncident("deadline", i, flags, measure, sha);
      this.markStop("deadline", orMask(F.DEADLINE, F.COLLECTOR_STOP));
    }
  }

  private appendIncident(kind: IncidentKind, i: number | null, flags: number, candidate: string | null, persistedSha: string | null): void {
    const d = this.descriptor;
    if (d === null) return;
    if (this.ordinary >= ORDINARY_INCIDENT_LIMIT) {
      this.eventBudget = true;
      this.markStop("budget", orMask(F.EVENT_BUDGET, F.COLLECTOR_STOP));
      return;
    }
    const incident: IncidentV1 = {
      schema: "b2proc.incident.v1", run_alias: d.run_alias, ordinal: this.ordinary++, i, observed_utc: this.lastReceived,
      principal_alias: d.writer_alias, kind, flags_mask: flags, candidate_sha256: candidate, persisted_sha256: persistedSha,
    };
    this.writeIncident(incident);
  }

  private writeIncident(incident: IncidentV1): boolean {
    const encoded = encodeIncident(incident);
    if (encoded.ok && this.persist(keyFor(incident.run_alias, "event", incident.ordinal), encoded.value, false).status === "committed") {
      this.persisted.push(incident);
      return true;
    }
    this.local.push(incident);
    return false;
  }

  private writerManifest(d: RunDescriptorV1, descriptorSha: string, created: string, sealSha: string | null, sealFlags: number): ManifestV1 {
    const unarmed = this.phaseValue === "new" || this.phaseValue === "refused";
    const entries: ManifestEntryV1[] = Array.from({ length: SLOT_COUNT }, (_, i) => {
      const o = this.outcomes.get(i);
      if (o) return { i, record_sha256: o.sha, write_status: o.status, read_status: "not_attempted" as const, flags_mask: orMask(o.flags, this.dupFlags.get(i) ?? 0) };
      const flags = unarmed ? (this.openFlags !== OK ? this.openFlags : F.AUTHORITY_MISSING) : orMask(F.COLLECTOR_STOP, this.phaseValue === "denied" ? F.AUTHORITY_MISSING : 0);
      return { i, record_sha256: null, write_status: "not_attempted" as const, read_status: "not_attempted" as const, flags_mask: flags };
    });
    const records = [...this.committed.values()].map((c) => c.record);
    return assembleManifest({
      kind: "writer_claim", descriptor: d, descriptorSha, principal: d.writer_alias, created_utc: created,
      read_started_utc: null, read_finished_utc: null, writer_seal_sha256: sealSha, entries,
      qualified_rss: records.filter(qualifiedRss).length,
      qualified_adjacent_cpu: records.filter((r) => r.cpu_interval.one_core_micropercent !== null).length,
      incident_keys_present: this.persisted.length,
      keys_read: 0, key_bytes: this.keyBytes, value_bytes: this.valueBytes,
      run_flags: orMask(sealFlags, this.stopFlags, authorityFlags(d), this.openFlags, this.eventBudget ? F.EVENT_BUDGET : 0, sealSha === null ? F.OPEN_OR_UNSEALED : 0),
    });
  }
}

/** "unknown" is normalized to a null build id at the input boundary only. */
function normalizeSlot(raw: unknown): unknown {
  if (!isRecordLike(raw) || !isRecordLike(raw.identity) || raw.identity.bot_build_id !== "unknown") return raw;
  return { ...raw, identity: { ...raw.identity, bot_build_id: null } };
}

// ---------------------------------------------------------------------------
// Independent reader
// ---------------------------------------------------------------------------

export interface ReadbackParams {
  store: PersistedStore;
  sha256: Sha256Hex;
  run_alias: string;
  reader_alias: string;
  created_utc: string;
  read_started_utc: string;
  read_finished_utc: string;
}

export interface ReadbackResult {
  manifest: ManifestV1 | null;
  manifest_text: string | null;
  seal: SealV1 | null;
  seal_text: string | null;
  flags: number;
  stop: "none" | "denied" | "deadline";
  reads: number;
}

type Read = { kind: "value"; value: string } | { kind: "absent" } | { kind: "error" } | { kind: "skipped" };

/**
 * Rebuild the 120-entry manifest from persisted bytes alone: one read per
 * fixed key, no writer cache, no re-scrape. A denial or the 300 s budget
 * ends reading at once and leaves the rest not attempted.
 */
export function readback(p: ReadbackParams): ReadbackResult {
  let elapsed = 0;
  let reads = 0;
  let keyBytes = 0;
  let valueBytes = 0;
  let stop = "none" as ReadbackResult["stop"];
  const read = (key: string): Read => {
    if (stop === "none" && elapsed >= READ_BUDGET_MS) stop = "deadline";
    if (stop !== "none") return { kind: "skipped" };
    reads++;
    keyBytes += utf8Length(key);
    const out = p.store.read(key);
    if (out.kind === "denied") { stop = "denied"; return { kind: "error" }; }
    const late = timedOut(out.elapsed_ms);
    elapsed += late ? OPERATION_TIMEOUT_MS : out.elapsed_ms;
    if (late || out.kind === "error") return { kind: "error" };
    if (out.kind === "absent") return { kind: "absent" };
    valueBytes += utf8Length(out.value);
    return { kind: "value", value: out.value };
  };
  const bail = (flags: number): ReadbackResult => ({ manifest: null, manifest_text: null, seal: null, seal_text: null, flags, stop, reads });
  const alias = p.run_alias;

  const rawDescriptor = read(keyFor(alias, "descriptor"));
  if (rawDescriptor.kind !== "value") return bail(rawDescriptor.kind === "absent" ? F.MISSING : orMask(F.FAILED_READ, rawDescriptor.kind === "skipped" ? F.TRUNCATED : 0));
  const decodedDescriptor = decodeDescriptor(rawDescriptor.value);
  if (!decodedDescriptor.ok) return bail(orMask(F.READ_MISMATCH, decodedDescriptor.flags));
  const d = decodedDescriptor.value;
  if (d.run_alias !== alias) return bail(F.READ_MISMATCH);
  if (d.reader_alias !== p.reader_alias) return bail(F.AUTHORITY_MISSING);
  const descriptorSha = p.sha256(rawDescriptor.value);
  const base = { descriptor: d, descriptorSha, principal: p.reader_alias, created_utc: p.created_utc, read_started_utc: p.read_started_utc, read_finished_utc: p.read_finished_utc };

  const finish = (m: ManifestArgs, sealFlags: number, lastVerified: { i: number; sha: string } | null): ReadbackResult => {
    const manifest = assembleManifest(m);
    const encoded = encodeManifest(manifest);
    if (!encoded.ok) return bail(encoded.flags);
    const seal: SealV1 = {
      schema: "b2proc.reader-seal.v1", run_alias: alias, principal_alias: p.reader_alias,
      last_i: lastVerified ? lastVerified.i : null, outcome: stop === "none" ? "finished" : "stopped",
      stopped_utc: stop === "none" ? null : p.read_finished_utc, flags_mask: orMask(manifest.flags_mask, sealFlags),
      last_committed_record_sha256: lastVerified ? lastVerified.sha : null, receipt_sha256: null,
    };
    const sealText = encodeSeal(seal);
    return {
      manifest, manifest_text: encoded.value, seal: sealText.ok ? seal : null, seal_text: sealText.ok ? sealText.value : null,
      flags: manifest.flags_mask, stop, reads,
    };
  };

  const authority = authorityFlags(d);
  if (authority !== OK) {
    return finish({
      ...base, kind: "independent_readback", writer_seal_sha256: null, entries: untouchedEntries(F.AUTHORITY_MISSING), qualified_rss: 0,
      qualified_adjacent_cpu: 0, incident_keys_present: 0, keys_read: reads, key_bytes: keyBytes, value_bytes: valueBytes,
      run_flags: authority,
    }, OK, null);
  }

  let runFlags = OK;

  // Writer seal.
  let seal: SealV1 | null = null;
  let sealSha: string | null = null;
  const rawSeal = read(keyFor(alias, "writer-seal"));
  if (rawSeal.kind === "value") {
    const decoded = decodeSeal(rawSeal.value);
    if (decoded.ok && decoded.value.schema === "b2proc.writer-seal.v1" && decoded.value.run_alias === alias && decoded.value.principal_alias === d.writer_alias) {
      seal = decoded.value;
      sealSha = p.sha256(rawSeal.value);
      runFlags = orMask(runFlags, seal.flags_mask);
    } else {
      runFlags = orMask(runFlags, F.READ_MISMATCH, F.OPEN_OR_UNSEALED);
    }
  } else {
    runFlags = orMask(runFlags, F.OPEN_OR_UNSEALED, rawSeal.kind === "error" ? F.FAILED_READ : 0);
  }

  // Writer manifest (claims, never trusted over the records).
  let claim: ManifestV1 | null = null;
  const rawClaim = read(keyFor(alias, "manifest:writer"));
  if (rawClaim.kind === "value") {
    const decoded = decodeManifest(rawClaim.value);
    const ok = decoded.ok && decoded.value.kind === "writer_claim" && decoded.value.run_alias === alias
      && decoded.value.fixture_alias === d.fixture_alias && decoded.value.principal_alias === d.writer_alias
      && decoded.value.descriptor_sha256 === descriptorSha && decoded.value.contract_sha256 === d.contract_sha256
      && decoded.value.writer_seal_sha256 === sealSha;
    if (ok) claim = decoded.ok ? decoded.value : null;
    else runFlags = orMask(runFlags, F.READ_MISMATCH, F.OPEN_OR_UNSEALED);
  } else {
    runFlags = orMask(runFlags, F.OPEN_OR_UNSEALED, rawClaim.kind === "error" ? F.FAILED_READ : 0);
  }

  // Records.
  const status: ReadStatus[] = [];
  const entryFlags: number[] = [];
  const shas: (string | null)[] = [];
  const recs: (RecordV1 | null)[] = [];
  const t = parseUtcMs(d.T_utc)!;
  for (let i = 0; i < SLOT_COUNT; i++) {
    const raw = read(keyFor(alias, "record", i));
    status[i] = "not_attempted";
    entryFlags[i] = OK;
    shas[i] = null;
    recs[i] = null;
    if (raw.kind === "skipped") { entryFlags[i] = orMask(F.TRUNCATED, stop === "deadline" ? F.DEADLINE : 0); continue; }
    if (raw.kind === "absent") { status[i] = "missing"; entryFlags[i] = F.MISSING; continue; }
    if (raw.kind === "error") { status[i] = "failed"; entryFlags[i] = F.FAILED_READ; continue; }
    const decoded = decodeRecord(raw.value);
    const offset = FIRST_OFFSET_SECONDS + STEP_SECONDS * i;
    const sound = decoded.ok && decoded.value.run_alias === d.run_alias && decoded.value.fixture_alias === d.fixture_alias
      && decoded.value.writer_alias === d.writer_alias && decoded.value.i === i && decoded.value.scheduled_utc === formatUtc(t + offset * 1000);
    if (!decoded.ok || !sound) { status[i] = "mismatch"; entryFlags[i] = orMask(F.READ_MISMATCH, decoded.ok ? 0 : decoded.flags); continue; }
    const rec = decoded.value;
    const intrinsic = intrinsicFlags(d, i, rec.identity, rec.observation);
    const stray = (rec.flags_mask & ~orMask(intrinsic, CONTEXT_FLAGS)) >>> 0;
    if (orMask(rec.flags_mask, intrinsic) !== rec.flags_mask || stray !== 0) { status[i] = "mismatch"; entryFlags[i] = F.READ_MISMATCH; continue; }
    status[i] = "verified";
    entryFlags[i] = rec.flags_mask;
    shas[i] = p.sha256(raw.value);
    recs[i] = rec;
  }

  // Compare against the writer's claims and each other.
  const writeStatus: WriteStatus[] = Array.from({ length: SLOT_COUNT }, (_, i) => claim?.entries[i]?.write_status ?? "unknown");
  const seenSeq = new Set<number>();
  for (let i = 0; i < SLOT_COUNT; i++) {
    const rec = recs[i];
    const claimed = claim?.entries[i];
    if (status[i] === "verified" && rec) {
      const contradicts = claimed !== undefined
        && ((claimed.record_sha256 !== null && claimed.record_sha256 !== shas[i]) || claimed.write_status === "failed" || claimed.write_status === "not_attempted");
      const dupSeq = rec.write_seq !== null && seenSeq.has(rec.write_seq);
      if (rec.write_seq !== null) seenSeq.add(rec.write_seq);
      if (contradicts || dupSeq) { status[i] = "mismatch"; entryFlags[i] = F.READ_MISMATCH; recs[i] = null; shas[i] = null; }
    } else if (status[i] === "missing") {
      const stopped = claimed?.write_status === "not_attempted" || (claim === null && seal?.outcome === "stopped" && i > (seal.last_i ?? -1));
      if (stopped) entryFlags[i] = orMask(F.MISSING, F.COLLECTOR_STOP);
    }
    if (claimed !== undefined) entryFlags[i] = orMask(entryFlags[i]!, claimed.flags_mask & WRITER_FACT_FLAGS);
  }
  for (let i = 1; i < SLOT_COUNT; i++) {
    const rec = recs[i];
    if (status[i] !== "verified" || !rec) continue;
    const prior = recs[i - 1] ?? null;
    const persistedInterval = rec.cpu_interval.previous_i !== null;
    const recomputed = computeCpuInterval(i, rec, prior ?? undefined);
    if (persistedInterval && !prior) { entryFlags[i] = orMask(entryFlags[i]!, F.NONADJACENT_CPU); continue; }
    // Slot i arrived before slot i-1: the writer held no predecessor, so it could neither pair them nor see
    // what the pair would show. The persisted arrival order (write_seq) is the evidence for that.
    const arrivedFirst = !persistedInterval && prior !== null && hasFlag(rec.flags_mask, F.NONADJACENT_CPU)
      && rec.write_seq !== null && prior.write_seq !== null && prior.write_seq > rec.write_seq;
    if (arrivedFirst) continue;
    const same = canonicalize(recomputed.interval) === canonicalize(rec.cpu_interval);
    const explainedNull = !persistedInterval && recomputed.interval.previous_i === null;
    // The pair itself says what went wrong (reset, wrap, clock, identity); a record may not omit it.
    const required = (recomputed.flags & ~orMask(F.NONADJACENT_CPU, F.CPU_BASELINE_ONLY)) >>> 0;
    const omitted = orMask(rec.flags_mask, required) !== rec.flags_mask;
    if ((!same && !explainedNull) || omitted) {
      status[i] = "mismatch";
      entryFlags[i] = orMask(F.READ_MISMATCH, entryFlags[i]! & WRITER_FACT_FLAGS);
      recs[i] = null;
      shas[i] = null;
    }
  }

  // Incidents.
  let incidents = 0;
  let incidentsComplete = true;
  for (let n = 0; n < INCIDENT_KEY_COUNT; n++) {
    const raw = read(keyFor(alias, "event", n));
    if (raw.kind === "skipped") { incidentsComplete = false; runFlags = orMask(runFlags, F.TRUNCATED, stop === "deadline" ? F.DEADLINE : 0); continue; }
    if (raw.kind === "error") { incidentsComplete = false; runFlags = orMask(runFlags, F.FAILED_READ); continue; }
    if (raw.kind === "absent") continue;
    const decoded = decodeIncident(raw.value);
    if (decoded.ok && decoded.value.run_alias === alias && decoded.value.ordinal === n && decoded.value.principal_alias === d.writer_alias) {
      incidents++;
      runFlags = orMask(runFlags, decoded.value.flags_mask);
    } else {
      runFlags = orMask(runFlags, F.READ_MISMATCH);
    }
  }
  if (stop !== "none") runFlags = orMask(runFlags, F.TRUNCATED, stop === "deadline" ? F.DEADLINE : 0);
  if (claim !== null) {
    // An incident the writer counted but the store no longer holds must not read as a clean run.
    if (incidentsComplete && claim.counts.incident_keys_present !== incidents) runFlags = orMask(runFlags, F.READ_MISMATCH);
  }

  // The seal must name the record it says it committed last.
  if (seal !== null && seal.last_i !== null && status[seal.last_i] === "verified" && shas[seal.last_i] !== seal.last_committed_record_sha256) {
    runFlags = orMask(runFlags, F.READ_MISMATCH);
  }

  const entries: ManifestEntryV1[] = Array.from({ length: SLOT_COUNT }, (_, i) => ({
    i, record_sha256: shas[i]!, write_status: writeStatus[i]!, read_status: status[i]!, flags_mask: entryFlags[i]!,
  }));
  const verified = recs.flatMap((r, i) => (r && status[i] === "verified" ? [{ r, i }] : []));
  const last = verified.length === 0 ? null : verified[verified.length - 1]!;
  return finish({
    ...base, kind: "independent_readback", writer_seal_sha256: sealSha, entries,
    qualified_rss: verified.filter(({ r }) => qualifiedRss(r)).length,
    qualified_adjacent_cpu: verified.filter(({ r, i }) => r.cpu_interval.one_core_micropercent !== null && !hasFlag(entryFlags[i]!, F.NONADJACENT_CPU)).length,
    incident_keys_present: incidents, keys_read: reads, key_bytes: keyBytes, value_bytes: valueBytes, run_flags: runFlags,
  }, OK, last ? { i: last.i, sha: shas[last.i]! } : null);
}

/**
 * Persist the reader's own manifest and seal once. A second reader finds the
 * keys taken and is refused: no overwrite, no extra run budget.
 */
export function persistReaderOutputs(store: PersistedStore, result: ReadbackResult, alias: string):
  { manifest: PersistResult["status"] | "skipped"; seal: PersistResult["status"] | "skipped" } {
  if (result.manifest_text === null || result.seal_text === null) return { manifest: "skipped", seal: "skipped" };
  const manifest = persistBytes(store, keyFor(alias, "manifest:reader"), result.manifest_text, false);
  if (manifest.status === "denied") return { manifest: "denied", seal: "skipped" };
  const seal = persistBytes(store, keyFor(alias, "reader-seal"), result.seal_text, false);
  return { manifest: manifest.status, seal: seal.status };
}
