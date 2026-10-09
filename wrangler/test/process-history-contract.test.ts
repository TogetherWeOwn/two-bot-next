import { afterEach, test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import * as C from "../src/process-history-contract.ts";

/**
 * Offline proof catalogue for the process-history V1 contract. Everything here
 * is synthetic: a fixed 120-slot series from the fixture file drives the model,
 * and each named failure family is generated across its whole indexed domain
 * (no first/middle/last sampling). The extra reader-binding probes at slots
 * 0, 59 and 119 are additional to those families, not a substitute. No clock, network, OS, environment or storage
 * is touched; the only I/O is reading checked-in files.
 */

const F = C.F;
const sha = (s: string): string => createHash("sha256").update(s).digest("hex");
const FIXTURE_URL = new URL("./fixtures/process-history-v1.json", import.meta.url);
const FIXTURE_TEXT = readFileSync(FIXTURE_URL, "utf8");
const FIX = JSON.parse(FIXTURE_TEXT) as {
  descriptor: C.RunDescriptorV1;
  series: { i: number; anchor_ns: string; rss_kb: string; utime: string; stime: string }[];
  record_sha256: string[];
  cpu_math: { name: string; hz: number; delta_ticks: string; delta_ns: string; allocated_vcpu_milli: number | null; one_core: string; allocated: string | null }[];
  proc_status: { name: string; text: string; bytes?: string; flags?: string[] }[];
  proc_stat: { name: string; text: string; fields?: Record<string, string>; flags?: string[] }[];
};
const DESC = FIX.descriptor;
const T_MS = C.parseUtcMs(DESC.T_utc)!;
const CREATED = C.formatUtc(T_MS + 14_400_000 + 10_000)!;
const STOPPED = C.formatUtc(T_MS + 14_400_000 + 5_000)!;
const READ_START = C.formatUtc(T_MS + 14_400_000 + 60_000)!;
const READ_END = C.formatUtc(T_MS + 14_400_000 + 90_000)!;
const range = (n: number): number[] => Array.from({ length: n }, (_, i) => i);
const clone = <T>(x: T): T => JSON.parse(JSON.stringify(x)) as T;
const mask = (...names: C.FlagName[]): number => C.orMask(...names.map((n) => F[n]));
const has = (m: number, ...names: C.FlagName[]): boolean => names.every((n) => C.hasFlag(m, F[n]));
const U64_MAX = (1n << 64n) - 1n;
// Assembled from pieces so this file does not match its own public-hygiene scan.
const INTERNAL_ID = new RegExp(["TO", "G-[0-9]|PA", "P-[0-9]"].join(""));
const ANY_URL = new RegExp(["https?", ":\\/\\/"].join(""));

/** Independent canonical form: sorted keys, compact, written separately from the module. */
function refCanon(v: unknown): string {
  if (Array.isArray(v)) return `[${v.map(refCanon).join(",")}]`;
  if (v !== null && typeof v === "object") {
    const o = v as Record<string, unknown>;
    return `{${Object.keys(o).sort().map((k) => `${JSON.stringify(k)}:${refCanon(o[k])}`).join(",")}}`;
  }
  return JSON.stringify(v);
}

/** Every flag any scenario produced; the last test requires all 32 bits to have been seen. */
const seen = { mask: 0, executed: 0 };
afterEach(() => { seen.executed++; });
const note = (...masks: number[]): void => { seen.mask = C.orMask(seen.mask, ...masks); };

// ---------------------------------------------------------------------------
// Synthetic inputs built from supplied proc text, exactly as a sampler would feed the model
// ---------------------------------------------------------------------------

function baseSlot(i: number, identity: C.IdentityV1 = DESC.initial_identity): C.SlotInput {
  const row = FIX.series[i]!;
  const due = T_MS + (7200 + 60 * i) * 1000;
  const rss = C.parseVmRss(`Name:\tsynthetic\nVmRSS:\t${row.rss_kb} kB\n`);
  const stat = C.parseProcStat(`4242 (synthetic) S 1 1 1 0 -1 4194560 100 0 0 0 ${row.utime} ${row.stime} 0 0 20 0 1 0 ${DESC.initial_identity.process_start_ticks} 1000 100\n`);
  assert.ok(rss.ok && stat.ok, "fixture proc text parses");
  assert.equal(stat.value.process_start_ticks, DESC.initial_identity.process_start_ticks);
  const anchor = BigInt(row.anchor_ns);
  return {
    i,
    writer_received_utc: C.formatUtc(due + 40)!,
    identity: clone(identity),
    observation: {
      utc_start: C.formatUtc(due)!,
      utc_end: C.formatUtc(due + 25)!,
      mono_start_ns: (anchor - 1_000_000n).toString(),
      mono_end_ns: (anchor + 9_000_000n).toString(),
      cpu_anchor_mono_ns: row.anchor_ns,
      rss_bytes: rss.value,
      cpu_user_ticks: stat.value.cpu_user_ticks,
      cpu_system_ticks: stat.value.cpu_system_ticks,
      source: "linux_proc_self_v1",
    },
  };
}

const obsOf = (s: C.SlotInput): C.ObservationV1 => s.observation!;

// ---------------------------------------------------------------------------
// Scripted store: fault injection and an exact call log (zero-I/O proofs read the log)
// ---------------------------------------------------------------------------

const TIMEOUT: C.CreateOutcome = { kind: "timeout", elapsed_ms: 2000 };
const REJECTED: C.CreateOutcome = { kind: "rejected", elapsed_ms: 1 };
const DENIED = { kind: "denied" } as const;
type Fault = { outcome: C.CreateOutcome | C.ReadOutcome; land?: boolean };
type Script = (op: "create" | "read", key: string, nth: number, value?: string) => Fault | undefined;

class SpyStore implements C.PersistedStore {
  readonly log: { op: "create" | "read"; key: string }[] = [];
  readonly inner: C.PersistedStore;
  private readonly script: Script;
  constructor(inner: C.PersistedStore, script: Script = () => undefined) {
    this.inner = inner;
    this.script = script;
  }
  create(key: string, value: string): C.CreateOutcome {
    this.log.push({ op: "create", key });
    const fault = this.script("create", key, this.count("create", key), value);
    if (!fault) return this.inner.create(key, value);
    if (fault.land) this.inner.create(key, value);
    return fault.outcome as C.CreateOutcome;
  }
  read(key: string): C.ReadOutcome {
    this.log.push({ op: "read", key });
    const fault = this.script("read", key, this.count("read", key));
    return fault ? (fault.outcome as C.ReadOutcome) : this.inner.read(key);
  }
  count(op?: "create" | "read", key?: string): number {
    return this.log.filter((e) => (op === undefined || e.op === op) && (key === undefined || e.key === key)).length;
  }
}

const rkey = (i: number): string => C.keyFor(DESC.run_alias, "record", i);
const ekey = (n: number): string => C.keyFor(DESC.run_alias, "event", n);

function newRun(opts: { desc?: unknown; reconcile?: boolean; script?: Script } = {}) {
  const map = new C.PersistedMap();
  const store = new SpyStore(map, opts.script);
  const writer = new C.ProcessHistoryWriter({ store, sha256: sha, reconcile_read_authorized: opts.reconcile ?? false });
  const opened = writer.open(opts.desc ?? DESC);
  return { map, store, writer, opened };
}

function feed(writer: C.ProcessHistoryWriter, indices: Iterable<number>, mutate?: (slot: C.SlotInput, i: number) => unknown): C.SlotResult[] {
  const out: C.SlotResult[] = [];
  for (const i of indices) {
    const slot = baseSlot(i);
    out.push(writer.record(mutate ? mutate(slot, i) : slot));
  }
  return out;
}

/** Seals the run; unless told otherwise, the seal and the writer manifest must actually have been persisted. */
function finishRun(writer: C.ProcessHistoryWriter, persisted = true): C.FinishResult {
  const fin = writer.finish({ created_utc: CREATED, stopped_utc: STOPPED })!;
  if (persisted) assert.ok(fin.seal_persisted && fin.manifest_persisted, "writer seal and manifest are persisted");
  return fin;
}

function readFrom(snapshot: string, opts: { desc?: C.RunDescriptorV1; script?: Script } = {}) {
  const fresh = C.PersistedMap.fromSnapshot(snapshot);
  assert.ok(fresh, "snapshot loads");
  const store = new SpyStore(fresh, opts.script);
  const d = opts.desc ?? DESC;
  const result = C.readback({
    store, sha256: sha, run_alias: d.run_alias, reader_alias: d.reader_alias,
    created_utc: CREATED, read_started_utc: READ_START, read_finished_utc: READ_END,
  });
  return { result, store, manifest: result.manifest! };
}

const readMap = (map: C.PersistedMap, opts: { desc?: C.RunDescriptorV1; script?: Script } = {}) => readFrom(map.snapshot(), opts);

function recordAt(map: C.PersistedMap, i: number): C.RecordV1 {
  const raw = map.read(rkey(i));
  assert.equal(raw.kind, "value");
  const decoded = C.decodeRecord((raw as { value: string }).value);
  assert.ok(decoded.ok, `record ${i} decodes`);
  return decoded.value;
}

function recordText(map: C.PersistedMap, i: number): string {
  const raw = map.read(rkey(i));
  assert.equal(raw.kind, "value");
  return (raw as { value: string }).value;
}

function incidentsOf(map: C.PersistedMap): C.IncidentV1[] {
  const out: C.IncidentV1[] = [];
  for (let n = 0; n < 32; n++) {
    const raw = map.read(ekey(n));
    if (raw.kind !== "value") continue;
    const decoded = C.decodeIncident(raw.value);
    assert.ok(decoded.ok, `incident ${n} decodes`);
    out.push(decoded.value);
  }
  return out;
}

function pairsOf(map: C.PersistedMap): [string, string][] {
  return JSON.parse(map.snapshot()) as [string, string][];
}

function assertManifestShape(m: C.ManifestV1): void {
  assert.equal(m.entries.length, 120, "all 120 indices are present");
  m.entries.forEach((e, idx) => assert.equal(e.i, idx, "entries are sorted by index"));
  const checked = C.validateManifest(m);
  assert.ok(checked.ok, "manifest validates");
  const text = C.encodeManifest(m);
  assert.ok(text.ok && utf8(text.value) <= 32768, "manifest within cap");
  assert.deepEqual(C.decodeManifest(text.ok ? text.value : ""), { ok: true, value: m });
  assert.equal(m.acceptance, "UNDECIDED");
  assert.equal(m.cpu_interval_boundary, "baseline_0_adjacent_1_to_119");
  assert.equal(m.budget.physical_storage_bytes, null);
  assert.ok(m.budget.accounted_bytes <= C.MAX_ACCOUNTED_BYTES);
  assert.equal(m.flags_mask, C.orMask(...m.entries.map((e) => e.flags_mask), m.flags_mask), "manifest mask covers every entry");
  note(m.flags_mask);
}

const utf8 = (s: string): number => new TextEncoder().encode(s).length;

/** Independent CPU expectation for slot i from the raw series (separate BigInt formula). */
function expectedRates(i: number, hz = 100n, milli: bigint | null = 1000n): { ticks: bigint; ns: bigint; one: bigint; alloc: bigint | null } {
  const a = FIX.series[i - 1]!;
  const b = FIX.series[i]!;
  const ticks = BigInt(b.utime) - BigInt(a.utime) + (BigInt(b.stime) - BigInt(a.stime));
  const ns = BigInt(b.anchor_ns) - BigInt(a.anchor_ns);
  const base = 100_000_000n * ticks * 1_000_000_000n;
  return { ticks, ns, one: base / (hz * ns), alloc: milli === null ? null : (base * 1000n) / (hz * ns * milli) };
}

// ---------------------------------------------------------------------------
// Fixture, bounds and primitives
// ---------------------------------------------------------------------------

test("fixture is synthetic, within its byte cap and public-safe", () => {
  assert.ok(C.withinCap("fixture", Buffer.byteLength(FIXTURE_TEXT)), "fixture <= 262144 bytes");
  assert.equal(FIX.series.length, 120);
  assert.equal(FIX.record_sha256.length, 120);
  assert.ok(C.withinCap("fixture", 262144) && !C.withinCap("fixture", 262145));
  assert.ok(C.validateDescriptor(DESC).ok);
  assert.equal(DESC.mode, "offline");
  assert.ok(Object.values(DESC.authority).every((v) => v === null), "no live authority is claimed");
  assert.ok(!INTERNAL_ID.test(FIXTURE_TEXT) && !ANY_URL.test(FIXTURE_TEXT) && !/@[a-z]|token|secret|password/i.test(FIXTURE_TEXT), "no internal id, URL, address or credential text");
});

test("bounds: 157 keys, 602112 value bytes, 20096 key bytes, 622208 logical bytes below 1 MiB", () => {
  assert.equal(C.MAX_KEYS, 157);
  assert.equal(C.MAX_VALUE_BYTES, 602112);
  assert.equal(C.MAX_KEY_BYTES, 20096);
  assert.equal(C.MAX_ACCOUNTED_BYTES, 622208);
  assert.equal(C.LOGICAL_BUDGET_BYTES, 1048576);
  assert.equal(C.LOGICAL_BUDGET_BYTES - C.MAX_ACCOUNTED_BYTES, 426368);
  assert.deepEqual(
    [C.CAPS.descriptor, C.CAPS.record, C.CAPS.incident, C.CAPS.manifest, C.CAPS.key, C.CAPS.fixture, C.CAPS.scratch],
    [4096, 4096, 1024, 32768, 128, 262144, 131072],
  );
  assert.ok(C.scratchRequirementBytes() <= C.CAPS.scratch, "parser/serializer working set fits the scratch cap");
  assert.equal(C.scratchRequirementBytes(), 58880, "modelled working set: 8192 + 4096 + 4096 + 32768 + 152 x 64");
  assert.equal(C.FIRST_OFFSET_SECONDS + C.STEP_SECONDS * C.SLOT_COUNT, C.END_EXCLUSIVE_OFFSET_SECONDS, "120 one-minute slots end exactly at T+4h");
  assert.equal(C.WRITE_ATTEMPTS_PER_KEY, 2);
  assert.equal(C.READ_BUDGET_MS, 300_000);
  assert.ok(C.DRAIN_BUDGET_MS >= 3 * C.WRITE_ATTEMPTS_PER_KEY * C.OPERATION_TIMEOUT_MS, "terminal drain (3 keys x 2 attempts x 2 s) fits 30 s");
});

test("U64 strings are exact decimal and never pass through Number", () => {
  for (const ok of ["0", "7", "18446744073709551615", "9007199254740993"]) assert.ok(C.isU64(ok), ok);
  for (const bad of ["", "01", "-1", "1.0", "1e3", " 1", "1 ", "18446744073709551616", "99999999999999999999", 5, null, undefined, 5n, "0x10"]) {
    assert.ok(!C.isU64(bad), String(bad));
  }
});

test("UTC strings: exactly 24 ASCII bytes, valid Gregorian date, milliseconds, Z", () => {
  assert.equal(C.parseUtcMs("1970-01-01T00:00:00.000Z"), 0);
  assert.equal(C.parseUtcMs("2000-02-29T23:59:59.999Z"), 951_868_799_999);
  for (const bad of [
    "1900-02-29T00:00:00.000Z", "2001-02-29T00:00:00.000Z", "2000-13-01T00:00:00.000Z", "2000-00-10T00:00:00.000Z",
    "2000-04-31T00:00:00.000Z", "2000-01-01T24:00:00.000Z", "2000-01-01T00:60:00.000Z", "2000-01-01T00:00:60.000Z",
    "2000-01-01T00:00:00Z", "2000-01-01T00:00:00.00Z", "2000-01-01T00:00:00.0000Z", "2000-01-01T00:00:00.000+00:00",
    "2000-01-01t00:00:00.000z", "2000-01-01 00:00:00.000Z", " 2000-01-01T00:00:00.000Z", "2000-01-01T00:00:00.000Z ", 5, null,
  ]) {
    assert.equal(C.parseUtcMs(bad), null, String(bad));
  }
  const table: [number, string][] = [
    [0, "1970-01-01T00:00:00.000Z"], [1, "1970-01-01T00:00:00.001Z"], [86_399_999, "1970-01-01T23:59:59.999Z"],
    [951_782_400_000, "2000-02-29T00:00:00.000Z"], [951_868_800_000, "2000-03-01T00:00:00.000Z"], [T_MS, DESC.T_utc],
    [T_MS + 14_400_000, "2000-01-02T04:00:00.000Z"], [253_402_300_799_999, "9999-12-31T23:59:59.999Z"],
  ];
  for (const [ms, text] of table) {
    assert.equal(C.formatUtc(ms), text);
    assert.equal(text.length, 24);
    assert.equal(C.parseUtcMs(text), ms);
  }
  assert.equal(C.formatUtc(253_402_300_800_000), null, "year 10000 is out of range");
  assert.equal(C.formatUtc(-1_000_000_000_000_000), null);
  assert.equal(C.formatUtc(1.5), null);
});

test("schedule_unarmed_120: all 120 offsets exist with no scheduled time while T is unset", () => {
  const unarmed = C.scheduleVector(null);
  assert.equal(unarmed.length, 120);
  unarmed.forEach((slot, i) => {
    assert.equal(slot.i, i);
    assert.equal(slot.offset_seconds, 7200 + 60 * i);
    assert.equal(slot.scheduled_utc, null);
  });
  assert.equal(unarmed[0]!.offset_seconds, 7200);
  assert.equal(unarmed[119]!.offset_seconds, 14340);
  assert.equal(unarmed[119]!.offset_seconds + C.STEP_SECONDS, C.END_EXCLUSIVE_OFFSET_SECONDS, "the end offset is exclusive");

  const armed = C.scheduleVector(DESC.T_utc);
  armed.forEach((slot) => {
    const at = C.parseUtcMs(slot.scheduled_utc)!;
    assert.equal(at, T_MS + slot.offset_seconds * 1000);
    assert.ok(at >= T_MS + 7_200_000 && at < T_MS + 14_400_000, "every slot lies in [T+2h, T+4h)");
  });
  assert.equal(armed[0]!.scheduled_utc, C.formatUtc(T_MS + 7_200_000));
  assert.equal(armed[119]!.scheduled_utc, C.formatUtc(T_MS + 14_340_000));
});

// ---------------------------------------------------------------------------
// Canonical bytes and the strict parser
// ---------------------------------------------------------------------------

test("canonical JSON: sorted keys, compact, integers only; strict parser rejects every non-canonical form", () => {
  assert.equal(C.canonicalize({ b: 1, a: [true, null, "x"], c: { z: 0, y: "" } }), "{\"a\":[true,null,\"x\"],\"b\":1,\"c\":{\"y\":\"\",\"z\":0}}");
  for (const bad of [1.5, -1, NaN, Infinity, undefined, 5n, () => 1, 2 ** 53]) assert.throws(() => C.canonicalize(bad), String(bad));
  const reject = (text: string) => assert.deepEqual(C.parseStrictJson(text), { ok: false, flags: F.INVALID_SCHEMA }, text);
  for (const text of [
    "{\"a\":1,\"a\":2}", "{\"a\":1, \"b\":2}", " {\"a\":1}", "{\"a\":1} ", "{\"a\":1.0}", "{\"a\":-1}", "{\"a\":01}", "{\"a\":1e3}",
    "{\"a\":9007199254740992}", "{\"a\":\"\\u0041\"}", "{\"a\":\"\\n\"}", "{\"a\":\"é\"}", "{\"a\":'x'}", "{a:1}", "{\"a\":1,}", "[1,]", "[",
    "{\"a\":NaN}", "{\"a\":undefined}", "{\"a\":\"line\nbreak\"}", "{\"a\":[[[[[[[[1]]]]]]]]}", "", "nul", "{\"a\":1}{\"b\":2}",
  ]) reject(text);
  assert.deepEqual(C.parseStrictJson("{\"a\":[1,true,null,\"x\"],\"b\":{}}"), { ok: true, value: { a: [1, true, null, "x"], b: {} } });
  // The parser must not let a key reach Object.prototype.
  const polluted = C.parseStrictJson("{\"__proto__\":{\"x\":1}}");
  assert.ok(polluted.ok);
  const carried = polluted.value as Record<string, unknown>;
  assert.ok(Object.hasOwn(carried, "__proto__"), "the key is an ordinary own property");
  assert.equal(carried.x, undefined, "and it did not become the prototype");
  assert.equal(Object.getPrototypeOf(carried), Object.prototype);
  assert.equal(({} as Record<string, unknown>).x, undefined);
});

test("decoders accept only the canonical bytes of a valid object and check the byte cap first", () => {
  const encoded = C.encodeDescriptor(DESC);
  assert.ok(encoded.ok);
  assert.equal(encoded.value, refCanon(DESC), "independent canonicalizer agrees byte for byte");
  assert.deepEqual(C.decodeDescriptor(encoded.value), { ok: true, value: DESC });
  // Same content, different bytes: reordered keys, whitespace, escaped equivalents.
  assert.deepEqual(C.decodeDescriptor(JSON.stringify(DESC)), { ok: false, flags: F.INVALID_SCHEMA });
  assert.deepEqual(C.decodeDescriptor(JSON.stringify(DESC, null, 1)), { ok: false, flags: F.INVALID_SCHEMA });
  assert.deepEqual(C.decodeDescriptor(encoded.value.replace("offline", "\\u006ffline")), { ok: false, flags: F.INVALID_SCHEMA });
  assert.deepEqual(C.decodeDescriptor(7), { ok: false, flags: F.INVALID_SCHEMA });
  // Over the cap is a byte-budget refusal before any parsing; at the cap it is parsed (and fails as non-canonical padding).
  assert.deepEqual(C.decodeRecord(" ".repeat(4097)), { ok: false, flags: F.BYTE_BUDGET });
  assert.deepEqual(C.decodeRecord(" ".repeat(4096)), { ok: false, flags: F.INVALID_SCHEMA });
  assert.deepEqual(C.decodeIncident(" ".repeat(1025)), { ok: false, flags: F.BYTE_BUDGET });
  assert.deepEqual(C.decodeIncident(" ".repeat(1024)), { ok: false, flags: F.INVALID_SCHEMA });
  const oversized = C.decodeManifest(" ".repeat(32769));
  assert.deepEqual(oversized, { ok: false, flags: F.BYTE_BUDGET });
  note(oversized.ok ? 0 : oversized.flags);
  assert.deepEqual(C.decodeManifest(" ".repeat(32768)), { ok: false, flags: F.INVALID_SCHEMA });
  assert.deepEqual(C.decodeSeal(" ".repeat(4097)), { ok: false, flags: F.BYTE_BUDGET });
  assert.deepEqual(C.decodeDescriptor(" ".repeat(4097)), { ok: false, flags: F.BYTE_BUDGET });
});

test("cap_values: every byte cap accepts the exact limit and refuses one byte more", () => {
  const boundaries: [keyof typeof C.CAPS, number][] = [
    ["record", 4096], ["descriptor", 4096], ["seal", 4096], ["incident", 1024], ["manifest", 32768], ["key", 128],
    ["fixture", 262144], ["procStatus", 8192], ["procStat", 4096], ["scratch", 131072],
  ];
  for (const [name, cap] of boundaries) {
    assert.ok(C.withinCap(name, cap), `${name} accepts ${cap}`);
    assert.ok(!C.withinCap(name, cap + 1), `${name} refuses ${cap + 1}`);
    assert.ok(C.withinCap(name, 0) && !C.withinCap(name, -1) && !C.withinCap(name, 1.5));
  }
  // proc inputs: exactly 8192 / 4096 bytes parse, one more byte is a byte-budget refusal (no truncation).
  const status = "VmRSS:\t1 kB\n";
  assert.deepEqual(C.parseVmRss(status + "x".repeat(8192 - status.length)), { ok: true, value: "1024" });
  assert.deepEqual(C.parseVmRss(status + "x".repeat(8193 - status.length)), { ok: false, flags: F.BYTE_BUDGET });
  const head = "5 (";
  const tail = ") S 1 1 1 0 -1 4194560 100 0 0 0 111 222 0 0 20 0 1 0 333 1000 100";
  const comm = (n: number) => head + "c".repeat(n - head.length - tail.length) + tail;
  assert.ok(C.parseProcStat(comm(4096)).ok && utf8(comm(4096)) === 4096);
  assert.deepEqual(C.parseProcStat(comm(4097)), { ok: false, flags: F.BYTE_BUDGET });
  // utf8Length agrees with the platform encoder on every width, including ill-formed UTF-16.
  assert.equal(C.utf8Length("\ud800\u0800"), new TextEncoder().encode("\ud800\u0800").length);
  let seed = 12345;
  const next = (n: number): number => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed % n; };
  const units = [0x41, 0x7f, 0x80, 0x7ff, 0x800, 0xd7ff, 0xd800, 0xdbff, 0xdc00, 0xdfff, 0xe000, 0xffff];
  for (let round = 0; round < 2000; round++) {
    const text = String.fromCharCode(...Array.from({ length: next(12) }, () => units[next(units.length)]!));
    assert.equal(C.utf8Length(text), new TextEncoder().encode(text).length, JSON.stringify(text));
  }
  // Multibyte text counts bytes, not characters.
  assert.deepEqual(C.parseVmRss("VmRSS:\t1 kB\n" + "é".repeat(4090)), { ok: true, value: "1024" });
  assert.deepEqual(C.parseVmRss("VmRSS:\t1 kB\n" + "é".repeat(4091)), { ok: false, flags: F.BYTE_BUDGET });
});

test("accounted bytes: exactly 1048576 passes, 1048577 does not", () => {
  assert.ok(C.accountedWithinBudget(C.MAX_KEY_BYTES, C.MAX_VALUE_BYTES));
  assert.ok(C.accountedWithinBudget(0, 1048576));
  assert.ok(!C.accountedWithinBudget(1, 1048576));
  assert.ok(!C.accountedWithinBudget(0, 1048577));
  assert.ok(!C.accountedWithinBudget(-1, 5));
});

// ---------------------------------------------------------------------------
// Failure vocabulary
// ---------------------------------------------------------------------------

test("flags_exhaustive: 32 fixed unsigned bits, bit 31 and the all-bits mask never go negative", () => {
  assert.equal(C.FLAG_NAMES.length, 32);
  assert.equal(new Set(C.FLAG_NAMES).size, 32);
  C.FLAG_NAMES.forEach((name, bit) => {
    assert.equal(F[name], 2 ** bit, name);
    assert.deepEqual(C.maskNames(F[name]), [name]);
    assert.ok(C.hasFlag(F[name], F[name]));
    assert.ok(C.hasFlag(C.ALL_FLAGS, F[name]));
    assert.ok(!C.hasFlag(0, F[name]));
  });
  assert.equal(F.OPEN_OR_UNSEALED, 2147483648);
  assert.equal(C.ALL_FLAGS, 4294967295);
  assert.equal(C.orMask(F.OPEN_OR_UNSEALED, F.MISSING), 2147483649);
  assert.ok(C.orMask(F.OPEN_OR_UNSEALED) > 0, "no signed coercion");
  assert.equal(C.maskNames(C.ALL_FLAGS).length, 32);
  assert.deepEqual(C.maskNames(0), []);
  assert.deepEqual(C.maskNames(mask("MISSING", "OPEN_OR_UNSEALED")), ["MISSING", "OPEN_OR_UNSEALED"]);
  for (const ok of [0, 1, 2147483648, 4294967295]) assert.ok(C.isMask(ok), String(ok));
  for (const bad of [-1, 4294967296, 1.5, "1", NaN, null, undefined, 2 ** 53]) assert.ok(!C.isMask(bad), String(bad));
});

test("flags_exhaustive: every bit and the all-bits mask survive a manifest round trip as unsigned integers", () => {
  const base = C.buildUnarmedManifest(DESC, sha, { kind: "independent_readback", principal: DESC.reader_alias, created_utc: CREATED });
  const variants = [...C.FLAG_NAMES.map((n) => F[n]), C.ALL_FLAGS, mask("MISSING", "OPEN_OR_UNSEALED", "AUTHORITY_MISSING")];
  for (const m of variants) {
    const manifest = clone(base);
    manifest.entries[7]!.flags_mask = m;
    manifest.entries[119]!.flags_mask = m;
    manifest.flags_mask = C.orMask(base.flags_mask, m);
    const encoded = C.encodeManifest(manifest);
    assert.ok(encoded.ok, `mask ${m}`);
    const decoded = C.decodeManifest(encoded.value);
    assert.ok(decoded.ok);
    assert.equal(decoded.value.entries[7]!.flags_mask, m);
    assert.ok(decoded.value.entries[7]!.flags_mask >= 0);
  }
  // A manifest whose run mask omits an entry's bit is inconsistent.
  const lying = clone(base);
  lying.entries[3]!.flags_mask = F.FAILED_READ;
  assert.ok(!C.validateManifest(lying).ok);
});

// ---------------------------------------------------------------------------
// Parsers and exact arithmetic
// ---------------------------------------------------------------------------

test("rss_units_and_source: kB scaled with checked integers; anything else is a source or schema failure", () => {
  assert.equal(FIX.proc_status.length, 12, "an emptied fixture must not pass vacuously");
  for (const c of FIX.proc_status) {
    const out = C.parseVmRss(c.text);
    if (c.bytes !== undefined) assert.deepEqual(out, { ok: true, value: c.bytes }, c.name);
    else {
      assert.ok(!out.ok, c.name);
      assert.equal(out.flags, mask(...(c.flags as C.FlagName[])), c.name);
      note(out.flags);
    }
  }
  assert.equal(C.parseVmRss("VmRSS:\t123 kB\n").ok && (C.parseVmRss("VmRSS:\t123 kB\n") as { value: string }).value, "125952");
  // cgroup, container or allocator figures cannot masquerade as the process source.
  const obs = obsOf(baseSlot(3));
  for (const source of ["cgroup_v2", "container", "allocator", "linux_proc_self_v2", ""]) {
    assert.ok(!C.validateObservation({ ...obs, source }).ok, source);
  }
  assert.ok(!C.validateObservation({ ...obs, source: "unavailable" }).ok, "unavailable source must carry no resource quantity");
  assert.ok(C.validateObservation({ ...obs, source: "unavailable", rss_bytes: null, cpu_user_ticks: null, cpu_system_ticks: null, cpu_anchor_mono_ns: null }).ok);
});

test("proc_stat_parser: fields 14, 15 and 22 only; comm and pid are consumed, never returned", () => {
  assert.equal(FIX.proc_stat.length, 12);
  for (const c of FIX.proc_stat) {
    const out = C.parseProcStat(c.text);
    if (c.fields !== undefined) {
      assert.deepEqual(out, { ok: true, value: c.fields }, c.name);
      assert.deepEqual(Object.keys((out as { value: object }).value).sort(), ["cpu_system_ticks", "cpu_user_ticks", "process_start_ticks"]);
    } else {
      assert.ok(!out.ok, c.name);
      assert.equal(out.flags, mask(...(c.flags as C.FlagName[])), c.name);
    }
  }
  const ok = C.parseProcStat("1234 (secret-name) S 1 1 1 0 -1 4194560 100 0 0 0 5 6 0 0 20 0 1 0 7 1000 100");
  assert.ok(ok.ok);
  assert.ok(!JSON.stringify(ok).includes("secret-name") && !JSON.stringify(ok).includes("1234"), "no comm or pid is retained");
});

test("cpu_math_exact: checked wide integers, one floor of the full numerator, no clamp, no Number", () => {
  assert.equal(FIX.cpu_math.length, 7);
  for (const c of FIX.cpu_math) {
    const rates = C.cpuRates(BigInt(c.delta_ticks), BigInt(c.delta_ns), c.hz, c.allocated_vcpu_milli);
    assert.ok(rates, c.name);
    assert.equal(rates.one_core?.toString(), c.one_core, c.name);
    assert.equal(rates.overflow, false);
    assert.equal(rates.allocated === null ? null : rates.allocated.toString(), c.allocated, c.name);
  }
  // Two busy cores exceed 100% of one core and stay unclamped.
  assert.ok(BigInt(FIX.cpu_math[3]!.one_core) > 100_000_000n);
  assert.equal(C.cpuRates(123456789012n, 987654321n, 100, null)?.one_core, 124999998873087500n, "exact beyond 2^53");
  // A result that does not fit u64 is unknown, never saturated; bad inputs are unknown too.
  assert.deepEqual(C.cpuRates(1n << 63n, 1n, 100, null), { one_core: null, allocated: null, overflow: true });
  assert.equal(C.cpuRates(1n, 0n, 100, null), null);
  assert.equal(C.cpuRates(-1n, 1n, 100, null), null);
  assert.equal(C.cpuRates(1n, 1n, 0, null), null);
  assert.equal(C.cpuRates(1n, 1n, 100.5, null), null);
  assert.deepEqual(C.cpuRates(U64_MAX, U64_MAX, 1_000_000, 1_000_000), { one_core: 100_000_000_000n, allocated: 100_000_000n, overflow: false }, "maximum strings stay exact");

  // The pair computation rides the same arithmetic and keeps exact decimal strings.
  const a = baseSlot(7);
  const b = baseSlot(8);
  const { interval, flags } = C.computeCpuInterval(8, b, a);
  const exp = expectedRates(8);
  assert.equal(flags, 0);
  assert.deepEqual(interval, {
    previous_i: 7, start_anchor_mono_ns: FIX.series[7]!.anchor_ns, end_anchor_mono_ns: FIX.series[8]!.anchor_ns,
    delta_mono_ns: exp.ns.toString(), delta_cpu_ticks: exp.ticks.toString(),
    one_core_micropercent: exp.one.toString(), allocated_micropercent: exp.alloc!.toString(),
  });
  assert.notEqual(interval.delta_mono_ns, "60000000000", "uses the observed anchors, not a nominal 60 s");

  // Spec vector through the model: HZ 100, 3000 ticks over exactly 60 s -> 50% of one core; 500 milli -> 100%.
  const base = baseSlot(0);
  const p1 = clone(base);
  const p2 = clone(base);
  obsOf(p1).cpu_anchor_mono_ns = "5000000000"; obsOf(p1).cpu_user_ticks = "1000"; obsOf(p1).cpu_system_ticks = "0";
  obsOf(p2).cpu_anchor_mono_ns = "65000000000"; obsOf(p2).cpu_user_ticks = "3000"; obsOf(p2).cpu_system_ticks = "1000";
  p2.identity.allocated_vcpu_milli = 500;
  p1.identity.allocated_vcpu_milli = 500;
  const half = C.computeCpuInterval(1, p2, p1).interval;
  assert.equal(half.delta_cpu_ticks, "3000");
  assert.equal(half.delta_mono_ns, "60000000000");
  assert.equal(half.one_core_micropercent, "50000000");
  assert.equal(half.allocated_micropercent, "100000000");
  p1.identity.cpu_ticks_per_second = 250; p2.identity.cpu_ticks_per_second = 250;
  obsOf(p2).cpu_user_ticks = "7500"; // +7500 ticks at 250 Hz is the same CPU time
  assert.equal(C.computeCpuInterval(1, p2, p1).interval.one_core_micropercent, "50000000");

  // Overflow of the rate leaves the exact deltas and flags the record, with no saturated rate.
  p1.identity.cpu_ticks_per_second = 100; p2.identity.cpu_ticks_per_second = 100;
  obsOf(p1).cpu_user_ticks = "0"; obsOf(p2).cpu_user_ticks = (1n << 63n).toString();
  obsOf(p2).cpu_anchor_mono_ns = "5000000001"; obsOf(p2).cpu_system_ticks = "0";
  const overflow = C.computeCpuInterval(1, p2, p1);
  assert.equal(overflow.flags, F.INVALID_SCHEMA);
  assert.equal(overflow.interval.delta_cpu_ticks, (1n << 63n).toString() + "");
  assert.equal(overflow.interval.one_core_micropercent, null);
  assert.equal(overflow.interval.allocated_micropercent, null);
  note(overflow.flags);
});

// ---------------------------------------------------------------------------
// Full run: complete_structural_120, pinned hashes, independent bytes and reader
// ---------------------------------------------------------------------------

test("complete_structural_120: 120 canonical records, 119 adjacent deltas, structural only", () => {
  const { map, store, writer, opened } = newRun({ reconcile: true });
  assert.deepEqual(opened, { opened: true, flags: 0 });
  const results = feed(writer, range(120));
  results.forEach((r, i) => {
    assert.equal(r.status, "committed");
    assert.equal(r.i, i);
    assert.equal(r.stopped, false);
    assert.equal(r.flags, i === 0 ? F.CPU_BASELINE_ONLY : 0, `slot ${i}`);
  });
  assert.deepEqual(results.map((r) => r.record_sha256), FIX.record_sha256, "pinned record hashes");
  const fin = finishRun(writer);
  assert.equal(fin.outcome, "finished");
  assert.ok(fin.seal_persisted && fin.manifest_persisted && !fin.terminal_incident_persisted);
  assert.equal(store.count("read"), 0, "a normal run never reads: no pre-window or end-boundary read");
  assert.equal(map.size, 1 + 120 + 1 + 1, "descriptor, 120 records, writer seal, writer manifest");

  let intervals = 0;
  range(120).forEach((i) => {
    const text = recordText(map, i);
    assert.equal(text, refCanon(JSON.parse(text)), "independent canonical bytes");
    assert.equal(sha(text), FIX.record_sha256[i]);
    assert.ok(utf8(text) <= 4096);
    const rec = recordAt(map, i);
    assert.equal(rec.write_seq, i);
    assert.equal(rec.scheduled_offset_seconds, 7200 + 60 * i);
    assert.equal(rec.scheduled_utc, C.formatUtc(T_MS + (7200 + 60 * i) * 1000));
    if (i === 0) {
      assert.equal(rec.flags_mask, F.CPU_BASELINE_ONLY);
      assert.ok(Object.values(rec.cpu_interval).every((v) => v === null), "slot 0 is a baseline, not zero CPU");
      return;
    }
    const exp = expectedRates(i);
    intervals++;
    assert.deepEqual(rec.cpu_interval, {
      previous_i: i - 1, start_anchor_mono_ns: FIX.series[i - 1]!.anchor_ns, end_anchor_mono_ns: FIX.series[i]!.anchor_ns,
      delta_mono_ns: exp.ns.toString(), delta_cpu_ticks: exp.ticks.toString(),
      one_core_micropercent: exp.one.toString(), allocated_micropercent: exp.alloc!.toString(),
    });
  });
  assert.equal(intervals, 119, "120 snapshots are a baseline plus 119 adjacent deltas: 120 CPU minutes are unmet");

  assert.equal(fin.manifest.kind, "writer_claim");
  assert.equal(fin.manifest.coverage, "unknown", "a writer claim is never independently complete");
  assert.deepEqual(fin.manifest.counts, { present: 120, missing: 0, read_failed: 0, verified: 0, unknown: 0, qualified_rss: 120, qualified_adjacent_cpu: 119, incident_keys_present: 0 });
  assertManifestShape(fin.manifest);

  const rb = readMap(map);
  const m = rb.manifest;
  assertManifestShape(m);
  assert.equal(rb.result.stop, "none");
  assert.equal(rb.result.reads, 155, "descriptor, seal, claim, 120 records and 32 incident keys");
  assert.equal(new Set(rb.store.log.map((e) => e.key)).size, rb.store.log.length, "at most one read per key");
  assert.ok(rb.store.log.every((e) => e.op === "read"));
  assert.equal(m.kind, "independent_readback");
  assert.equal(m.principal_alias, DESC.reader_alias);
  assert.notEqual(m.principal_alias, DESC.writer_alias);
  assert.equal(m.coverage, "complete_structural");
  assert.equal(m.flags_mask, F.CPU_BASELINE_ONLY);
  assert.deepEqual(m.counts, { present: 120, missing: 0, read_failed: 0, verified: 120, unknown: 0, qualified_rss: 120, qualified_adjacent_cpu: 119, incident_keys_present: 0 });
  assert.equal(m.budget.keys_read, 155);
  assert.equal(m.budget.accounted_bytes, m.budget.key_bytes + m.budget.value_bytes);
  assert.ok(m.writer_seal_sha256 !== null && m.T_utc === DESC.T_utc);
  assert.equal(m.read_started_utc, READ_START);
  m.entries.forEach((e, i) => {
    assert.deepEqual([e.record_sha256, e.write_status, e.read_status], [FIX.record_sha256[i], "committed", "verified"]);
    assert.equal(e.flags_mask, i === 0 ? F.CPU_BASELINE_ONLY : 0);
  });
  assert.equal(rb.result.seal?.outcome, "finished");
  assert.equal(rb.result.seal?.last_i, 119);

  // The reader's outputs persist once; a second reader's manifest write finds the key taken, so it writes no seal.
  const target = map;
  assert.deepEqual(C.persistReaderOutputs(target, rb.result, DESC.run_alias), { manifest: "committed", seal: "committed" });
  const stored = (target.read(C.keyFor(DESC.run_alias, "manifest:reader")) as { value: string }).value;
  assert.deepEqual(C.decodeManifest(stored), { ok: true, value: m });
  assert.deepEqual(C.persistReaderOutputs(target, rb.result, DESC.run_alias), { manifest: "exists", seal: "skipped" });
  assert.equal((target.read(C.keyFor(DESC.run_alias, "manifest:reader")) as { value: string }).value, stored);
});

test("reader seal follows only a manifest this reader committed", () => {
  const { map, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  finishRun(writer);
  const { result } = readMap(map);
  const manifestKey = C.keyFor(DESC.run_alias, "manifest:reader");
  const sealKey = C.keyFor(DESC.run_alias, "reader-seal");

  const foreign = new C.PersistedMap();
  assert.equal(foreign.create(manifestKey, "foreign manifest bytes").kind, "created");
  assert.deepEqual(C.persistReaderOutputs(foreign, result, DESC.run_alias), { manifest: "exists", seal: "skipped" });
  assert.deepEqual(foreign.read(manifestKey), { kind: "value", value: "foreign manifest bytes", elapsed_ms: 0 }, "the foreign manifest is untouched");
  assert.equal(foreign.read(sealKey).kind, "absent", "a seal never attests to a manifest this reader did not commit");

  for (const [outcome, status] of [[TIMEOUT, "unknown"], [REJECTED, "failed"]] as const) {
    const spy = new SpyStore(new C.PersistedMap(), (op, key) => (op === "create" && key === manifestKey ? { outcome } : undefined));
    assert.deepEqual(C.persistReaderOutputs(spy, result, DESC.run_alias), { manifest: status, seal: "skipped" }, status);
    assert.equal(spy.count("create", sealKey), 0, `${status} manifest: no seal write`);
  }

  const deniedSeal = new SpyStore(new C.PersistedMap(), (op, key) => (op === "create" && key === sealKey ? { outcome: DENIED } : undefined));
  assert.deepEqual(C.persistReaderOutputs(deniedSeal, result, DESC.run_alias), { manifest: "committed", seal: "denied" });
  assert.equal(deniedSeal.count("create", sealKey), 1, "a denied seal write is terminal");
});

test("independent_reader: a destroyed writer, fresh serialized bytes, and tampering is detected", () => {
  let snapshot: string;
  {
    const { map, writer } = newRun({ reconcile: true });
    feed(writer, range(120));
    finishRun(writer);
    snapshot = map.snapshot();
  } // writer, store and any cache are unreachable from here
  const clean = readFrom(snapshot);
  assert.equal(clean.manifest.coverage, "complete_structural");

  const pairs = JSON.parse(snapshot) as [string, string][];
  const edit = (key: string, fn: (rec: Record<string, unknown>) => void): string => JSON.stringify(pairs.map(([k, v]) => {
    if (k !== key) return [k, v];
    const obj = JSON.parse(v) as Record<string, unknown>;
    fn(obj);
    return [k, C.canonicalize(obj)];
  }));
  // Change one counter after the writer finished; still canonical and schema-valid, but not what the writer committed.
  const tampered = readFrom(edit(rkey(40), (o) => { (o.observation as Record<string, unknown>).rss_bytes = "1"; }));
  const e40 = tampered.manifest.entries[40]!;
  assert.deepEqual([e40.read_status, e40.record_sha256], ["mismatch", null]);
  assert.ok(has(e40.flags_mask, "READ_MISMATCH"));
  assert.equal(tampered.manifest.coverage, "incomplete");
  assert.ok(has(tampered.manifest.entries[41]!.flags_mask, "NONADJACENT_CPU"), "a neighbour of an unverified record has no verified adjacent pair");
  assert.equal(tampered.manifest.counts.verified, 119);
  note(tampered.manifest.flags_mask);

  // No descriptor: nothing can be bound, so there is no manifest and the reader says so.
  const empty = readFrom("[]");
  assert.equal(empty.result.manifest, null);
  assert.equal(empty.result.flags, F.MISSING);
  assert.equal(empty.store.count("read"), 1);
  // The intended independent principal only: the writer's alias is refused after the one descriptor read.
  const asWriter = C.readback({
    store: C.PersistedMap.fromSnapshot(snapshot)!, sha256: sha, run_alias: DESC.run_alias, reader_alias: DESC.writer_alias,
    created_utc: CREATED, read_started_utc: READ_START, read_finished_utc: READ_END,
  });
  assert.equal(asWriter.manifest, null);
  assert.equal(asWriter.flags, F.AUTHORITY_MISSING);
  assert.equal(asWriter.reads, 1);
  note(asWriter.flags);
});

test("missing_each_index: all 120 removals; never stitched across the gap", () => {
  const { map, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  finishRun(writer);
  const pairs = pairsOf(map);
  for (let k = 0; k < 120; k++) {
    const rb = readFrom(JSON.stringify(pairs.filter(([key]) => key !== rkey(k))));
    const m = rb.manifest;
    assertManifestShape(m);
    const gone = m.entries[k]!;
    assert.deepEqual([gone.read_status, gone.record_sha256, gone.write_status], ["missing", null, "committed"], `k=${k}`);
    assert.equal(gone.flags_mask, F.MISSING, `k=${k}`);
    assert.equal(m.coverage, "incomplete");
    assert.equal(m.counts.missing, 1);
    assert.equal(m.counts.verified, 119);
    assert.equal(m.counts.qualified_rss, 119);
    const lost = (k >= 1 ? 1 : 0) + (k + 1 <= 119 ? 1 : 0);
    assert.equal(m.counts.qualified_adjacent_cpu, 119 - lost, `k=${k}: neither k nor k+1 keeps a delta`);
    if (k + 1 <= 119) {
      const after = m.entries[k + 1]!;
      assert.equal(after.read_status, "verified");
      assert.equal(after.record_sha256, FIX.record_sha256[k + 1]);
      assert.ok(has(after.flags_mask, "NONADJACENT_CPU"), `k=${k}`);
    }
    if (k + 2 <= 119) assert.ok(!has(m.entries[k + 2]!.flags_mask, "NONADJACENT_CPU"), "i-2 is never used to bridge");
    assert.ok(has(m.flags_mask, "MISSING"));
  }
});

test("duplicate_identical_each_index and duplicate_conflict_each_index: all 120 each", () => {
  for (let k = 0; k < 120; k++) {
    {
      const { map, store, writer } = newRun();
      feed(writer, range(k + 1));
      const before = recordText(map, k);
      const retry = baseSlot(k);
      retry.writer_received_utc = C.formatUtc(T_MS + 7_200_000 + 60_000 * k + 900)!;
      const r = writer.record(retry);
      assert.deepEqual([r.status, r.flags, r.stopped, r.record_sha256], ["duplicate_retry", F.DUPLICATE, false, FIX.record_sha256[k]], `k=${k}`);
      assert.equal(recordText(map, k), before, "original bytes untouched");
      assert.equal(store.count("create", rkey(k)), 1, "no second write for the same index");
      assert.equal(writer.committedCount, k + 1, "a retry is not a second sample");
      feed(writer, range(120).slice(k + 1));
      const fin = finishRun(writer);
      assert.equal(fin.outcome, "finished");
      const events = incidentsOf(map);
      assert.equal(events.length, 1);
      assert.deepEqual(
        [events[0]!.kind, events[0]!.ordinal, events[0]!.i, events[0]!.flags_mask, events[0]!.persisted_sha256],
        ["duplicate_retry", 0, k, F.DUPLICATE, FIX.record_sha256[k]],
      );
      assert.equal(events[0]!.candidate_sha256, sha(refCanon({ identity: baseSlot(k).identity, observation: baseSlot(k).observation })));
      assert.ok(has(fin.manifest.entries[k]!.flags_mask, "DUPLICATE"));
      const rb = readMap(map);
      assert.equal(rb.manifest.counts.verified, 120);
      assert.equal(rb.manifest.coverage, "incomplete", "a duplicate keeps the run from structural completeness");
      assert.ok(has(rb.manifest.flags_mask, "DUPLICATE"));
      note(rb.manifest.flags_mask);
    }
    {
      const { map, store, writer } = newRun();
      feed(writer, range(k + 1));
      const before = recordText(map, k);
      const changed = baseSlot(k);
      obsOf(changed).rss_bytes = "4096";
      const r = writer.record(changed);
      assert.deepEqual([r.status, r.flags, r.stopped], ["rejected", mask("DUPLICATE", "COLLECTOR_STOP"), true], `k=${k}`);
      assert.equal(recordText(map, k), before, "the original persisted bytes are preserved");
      const calls = store.log.length;
      assert.equal(writer.record(baseSlot(Math.min(k + 1, 119))).status, "refused");
      assert.equal(store.log.length, calls, "no I/O after the stop");
      const fin = finishRun(writer);
      assert.equal(fin.outcome, "stopped");
      assert.ok(fin.terminal_incident_persisted);
      const events = incidentsOf(map);
      assert.deepEqual(events.map((e) => [e.ordinal, e.kind]), [[0, "duplicate_conflict"], [31, "stop"]]);
      const rb = readMap(map);
      range(120).forEach((i) => {
        const e = rb.manifest.entries[i]!;
        if (i <= k) assert.equal(e.read_status, "verified");
        else {
          assert.deepEqual([e.read_status, e.write_status, e.flags_mask], ["missing", "not_attempted", mask("MISSING", "COLLECTOR_STOP")], `k=${k} i=${i}`);
        }
      });
      assert.equal(rb.manifest.coverage, "incomplete");
      assert.ok(has(rb.manifest.flags_mask, "DUPLICATE", "COLLECTOR_STOP"));
      note(rb.manifest.flags_mask);
    }
  }
});

test("out_of_order_each_index: slot k first for every k (k = 0 uses 'slot 1 before slot 0', the k = 1 order); flags survive sorting and no nearest-prior CPU is used", () => {
  for (let k = 0; k < 120; k++) {
    const early = k === 0 ? 1 : k;
    const order = k === 0 ? [1, 0, ...range(120).slice(2)] : [k, ...range(120).filter((x) => x !== k)];
    const { map, writer } = newRun();
    const results = feed(writer, order);
    results.forEach((r, idx) => {
      assert.equal(r.status, "committed");
      assert.equal(has(r.flags, "OUT_OF_ORDER"), idx < 2, `k=${k} arrival ${idx}: the early slot and then slot 0 are the only disordered arrivals`);
    });
    const first = recordAt(map, early);
    assert.equal(first.write_seq, 0, "write_seq follows arrival, not the index");
    assert.equal(first.flags_mask, mask("OUT_OF_ORDER", "NONADJACENT_CPU"));
    assert.ok(Object.values(first.cpu_interval).every((v) => v === null), "i-1 was absent: no interval, and not i-2 either");
    const zero = recordAt(map, 0);
    assert.equal(zero.write_seq, 1);
    assert.equal(zero.flags_mask, mask("OUT_OF_ORDER", "CPU_BASELINE_ONLY"));
    if (early + 1 <= 119) assert.equal(recordAt(map, early + 1).cpu_interval.previous_i, early, "the next slot pairs with its exact predecessor");
    const fin = finishRun(writer);
    assert.equal(fin.outcome, "finished");
    assert.deepEqual(incidentsOf(map).map((e) => [e.ordinal, e.kind, e.i]), [[0, "out_of_order", early], [1, "out_of_order", 0]]);
    const rb = readMap(map);
    const m = rb.manifest;
    assertManifestShape(m);
    assert.deepEqual(m.entries.map((e) => e.i), range(120), "the final manifest is sorted");
    assert.ok(has(m.entries[early]!.flags_mask, "OUT_OF_ORDER", "NONADJACENT_CPU"), "sorting does not erase the arrival flag");
    assert.ok(has(m.entries[0]!.flags_mask, "OUT_OF_ORDER"));
    assert.equal(m.counts.verified, 120);
    assert.equal(m.counts.qualified_adjacent_cpu, 118);
    assert.equal(m.coverage, "incomplete");
    note(m.flags_mask);
  }
});

test("late_arrival_each_index: slot e+1 arrives before slot e for every e in 1..118; only the late slot is disordered", () => {
  for (let e = 1; e < 119; e++) {
    const { map, writer } = newRun();
    const order = [...range(e), e + 1, e, ...range(120).slice(e + 2)];
    const results = feed(writer, order);
    assert.ok(results.every((r) => r.status === "committed" && !r.stopped), `e=${e}`);
    const flagged = results.filter((r) => has(r.flags, "OUT_OF_ORDER")).map((r) => r.i);
    assert.deepEqual(flagged, [e], `e=${e}: slot e arrives below slot e+1`);
    assert.equal(recordAt(map, e + 1).flags_mask, F.NONADJACENT_CPU, "slot e+1 had no slot e to pair with");
    assert.ok(Object.values(recordAt(map, e + 1).cpu_interval).every((v) => v === null));
    assert.equal(recordAt(map, e).flags_mask, F.OUT_OF_ORDER, "slot e still pairs with slot e-1");
    assert.equal(recordAt(map, e).cpu_interval.previous_i, e - 1);
    if (e + 2 <= 119) assert.equal(recordAt(map, e + 2).cpu_interval.previous_i, e + 1);
    assert.equal(finishRun(writer).outcome, "finished");
    const m = readMap(map).manifest;
    assert.equal(m.counts.qualified_adjacent_cpu, 118);
    assert.ok(has(m.entries[e]!.flags_mask, "OUT_OF_ORDER") && has(m.entries[e + 1]!.flags_mask, "NONADJACENT_CPU"));
    note(m.flags_mask);
  }
});

test("skipped_slot_each_index: a gap is never bridged with the slot before it", () => {
  for (let k = 0; k < 120; k++) {
    const { map, writer } = newRun();
    const results = feed(writer, range(120).filter((i) => i !== k));
    assert.ok(results.every((r) => r.status === "committed" && !r.stopped), `k=${k}`);
    if (k < 119) {
      const after = recordAt(map, k + 1);
      assert.equal(after.flags_mask, C.orMask(F.NONADJACENT_CPU, k === 0 ? F.OUT_OF_ORDER : 0), `k=${k}`);
      assert.ok(Object.values(after.cpu_interval).every((v) => v === null), "slot k-1 must not stand in for the missing slot k");
      if (k + 2 <= 119) assert.equal(recordAt(map, k + 2).cpu_interval.previous_i, k + 1);
    }
    const fin = finishRun(writer);
    assert.equal(fin.outcome, "stopped", "a run that covered 119 slots did not finish");
    assert.deepEqual([fin.manifest.entries[k]!.write_status, fin.manifest.entries[k]!.flags_mask], ["not_attempted", F.COLLECTOR_STOP]);
    const m = readMap(map).manifest;
    assert.deepEqual([m.entries[k]!.read_status, m.entries[k]!.flags_mask], ["missing", mask("MISSING", "COLLECTOR_STOP")]);
    assert.equal(m.coverage, "incomplete");
    assert.equal(m.counts.qualified_adjacent_cpu, 119 - (k >= 1 ? 1 : 0) - (k + 1 <= 119 ? 1 : 0));
  }
});

test("zero_anchor_each_index: an anchor of \"0\" or a missing system tick on slot i-1 is not usable, so slot i is NONADJACENT_CPU with no interval", () => {
  for (let e = 1; e < 120; e++) {
    const prev = clone(baseSlot(e - 1));
    obsOf(prev).cpu_anchor_mono_ns = "0";
    obsOf(prev).mono_start_ns = "0";
    obsOf(prev).mono_end_ns = "9000000";
    const pair = C.computeCpuInterval(e, baseSlot(e), baseSlot(e - 1));
    assert.equal(pair.flags, 0, `control e=${e}: the untouched predecessor pairs`);
    const zero = C.computeCpuInterval(e, baseSlot(e), prev);
    assert.equal(zero.flags, F.NONADJACENT_CPU, `e=${e}`);
    assert.ok(Object.values(zero.interval).every((v) => v === null), `e=${e}: no span measured from the epoch`);
  }
  for (let e = 1; e < 120; e++) {
    const prev = clone(baseSlot(e - 1));
    obsOf(prev).cpu_system_ticks = null;
    const lost = C.computeCpuInterval(e, baseSlot(e), prev);
    assert.equal(lost.flags, F.NONADJACENT_CPU, `e=${e}: a missing system tick on slot i-1`);
    assert.ok(Object.values(lost.interval).every((v) => v === null), `e=${e}: no interval from a missing counter`);
  }
});

test("counter reset and wrap at every slot 1..119; slot 0 is only a baseline", () => {
  for (const v of ["0", U64_MAX.toString()]) {
    const { map, writer } = newRun();
    const r = feed(writer, [0], (s) => { obsOf(s).cpu_user_ticks = v; obsOf(s).cpu_system_ticks = v; return s; })[0]!;
    assert.equal(r.flags, F.CPU_BASELINE_ONLY, "no presumed preceding zero");
    assert.ok(Object.values(recordAt(map, 0).cpu_interval).every((x) => x === null));
  }
  const series = FIX.series;
  for (let i = 1; i < 120; i++) {
    for (const kind of ["user", "system", "wrap"] as const) {
      const { map, writer } = newRun();
      const results = feed(writer, range(120), (s, at) => {
        if (kind === "user" && at === i) obsOf(s).cpu_user_ticks = (BigInt(series[i - 1]!.utime) - 1n).toString();
        if (kind === "system" && at === i) obsOf(s).cpu_system_ticks = (BigInt(series[i - 1]!.stime) - 1n).toString();
        if (kind === "wrap" && at === i - 1) obsOf(s).cpu_user_ticks = U64_MAX.toString();
        if (kind === "wrap" && at === i) obsOf(s).cpu_user_ticks = "0";
        return s;
      });
      assert.ok(results.every((r) => r.status === "committed" && !r.stopped), `${kind} i=${i}: a counter reset does not stop the run`);
      const rec = recordAt(map, i);
      assert.ok(has(rec.flags_mask, "COUNTER_RESET_OR_WRAP"), `${kind} i=${i}`);
      assert.ok(Object.values(rec.cpu_interval).every((v) => v === null), "rate and deltas are null; no repair");
      assert.equal(rec.observation!.cpu_user_ticks, kind === "user" ? (BigInt(series[i - 1]!.utime) - 1n).toString() : kind === "wrap" ? "0" : series[i]!.utime, "raw values stay inspectable");
      if (kind === "wrap" && i >= 2) {
        const before = recordAt(map, i - 1);
        assert.ok(has(before.flags_mask, "INVALID_SCHEMA"), "an unrepresentable rate is flagged, not saturated");
        assert.equal(before.cpu_interval.one_core_micropercent, null);
        assert.equal(before.cpu_interval.delta_cpu_ticks, (U64_MAX - BigInt(series[i - 2]!.utime) + (BigInt(series[i - 1]!.stime) - BigInt(series[i - 2]!.stime))).toString());
      }
      finishRun(writer);
      const m = readMap(map).manifest;
      assert.equal(m.counts.verified, 120);
      assert.equal(m.coverage, "incomplete");
      assert.ok(has(m.entries[i]!.flags_mask, "COUNTER_RESET_OR_WRAP"));
      note(m.flags_mask);
    }
  }
});

// ---------------------------------------------------------------------------
// Identity, provenance and clocks
// ---------------------------------------------------------------------------

const IDENTITY_CHANGES: Record<string, { apply: (id: C.IdentityV1) => void; extra: number }> = {
  bot_source_sha: { apply: (id) => { id.bot_source_sha = "1".repeat(40); }, extra: 0 },
  worker_source_sha: { apply: (id) => { id.worker_source_sha = "2".repeat(40); }, extra: 0 },
  bot_build_id: { apply: (id) => { id.bot_build_id = "synthetic-build-2"; }, extra: 0 },
  container_image_sha256: { apply: (id) => { id.container_image_sha256 = "3".repeat(64); }, extra: 0 },
  worker_version_id: { apply: (id) => { id.worker_version_id = "00000000-0000-4000-8000-00000000b2a2"; }, extra: 0 },
  process_alias: { apply: (id) => { id.process_alias = "4".repeat(32); }, extra: 0 },
  process_start_ticks: { apply: (id) => { id.process_start_ticks = "424243"; }, extra: 0 },
  startup_observed_utc: { apply: (id) => { id.startup_observed_utc = "2000-01-01T00:00:01.000Z"; }, extra: 0 },
  startup_mono_ns: { apply: (id) => { id.startup_mono_ns = "1000000001"; }, extra: 0 },
  clock_kind: { apply: (id) => { id.clock_kind = "unavailable"; }, extra: F.CLOCK_UNKNOWN },
  cpu_counter_width_bits: { apply: (id) => { id.cpu_counter_width_bits = 32; }, extra: F.CPU_WIDTH_UNKNOWN },
  cpu_ticks_per_second: { apply: (id) => { id.cpu_ticks_per_second = 250; }, extra: 0 },
  allocated_vcpu_milli: { apply: (id) => { id.allocated_vcpu_milli = 2000; }, extra: 0 },
  limits_receipt_sha256: { apply: (id) => { id.limits_receipt_sha256 = "5".repeat(64); }, extra: 0 },
};

test("identity_change_each_field x slots 1..119: the changed observation is kept, flagged, and the run stops", () => {
  assert.deepEqual(Object.keys(IDENTITY_CHANGES), C.IDENTITY_FIELDS, "every frozen identity field is covered");
  let combinations = 0;
  for (const [field, change] of Object.entries(IDENTITY_CHANGES)) {
    for (let i = 1; i < 120; i++) {
      combinations++;
      const { map, store, writer } = newRun();
      const before = feed(writer, range(i));
      assert.ok(before.every((r) => r.status === "committed" && !r.stopped));
      const slot = baseSlot(i);
      change.apply(slot.identity);
      const r = writer.record(slot);
      const expected = C.orMask(F.IDENTITY_CHANGE, change.extra);
      assert.deepEqual([r.status, r.flags, r.stopped], ["committed", expected, true], `${field} @${i}`);
      const rec = recordAt(map, i);
      assert.equal(rec.flags_mask, expected);
      assert.ok(Object.values(rec.cpu_interval).every((v) => v === null), "no continuity across the change");
      const calls = store.log.length;
      assert.equal(writer.record(baseSlot(Math.min(i + 1, 119))).status, "refused");
      assert.equal(store.log.length, calls, "stopped: no further I/O");
      const fin = finishRun(writer);
      assert.equal(fin.outcome, "stopped");
      assert.equal(fin.seal?.last_i, i);
      assert.deepEqual(incidentsOf(map).map((e) => [e.ordinal, e.kind]), [[0, "identity_change"], [31, "stop"]]);
      const m = readMap(map).manifest;
      assert.equal(m.entries[i]!.read_status, "verified", `${field} @${i}: the changed observation is a persisted record`);
      assert.ok(has(m.entries[i]!.flags_mask, "IDENTITY_CHANGE"));
      assert.equal(m.coverage, "incomplete");
      if (i < 119) assert.equal(m.entries[i + 1]!.flags_mask, mask("MISSING", "COLLECTOR_STOP"));
      note(m.flags_mask);
    }
  }
  assert.equal(combinations, 14 * 119);
});

test("provenance_null_each_field: nulls stay null, flags say unknown, no deployed value is synthesized", () => {
  const nullable = [
    "bot_source_sha", "worker_source_sha", "bot_build_id", "container_image_sha256", "worker_version_id", "process_alias",
    "process_start_ticks", "startup_observed_utc", "startup_mono_ns", "cpu_counter_width_bits", "cpu_ticks_per_second",
    "allocated_vcpu_milli", "limits_receipt_sha256",
  ] as const;
  const provenance = nullable.slice(0, 9);
  const cases: { name: string; fields: readonly string[]; clock?: boolean }[] = [
    ...nullable.map((f) => ({ name: f, fields: [f] })),
    { name: "all nullable fields together", fields: nullable },
    { name: "clock unavailable", fields: [], clock: true },
  ];
  for (const c of cases) {
    const desc = clone(DESC);
    for (const f of c.fields) (desc.initial_identity as unknown as Record<string, unknown>)[f] = null;
    if (c.clock) desc.initial_identity.clock_kind = "unavailable";
    const second = newRun({ desc });
    assert.ok(second.opened.opened, c.name);
    const out = range(120).map((i) => second.writer.record(baseSlot(i, desc.initial_identity)));
    assert.ok(out.every((r) => r.status === "committed" && !r.stopped), c.name);
    const only = new Set(c.fields);
    const unknownProvenance = c.fields.some((f) => (provenance as readonly string[]).includes(f));
    const noWidth = only.has("cpu_counter_width_bits") || only.has("limits_receipt_sha256");
    const noHz = only.has("cpu_ticks_per_second") || only.has("limits_receipt_sha256");
    const noVcpu = only.has("allocated_vcpu_milli") || only.has("limits_receipt_sha256");
    const noClock = c.clock === true;
    const expectedFlags = C.orMask(
      unknownProvenance ? F.IDENTITY_UNKNOWN : 0, noWidth ? F.CPU_WIDTH_UNKNOWN : 0, noHz ? F.CPU_HZ_UNKNOWN : 0,
      noVcpu ? F.VCPU_UNKNOWN : 0, noClock ? F.CLOCK_UNKNOWN : 0,
    );
    const hasInterval = !unknownProvenance && !noWidth && !noClock;
    for (let i = 0; i < 120; i++) {
      const rec = recordAt(second.map, i);
      assert.equal(rec.flags_mask, C.orMask(expectedFlags, i === 0 ? F.CPU_BASELINE_ONLY : 0), `${c.name} @${i}`);
      assert.deepEqual(rec.identity, desc.initial_identity, "frozen provenance is persisted as given, including nulls");
      if (i === 0 || !hasInterval) {
        assert.ok(Object.values(rec.cpu_interval).every((v) => v === null), `${c.name} @${i} has no interval`);
      } else {
        const exp = expectedRates(i, 100n, noVcpu ? null : 1000n);
        assert.equal(rec.cpu_interval.delta_cpu_ticks, exp.ticks.toString());
        assert.equal(rec.cpu_interval.one_core_micropercent, noHz ? null : exp.one.toString(), c.name);
        assert.equal(rec.cpu_interval.allocated_micropercent, noHz || noVcpu ? null : exp.alloc!.toString(), c.name);
      }
    }
    assert.equal(finishRun(second.writer).outcome, "finished", c.name);
    const m = readMap(second.map, { desc }).manifest;
    assertManifestShape(m);
    assert.equal(m.counts.verified, 120);
    const ratesKnown = hasInterval && !noHz;
    assert.equal(m.counts.qualified_adjacent_cpu, ratesKnown ? 119 : 0, c.name);
    const tolerated = (expectedFlags & ~C.STRUCTURAL_TOLERATED_FLAGS) >>> 0;
    assert.equal(m.coverage, tolerated === 0 ? "complete_structural" : "incomplete", c.name);
  }
});

test("clock_and_slot: slot drift, reversed clocks, zero or backwards anchors and the T+4h edge", () => {
  const AT = 5;
  const due = (i: number) => T_MS + (7200 + 60 * i) * 1000;
  const setWall = (s: C.SlotInput, start: number, end: number) => { obsOf(s).utc_start = C.formatUtc(start)!; obsOf(s).utc_end = C.formatUtc(end)!; };
  const setAnchor = (s: C.SlotInput, anchor: bigint) => {
    obsOf(s).cpu_anchor_mono_ns = anchor.toString();
    obsOf(s).mono_start_ns = (anchor > 1_000_000n ? anchor - 1_000_000n : 0n).toString();
    obsOf(s).mono_end_ns = (anchor + 9_000_000n).toString();
  };
  const cases: { name: string; at?: number; mutate: (s: C.SlotInput) => void; flags: number; stop: boolean }[] = [
    { name: "exactly due", mutate: () => {}, flags: 0, stop: false },
    { name: "positive drift inside the slot", mutate: (s) => setWall(s, due(AT) + 1, due(AT) + 26), flags: F.JITTER_UNACCEPTED, stop: false },
    { name: "negative drift", mutate: (s) => setWall(s, due(AT) - 1, due(AT) + 24), flags: C.orMask(F.OUTSIDE_SLOT, F.JITTER_UNACCEPTED), stop: false },
    { name: "spans into the next minute", mutate: (s) => setWall(s, due(AT), due(AT) + 60_000), flags: F.OUTSIDE_SLOT, stop: false },
    { name: "last millisecond of the slot", mutate: (s) => setWall(s, due(AT), due(AT) + 59_999), flags: 0, stop: false },
    { name: "reversed wall bracket", mutate: (s) => setWall(s, due(AT) + 30, due(AT) + 10), flags: C.orMask(F.CLOCK_DISAGREEMENT, F.JITTER_UNACCEPTED), stop: false },
    { name: "wall time earlier than the previous slot", mutate: (s) => setWall(s, due(AT - 1) - 1000, due(AT - 1) - 975), flags: C.orMask(F.CLOCK_DISAGREEMENT, F.OUTSIDE_SLOT, F.JITTER_UNACCEPTED), stop: false },
    { name: "zero anchor", mutate: (s) => setAnchor(s, 0n), flags: C.orMask(F.CLOCK_UNKNOWN, F.CLOCK_RESET), stop: true },
    { name: "backwards anchor", mutate: (s) => setAnchor(s, BigInt(FIX.series[AT - 1]!.anchor_ns) - 1n), flags: F.CLOCK_RESET, stop: true },
    { name: "repeated anchor", mutate: (s) => setAnchor(s, BigInt(FIX.series[AT - 1]!.anchor_ns)), flags: F.CLOCK_RESET, stop: true },
    { name: "anchor before process start", mutate: (s) => setAnchor(s, 999_999_999n), flags: F.CLOCK_RESET, stop: true },
    { name: "mixed clock kinds", mutate: (s) => { s.identity.clock_kind = "unavailable"; }, flags: C.orMask(F.IDENTITY_CHANGE, F.CLOCK_UNKNOWN), stop: true },
    { name: "last slot ends exactly at T+4h", at: 119, mutate: (s) => setWall(s, due(119), T_MS + 14_400_000), flags: C.orMask(F.OUTSIDE_SLOT, F.DEADLINE), stop: true },
    { name: "last slot ends one millisecond before T+4h", at: 119, mutate: (s) => setWall(s, due(119), T_MS + 14_399_999), flags: 0, stop: false },
  ];
  for (const c of cases) {
    const at = c.at ?? AT;
    const { map, writer } = newRun();
    feed(writer, range(at));
    const slot = baseSlot(at);
    c.mutate(slot);
    const r = writer.record(slot);
    assert.deepEqual([r.status, r.flags, r.stopped], ["committed", c.flags, c.stop], c.name);
    assert.equal(recordAt(map, at).flags_mask, c.flags, c.name);
    if (has(c.flags, "CLOCK_RESET", "CLOCK_UNKNOWN") || has(c.flags, "IDENTITY_CHANGE")) {
      assert.ok(Object.values(recordAt(map, at).cpu_interval).every((v) => v === null), c.name);
    }
    note(c.flags);
  }
  // No accepted jitter, skew or active-time definition exists: nothing here produces a pass.
  const { writer } = newRun();
  assert.equal(writer.record({ ...baseSlot(0), i: 120 }).status, "rejected", "slot 120 is outside the schedule");
});

// ---------------------------------------------------------------------------
// Stop, failed and ambiguous writes, failed and mismatched reads
// ---------------------------------------------------------------------------

test("stopped_each_index: stop before k for all 120 k; prior bytes immutable, nothing caught up", () => {
  for (let k = 0; k < 120; k++) {
    const { map, store, writer } = newRun();
    feed(writer, range(k));
    const before = pairsOf(map);
    writer.stop();
    assert.equal(writer.phase, "stopped");
    const calls = store.log.length;
    assert.deepEqual(feed(writer, range(120).slice(k)).map((r) => r.status), Array(120 - k).fill("refused"), `k=${k}`);
    assert.equal(store.log.length, calls, "refused submissions perform no I/O and no catch-up sample");
    const fin = finishRun(writer);
    assert.equal(fin.outcome, "stopped");
    assert.equal(fin.seal?.last_i, k === 0 ? null : k - 1);
    assert.ok(fin.seal_persisted && fin.manifest_persisted && fin.terminal_incident_persisted);
    const after = pairsOf(map);
    for (const [key, value] of before) assert.equal(after.find(([a]) => a === key)?.[1], value, "committed bytes are unchanged");
    assert.equal(after.filter(([key]) => key.includes(":record:")).length, k, "no record exists at or after the stop");
    const rb = readMap(map);
    const m = rb.manifest;
    assertManifestShape(m);
    range(120).forEach((i) => {
      const e = m.entries[i]!;
      if (i < k) assert.equal(e.read_status, "verified");
      else assert.deepEqual([e.read_status, e.write_status, e.record_sha256, e.flags_mask], ["missing", "not_attempted", null, mask("MISSING", "COLLECTOR_STOP")], `k=${k} i=${i}`);
    });
    assert.equal(m.coverage, "incomplete");
    assert.deepEqual(incidentsOf(map).map((e) => [e.ordinal, e.kind]), [[31, "stop"]]);
    assert.equal(rb.result.seal?.outcome, "finished" /* the reader finished reading */);
    note(m.flags_mask);
  }
});

test("failed_write_each_index and ambiguous_write_each_index: 120 each, no hash for unsaved bytes", () => {
  for (let k = 0; k < 120; k++) {
    // Definite rejection on both bounded attempts.
    {
      const { map, store, writer } = newRun({ script: (op, key) => (op === "create" && key === rkey(k) ? { outcome: REJECTED } : undefined) });
      const results = feed(writer, range(k + 1));
      const r = results[k]!;
      assert.deepEqual([r.status, r.flags, r.record_sha256, r.stopped], ["failed", F.FAILED_WRITE, null, true], `k=${k}`);
      assert.equal(store.count("create", rkey(k)), 2, "at most two same-key attempts");
      assert.equal(store.count("read"), 0);
      assert.equal(map.read(rkey(k)).kind, "absent");
      const fin = finishRun(writer);
      assert.equal(fin.outcome, "stopped");
      const e = fin.manifest.entries[k]!;
      assert.deepEqual([e.write_status, e.record_sha256, e.flags_mask], ["failed", null, F.FAILED_WRITE]);
      assert.equal(fin.seal?.last_i, k === 0 ? null : k - 1);
      const m = readMap(map).manifest;
      assert.equal(m.entries[k]!.read_status, "missing");
      assert.equal(m.entries[k]!.write_status, "failed");
      assert.equal(m.coverage, "incomplete");
      note(fin.manifest.flags_mask, m.flags_mask);
    }
    // Timeouts on both attempts, no reconciliation authority: outcome stays unknown.
    {
      const { map, store, writer } = newRun({ script: (op, key) => (op === "create" && key === rkey(k) ? { outcome: TIMEOUT } : undefined) });
      const r = feed(writer, range(k + 1))[k]!;
      assert.deepEqual([r.status, r.flags, r.record_sha256, r.stopped], ["unknown", F.WRITE_OUTCOME_UNKNOWN, null, true], `k=${k}`);
      assert.equal(store.count("create", rkey(k)), 2);
      assert.equal(store.count("read"), 0, "no reconciliation read without authority");
      const fin = finishRun(writer);
      assert.equal(fin.outcome, "unknown");
      assert.deepEqual([fin.manifest.entries[k]!.write_status, fin.manifest.entries[k]!.record_sha256], ["unknown", null]);
      assert.ok(has(fin.manifest.flags_mask, "WRITE_OUTCOME_UNKNOWN"));
      note(fin.manifest.flags_mask);
    }
    // Authorized: one same-key read resolves it. The first attempt did land, so the bytes are identical and counted once.
    {
      const { map, store, writer } = newRun({ reconcile: true, script: (op, key, nth) => (op === "create" && key === rkey(k) && nth === 1 ? { outcome: TIMEOUT, land: true } : undefined) });
      const results = feed(writer, range(120));
      assert.ok(results.every((x) => x.status === "committed"), `k=${k}`);
      assert.equal(store.count("read", rkey(k)), 1, "exactly one reconciliation read");
      assert.equal(store.count("create", rkey(k)), 2);
      assert.equal(results[k]!.record_sha256, FIX.record_sha256[k]);
      assert.deepEqual(incidentsOf(map).map((e) => [e.kind, e.i]), [["duplicate_retry", k]]);
      assert.equal(finishRun(writer).outcome, "finished");
    }
    // Authorized, but the bytes never landed: still unknown.
    {
      const { store, writer } = newRun({ reconcile: true, script: (op, key) => (op === "create" && key === rkey(k) ? { outcome: TIMEOUT } : undefined) });
      const r = feed(writer, range(k + 1))[k]!;
      assert.equal(r.status, "unknown");
      assert.equal(store.count("read", rkey(k)), 1);
    }
  }
});

test("failed_read_each_index and read_mismatch_each_index: 120 each; a read failure is not absence", () => {
  const { map, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  finishRun(writer);
  const snapshot = map.snapshot();
  const pairs = JSON.parse(snapshot) as [string, string][];
  const replace = (key: string, value: string): string => JSON.stringify(pairs.map(([k, v]) => (k === key ? [k, value] : [k, v])));
  const retag = (key: string, fn: (o: Record<string, unknown>) => void): string => {
    const original = JSON.parse(pairs.find(([k]) => k === key)![1]) as Record<string, unknown>;
    fn(original);
    return replace(key, C.canonicalize(original));
  };
  for (let k = 0; k < 120; k++) {
    for (const outcome of [{ kind: "error", elapsed_ms: 5 }, { kind: "value", value: "{}", elapsed_ms: 2001 }] as C.ReadOutcome[]) {
      const rb = readFrom(snapshot, { script: (op, key) => (op === "read" && key === rkey(k) ? { outcome } : undefined) });
      const e = rb.manifest.entries[k]!;
      assert.deepEqual([e.read_status, e.record_sha256, e.flags_mask], ["failed", null, F.FAILED_READ], `k=${k}`);
      assert.ok(!has(e.flags_mask, "MISSING"), "unreadable is not missing");
      assert.equal(rb.manifest.coverage, "incomplete");
      assert.equal(rb.manifest.counts.read_failed, 1);
      assert.equal(rb.result.reads, 155, "later keys are still read");
      note(rb.manifest.flags_mask);
    }
    // First permission denial: stop at once, zero further reads.
    {
      const rb = readFrom(snapshot, { script: (op, key) => (op === "read" && key === rkey(k) ? { outcome: DENIED } : undefined) });
      assert.equal(rb.result.stop, "denied");
      assert.equal(rb.store.log[rb.store.log.length - 1]!.key, rkey(k), "the denied read is the last read issued");
      assert.equal(rb.manifest.entries[k]!.read_status, "failed");
      range(120).filter((i) => i > k).forEach((i) => {
        assert.deepEqual([rb.manifest.entries[i]!.read_status, rb.manifest.entries[i]!.flags_mask], ["not_attempted", F.TRUNCATED], `k=${k} i=${i}`);
      });
      assert.ok(has(rb.manifest.flags_mask, "TRUNCATED", "FAILED_READ"));
      assert.equal(rb.manifest.coverage, "incomplete");
      const calls = rb.store.log.length;
      assert.deepEqual(C.persistReaderOutputs(rb.store, rb.result, DESC.run_alias), { manifest: "skipped", seal: "skipped" }, `k=${k}`);
      assert.equal(rb.store.count("create"), 0, "a denied reader writes neither its manifest nor its seal");
      assert.equal(rb.store.log.length, calls, "persisting after the denial initiates no I/O");
      note(rb.manifest.flags_mask);
    }
    // Mismatches: a tampered value, a stale record under the wrong key, a record of another run, non-canonical bytes.
    const next = (k + 1) % 120;
    const variants: [string, string][] = [
      ["tampered counter", retag(rkey(k), (o) => { (o.observation as Record<string, unknown>).cpu_user_ticks = "7"; })],
      ["stale record from another slot", replace(rkey(k), pairs.find(([key]) => key === rkey(next))![1])],
      ["another run's record", retag(rkey(k), (o) => { o.run_alias = "f".repeat(32); })],
      ["non-canonical bytes", replace(rkey(k), JSON.stringify(JSON.parse(pairs.find(([key]) => key === rkey(k))![1]), null, 1))],
    ];
    for (const [name, snap] of variants) {
      const rb = readFrom(snap);
      const e = rb.manifest.entries[k]!;
      assert.equal(e.read_status, "mismatch", `${name} k=${k}`);
      assert.equal(e.record_sha256, null);
      assert.ok(has(e.flags_mask, "READ_MISMATCH"), name);
      assert.ok(!has(e.flags_mask, "MISSING", "FAILED_READ"), name);
      assert.equal(rb.manifest.coverage, "incomplete");
      assert.equal(rb.manifest.counts.present, 120, "present but unverified");
      assert.equal(rb.manifest.counts.verified, 119);
      note(rb.manifest.flags_mask);
    }
  }
});

test("reader budget: 300 s wall time bounds the reads, one attempt per key, then everything left is truncated", () => {
  const { map, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  finishRun(writer);
  const stuck = C.PersistedMap.fromSnapshot(map.snapshot())!;
  const log: string[] = [];
  const slowStore: C.PersistedStore = {
    create: (k, v) => stuck.create(k, v),
    read: (key) => { log.push(key); const out = stuck.read(key); return out.kind === "value" ? { ...out, elapsed_ms: 2000 } : { ...out, elapsed_ms: 2000 }; },
  };
  const result = C.readback({ store: slowStore, sha256: sha, run_alias: DESC.run_alias, reader_alias: DESC.reader_alias, created_utc: CREATED, read_started_utc: READ_START, read_finished_utc: READ_END });
  assert.equal(result.reads, 150, "300000 ms / 2000 ms");
  assert.equal(result.stop, "deadline");
  assert.equal(new Set(log).size, log.length, "one attempt per key");
  const m = result.manifest!;
  assert.ok(has(m.flags_mask, "TRUNCATED", "DEADLINE"));
  assert.equal(m.counts.verified, 120, "all records were read before the budget ran out");
  assert.equal(m.coverage, "incomplete");
  assert.equal(result.seal?.outcome, "stopped");
  note(m.flags_mask);
});

// ---------------------------------------------------------------------------
// Seals, crashes, denial, budgets, concurrency
// ---------------------------------------------------------------------------

test("seal_crash_and_denial: a first denial is absorbing for the writer; crashes stay open and unknown", () => {
  // Crash with no seal.
  {
    const { map, writer } = newRun();
    feed(writer, range(120));
    const m = readMap(map).manifest;
    assert.ok(has(m.flags_mask, "OPEN_OR_UNSEALED"));
    assert.equal(m.coverage, "unknown");
    assert.equal(m.writer_seal_sha256, null);
    note(m.flags_mask);
    const partial = newRun();
    feed(partial.writer, range(60));
    const mp = readMap(partial.map).manifest;
    assert.equal(mp.coverage, "incomplete");
    assert.ok(has(mp.flags_mask, "OPEN_OR_UNSEALED", "MISSING"));
  }
  // Denial while opening.
  {
    const { store, writer, opened } = newRun({ script: (op, key) => (op === "create" && key.endsWith(":descriptor") ? { outcome: DENIED } : undefined) });
    assert.deepEqual(opened, { opened: false, flags: mask("FAILED_WRITE", "AUTHORITY_MISSING") });
    assert.equal(writer.phase, "denied");
    assert.equal(store.log.length, 1);
    const post = writer.record(baseSlot(0));
    assert.deepEqual([post.status, post.stopped, post.flags], ["refused", true, F.COLLECTOR_STOP]);
    assert.equal(store.log.length, 1, "no I/O after the denial");
  }
  // Denial at every record index.
  for (let k = 0; k < 120; k++) {
    const { map, store, writer } = newRun({ reconcile: true, script: (op, key) => (op === "create" && key === rkey(k) ? { outcome: DENIED } : undefined) });
    const results = feed(writer, range(k + 1));
    const r = results[k]!;
    assert.deepEqual([r.status, r.flags, r.stopped], ["denied", mask("FAILED_WRITE", "AUTHORITY_MISSING"), true], `k=${k}`);
    assert.equal(store.count("create", rkey(k)), 1, "a denial is never retried");
    const calls = store.log.length;
    assert.equal(writer.record(baseSlot(Math.min(k + 1, 119))).status, "refused");
    const fin = finishRun(writer, false);
    assert.equal(store.log.length, calls, `k=${k}: zero newly initiated I/O after the denial, including terminal writes`);
    assert.equal(fin.outcome, "unknown");
    assert.deepEqual([fin.seal_persisted, fin.manifest_persisted, fin.terminal_incident_persisted], [false, false, false]);
    assert.ok(has(fin.manifest.flags_mask, "OPEN_OR_UNSEALED", "AUTHORITY_MISSING", "COLLECTOR_STOP", "FAILED_WRITE"));
    for (let j = k + 1; j < 120; j++) assert.ok(has(fin.manifest.entries[j]!.flags_mask, "COLLECTOR_STOP", "AUTHORITY_MISSING"), `k=${k} j=${j}: a slot not attempted after the denial`);
    assert.equal(writer.persistedIncidents().length, 0);
    assert.deepEqual(writer.localOnlyIncidents().map((e) => e.kind), ["authority"]);
    assert.equal(map.read(C.keyFor(DESC.run_alias, "writer-seal")).kind, "absent");
    note(fin.manifest.flags_mask);
  }
  // Denial on the seal write blocks the manifest write.
  {
    const { store, writer } = newRun({ script: (op, key) => (op === "create" && key.endsWith(":writer-seal") ? { outcome: DENIED } : undefined) });
    feed(writer, range(10));
    writer.stop();
    const fin = finishRun(writer, false);
    assert.equal(store.count("create", C.keyFor(DESC.run_alias, "manifest:writer")), 0);
    assert.ok(!fin.seal_persisted && !fin.manifest_persisted);
  }
  // A reader output that times out stays unknown after two attempts.
  {
    const { map, writer } = newRun({ reconcile: true });
    feed(writer, range(120));
    finishRun(writer);
    const rb = readMap(map);
    const store = new SpyStore(map, (op, key) => (op === "create" && key.endsWith(":reader-seal") ? { outcome: TIMEOUT } : undefined));
    assert.deepEqual(C.persistReaderOutputs(store, rb.result, DESC.run_alias), { manifest: "committed", seal: "unknown" });
    assert.equal(store.count("create", C.keyFor(DESC.run_alias, "reader-seal")), 2);
  }
});

test("total_and_key_budget: 157 fixed keys, per-class caps, no 158th key, no second run, no spill", () => {
  const keys: string[] = [C.keyFor(DESC.run_alias, "descriptor")];
  range(120).forEach((i) => keys.push(rkey(i)));
  range(32).forEach((n) => keys.push(ekey(n)));
  keys.push(C.keyFor(DESC.run_alias, "writer-seal"), C.keyFor(DESC.run_alias, "manifest:writer"), C.keyFor(DESC.run_alias, "manifest:reader"), C.keyFor(DESC.run_alias, "reader-seal"));
  assert.equal(keys.length, 157);
  assert.equal(new Set(keys).size, 157);
  keys.forEach((key) => {
    assert.ok(utf8(key) <= 128);
    const parsed = C.parseKey(key);
    assert.ok(parsed, key);
    assert.equal(C.keyFor(parsed.alias, parsed.kind, parsed.index ?? undefined), key);
  });
  assert.equal(C.parseKey(rkey(0).replace(":000", ":120")), null, "index 120 is not a key");
  assert.equal(C.parseKey(ekey(0).replace(":00", ":32")), null);
  const cap = (key: string): number => {
    const kind = C.parseKey(key)!.kind;
    return kind === "descriptor" ? 4096 : kind === "record" ? 4096 : kind === "event" ? 1024 : kind.startsWith("manifest") ? 32768 : 4096;
  };
  const map = new C.PersistedMap();
  let value = 0;
  for (const key of keys) {
    assert.equal(map.create(key, "x".repeat(cap(key) + 1)).kind, "rejected", "one byte over the class cap");
    assert.equal(map.create(key, "x".repeat(cap(key))).kind, "created", "exactly the class cap");
    value += cap(key);
  }
  assert.equal(value, 602112, "maximum value bytes");
  assert.equal(map.size, 157);
  assert.ok(map.accountedBytes() <= 622208 && map.accountedBytes() < 1048576);
  assert.equal(map.create(keys[0]!, "y").kind, "exists", "no overwrite");
  const fresh = new C.PersistedMap();
  const over = (key: string, flags: number) => {
    const out = fresh.create(key, "v");
    assert.deepEqual(out, { kind: "rejected", elapsed_ms: 0, flags });
    note(out.kind === "rejected" ? (out.flags ?? 0) : 0);
  };
  over("k".repeat(129), F.BYTE_BUDGET);
  over(rkey(0).replace(":000", ":120"), F.ROW_BUDGET);
  over(C.keyFor(DESC.run_alias, "record", 0) + "/spill", F.ROW_BUDGET);
  over("not-a-key", F.ROW_BUDGET);
  assert.equal(fresh.create(rkey(0), "v").kind, "created");
  over(C.keyFor("a".repeat(32), "record", 0), F.CONCURRENT_WRITER);
  assert.equal(C.PersistedMap.fromSnapshot("[[\"not-a-key\",\"v\"]]"), null);
  assert.equal(C.PersistedMap.fromSnapshot("{"), null);
  // A snapshot of a complete run reloads byte for byte.
  const { map: run, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  finishRun(writer);
  assert.equal(C.PersistedMap.fromSnapshot(run.snapshot())!.snapshot(), run.snapshot());
});

test("event_budget_and_concurrency: 31 ordinary incidents, then the reserved terminal one; one writer, one run, one reader", () => {
  const { map, writer } = newRun();
  feed(writer, [0]);
  const retry = (): C.SlotResult => writer.record(baseSlot(0));
  for (let n = 0; n < 31; n++) assert.equal(retry().status, "duplicate_retry", `retry ${n}`);
  assert.deepEqual(incidentsOf(map).map((e) => e.ordinal), range(31));
  const before = pairsOf(map);
  const last = retry();
  assert.equal(last.stopped, true, "the 32nd ordinary incident hits the reserved slot and stops");
  assert.equal(writer.phase, "stopped");
  assert.equal(incidentsOf(map).length, 31, "nothing is written or deleted for the overflow");
  const fin = finishRun(writer);
  assert.ok(fin.terminal_incident_persisted);
  const events = incidentsOf(map);
  assert.equal(events.length, 32);
  assert.deepEqual([events[31]!.ordinal, events[31]!.kind], [31, "budget"]);
  assert.ok(has(events[31]!.flags_mask, "EVENT_BUDGET", "COLLECTOR_STOP"));
  for (const [key, value] of before) if (key.includes(":event:")) assert.equal(pairsOf(map).find(([a]) => a === key)?.[1], value, "oldest events are never deleted");
  const m = readMap(map).manifest;
  assert.equal(m.counts.incident_keys_present, 32);
  assert.ok(has(m.flags_mask, "EVENT_BUDGET"));
  note(m.flags_mask);

  // A second writer on the same run is refused without touching the first run's bytes.
  const run = newRun();
  feed(run.writer, range(3));
  const snapshot = run.map.snapshot();
  const second = new C.ProcessHistoryWriter({ store: run.map, sha256: sha, reconcile_read_authorized: true });
  assert.deepEqual(second.open(DESC), { opened: false, flags: F.CONCURRENT_WRITER });
  assert.equal(second.record(baseSlot(3)).status, "refused");
  assert.equal(run.map.snapshot(), snapshot);
  // A second run alias cannot join the same store.
  const other = clone(DESC);
  other.run_alias = "e".repeat(32);
  const third = new C.ProcessHistoryWriter({ store: run.map, sha256: sha, reconcile_read_authorized: true });
  const opened = third.open(other);
  assert.equal(opened.opened, false);
  assert.ok(has(opened.flags, "CONCURRENT_WRITER"));
  assert.equal(run.map.snapshot(), snapshot);
  note(opened.flags);
});

test("retention_and_restoration: the lease only ends eligibility; no delete, resume or takeover exists", () => {
  const sealed = "2000-01-02T04:00:05.000Z";
  assert.equal(C.leaseState(sealed, "2000-01-16T04:00:04.999Z"), "active");
  assert.equal(C.leaseState(sealed, "2000-01-16T04:00:05.000Z"), "frozen");
  assert.equal(C.leaseState(sealed, "2001-01-01T00:00:00.000Z"), "frozen");
  assert.equal(C.leaseState("nope", sealed), "invalid");
  assert.equal(C.INSPECTION_LEASE_DAYS, 14);
  const surface = [...Object.getOwnPropertyNames(C.PersistedMap.prototype), ...Object.getOwnPropertyNames(C.ProcessHistoryWriter.prototype)];
  for (const verb of ["delete", "remove", "clear", "set", "put", "overwrite", "list", "keys", "entries", "resume", "restore", "reset", "takeover", "catchUp", "repair"]) {
    assert.ok(!surface.some((name) => name.toLowerCase().startsWith(verb.toLowerCase())), `no ${verb}`);
  }
  const { map, writer } = newRun();
  feed(writer, range(10));
  writer.stop();
  finishRun(writer);
  const frozen = map.snapshot();
  const hashes = pairsOf(map).map(([k, v]) => [k, sha(v)]);
  // After a stop: no restart, no gap-filling, no second writer on the same run.
  assert.equal(writer.record(baseSlot(10)).status, "refused");
  assert.equal(new C.ProcessHistoryWriter({ store: map, sha256: sha, reconcile_read_authorized: true }).open(DESC).opened, false);
  assert.equal(map.snapshot(), frozen);
  assert.deepEqual(pairsOf(map).map(([k, v]) => [k, sha(v)]), hashes, "exact old hashes are preserved");
});

// ---------------------------------------------------------------------------
// Privacy, schema closure, bounds helpers
// ---------------------------------------------------------------------------

test("privacy_each_forbidden_field: raw identifiers and dynamic text are rejected with a fixed reason and never retained", () => {
  const SENTINEL = "SENTINEL-SECRET-VALUE";
  // One probe per fragment of the key-name heuristic, written out here rather than read from the module.
  const forbidden = [
    "xpid", "comm", "bootid", "cmdline", "argv", "env", "token", "secret", "password", "credential", "authorization", "url", "uri",
    "host", "path", "sql", "query", "message", "error", "reason", "detail", "content", "body", "discord", "guild", "channel",
    "snowflake", "user_id", "userid", "member", "process_id", "api_key", "apikey", "note_text",
  ];
  const record = recordFor(4);
  for (const key of forbidden) {
    const probes: [string, unknown, (v: unknown) => C.Checked<unknown>][] = [
      ["record", { ...record, [key]: SENTINEL }, C.validateRecord],
      ["record.identity", { ...record, identity: { ...record.identity, [key]: SENTINEL } }, C.validateRecord],
      ["record.observation", { ...record, observation: { ...record.observation, [key]: SENTINEL } }, C.validateRecord],
      ["record.cpu_interval", { ...record, cpu_interval: { ...record.cpu_interval, [key]: SENTINEL } }, C.validateRecord],
      ["descriptor", { ...DESC, [key]: SENTINEL }, C.validateDescriptor],
      ["descriptor.authority", { ...DESC, authority: { ...DESC.authority, [key]: SENTINEL } }, C.validateDescriptor],
    ];
    for (const [where, value, validate] of probes) {
      const out = validate(value);
      assert.ok(!out.ok, `${where}.${key}`);
      assert.ok(has(out.flags, "INVALID_SCHEMA", "PRIVACY_REJECTED"), `${where}.${key}`);
      assert.ok(!JSON.stringify(out).includes(SENTINEL), "the rejection never echoes the value");
      note(out.flags);
    }
  }
  const nested = C.validateRecord({ ...record, extra: { a: 1 } });
  assert.ok(!nested.ok && nested.flags === F.INVALID_SCHEMA, "an unknown nested property is invalid but not named private");
  // Forbidden material inside permitted string slots.
  const slots: [string, (r: C.RecordV1) => void][] = [
    ["URL", (r) => { r.identity.bot_build_id = "https://example.invalid/build"; }],
    ["path", (r) => { r.identity.bot_build_id = "/proc/self/stat"; }],
    ["whitespace", (r) => { r.identity.bot_build_id = "build 1"; }],
    ["bearer text", (r) => { r.identity.bot_build_id = "Bearer abc"; }],
    ["snowflake", (r) => { r.identity.process_alias = "123456789012345678"; }],
    ["email-like", (r) => { r.identity.bot_build_id = "a@b"; }],
    ["the reserved literal", (r) => { r.identity.bot_build_id = "unknown"; }],
  ];
  for (const [name, mutate] of slots) {
    const rec = clone(record);
    mutate(rec);
    const out = C.validateRecord(rec);
    assert.ok(!out.ok, name);
    if (name !== "the reserved literal") assert.ok(has(out.flags, "PRIVACY_REJECTED"), name);
  }
  // Every alternative of the value heuristic, in a slot where any non-hex text is already invalid.
  const privateValues = [
    "a:b", "a/b", "a\\b", "a@b", "a b", "bearer", "token", "secret", "password", "authorization", "select", "insert", "delete",
    "update", "drop", "12345678901234567", "12345678901234567890",
  ];
  for (const value of privateValues) {
    const rec = clone(record);
    rec.identity.process_alias = value;
    const out = C.validateRecord(rec);
    assert.ok(!out.ok && has(out.flags, "INVALID_SCHEMA", "PRIVACY_REJECTED"), value);
  }
  const plain = clone(record);
  plain.identity.process_alias = "zzzz";
  const plainOut = C.validateRecord(plain);
  assert.ok(!plainOut.ok && plainOut.flags === F.INVALID_SCHEMA, "a bad alias that looks harmless is invalid but not named private");
  // Through the writer: nothing is hashed, written or retained, and the run stops with a fixed reason.
  const { map, writer } = newRun();
  feed(writer, range(4));
  const injected = { ...baseSlot(4), pid: SENTINEL };
  const r = writer.record(injected);
  assert.deepEqual([r.status, r.stopped, r.record_sha256], ["rejected", true, null]);
  assert.ok(has(r.flags, "INVALID_SCHEMA", "PRIVACY_REJECTED"));
  assert.ok(!JSON.stringify(r).includes(SENTINEL));
  assert.equal(map.read(rkey(4)).kind, "absent");
  finishRun(writer);
  assert.ok(!map.snapshot().includes(SENTINEL), "no persisted byte carries the rejected payload");
  const privacy = incidentsOf(map).find((e) => e.kind === "privacy")!;
  assert.equal(privacy.candidate_sha256, null, "a privacy-rejected payload is never hashed as a redaction");
  note(r.flags);
  // Synthetic fields pass.
  assert.ok(C.validateRecord(record).ok);
  const desc = clone(DESC);
  desc.initial_identity.bot_build_id = null;
  // "unknown" is normalized to null at the input boundary, so it matches a frozen null build id.
  const fresh = new C.PersistedMap();
  const w2 = new C.ProcessHistoryWriter({ store: fresh, sha256: sha, reconcile_read_authorized: false });
  assert.ok(w2.open(desc).opened);
  const sl = baseSlot(0, desc.initial_identity);
  sl.identity.bot_build_id = "unknown";
  assert.equal(w2.record(sl).status, "committed");
  assert.equal(recordAt(fresh, 0).identity.bot_build_id, null);
});

function recordFor(i: number): C.RecordV1 {
  const { map, writer } = newRun();
  feed(writer, range(i + 1));
  return recordAt(map, i);
}

test("closed schemas: every required key, literal and range is enforced", () => {
  const rec = recordFor(9);
  assert.ok(C.validateRecord(rec).ok);
  for (const key of Object.keys(rec)) {
    const dropped = clone(rec) as unknown as Record<string, unknown>;
    delete dropped[key];
    assert.ok(!C.validateRecord(dropped).ok, `missing ${key}`);
  }
  const bad = (mutate: (r: C.RecordV1) => void): boolean => { const r = clone(rec); mutate(r); return C.validateRecord(r).ok; };
  assert.ok(!bad((r) => { r.i = 120; }));
  assert.ok(!bad((r) => { r.i = -1; }));
  assert.ok(!bad((r) => { r.scheduled_offset_seconds += 1; }));
  assert.ok(!bad((r) => { r.write_seq = 120; }));
  assert.ok(!bad((r) => { r.flags_mask = 4294967296; }));
  assert.ok(!bad((r) => { r.flags_mask = -1; }));
  assert.ok(!bad((r) => { r.flags_mask = C.orMask(r.flags_mask, F.CPU_BASELINE_ONLY); }), "the baseline flag belongs to slot 0 only");
  assert.ok(!bad((r) => { r.schema = "b2proc.record.v2" as "b2proc.record.v1"; }));
  assert.ok(!bad((r) => { r.observation!.rss_bytes = "01"; }));
  {
    // Each observation rule on its own, so no other rule can mask it.
    const o = obsOf(baseSlot(3));
    const observation = (mono_start_ns: string, mono_end_ns: string, cpu_anchor_mono_ns: string | null) => C.validateObservation({ ...o, mono_start_ns, mono_end_ns, cpu_anchor_mono_ns }).ok;
    assert.ok(observation("100", "200", "150"));
    assert.ok(observation("100", "100", "100"), "a zero-length bracket is fine");
    assert.ok(observation("100", "200", null), "no anchor, nothing to place");
    assert.ok(!observation("100", "50", null), "monotonic end before start (no anchor involved)");
    assert.ok(!observation("100", "200", "201"), "anchor after the bracket");
    assert.ok(!observation("100", "200", "99"), "anchor before the bracket");
  }
  {
    // A tombstone must say why: only the missing source flag differs between these two records.
    const base = recordFor(0);
    const tomb = clone(base);
    tomb.observation = null;
    tomb.flags_mask = C.orMask(F.CPU_BASELINE_ONLY, F.SOURCE_UNAVAILABLE);
    assert.ok(C.validateRecord(tomb).ok);
    tomb.flags_mask = F.CPU_BASELINE_ONLY;
    assert.ok(!C.validateRecord(tomb).ok, "a null observation without SOURCE_UNAVAILABLE");
  }
  assert.ok(!bad((r) => { r.cpu_interval.end_anchor_mono_ns = (BigInt(r.cpu_interval.end_anchor_mono_ns!) + 1n).toString(); r.cpu_interval.delta_mono_ns = (BigInt(r.cpu_interval.delta_mono_ns!) + 1n).toString(); }), "interval end must be the observation's own anchor");
  assert.ok(bad((r) => { r.identity.bot_build_id = "b".repeat(64); }), "64-character build id");
  assert.ok(!bad((r) => { r.identity.bot_build_id = "b".repeat(65); }), "65-character build id");
  for (const month of ["2001-04", "2001-06", "2001-09", "2001-11"]) {
    assert.ok(C.parseUtcMs(`${month}-30T00:00:00.000Z`) !== null, `${month}-30`);
    assert.equal(C.parseUtcMs(`${month}-31T00:00:00.000Z`), null, `${month}-31`);
  }
  for (const month of ["2001-01", "2001-03", "2001-05", "2001-07", "2001-08", "2001-10", "2001-12"]) {
    assert.ok(C.parseUtcMs(`${month}-31T00:00:00.000Z`) !== null, `${month}-31`);
  }
  assert.ok(!bad((r) => { r.cpu_interval.previous_i = 3; }));
  assert.ok(!bad((r) => { r.cpu_interval.one_core_micropercent = null; r.cpu_interval.allocated_micropercent = "1"; }));
  assert.ok(!bad((r) => { r.cpu_interval.delta_mono_ns = "1"; }));
  assert.ok(!bad((r) => { r.identity.cpu_ticks_per_second = 0; }));
  assert.ok(!bad((r) => { r.identity.cpu_counter_width_bits = 1.5 as number; }));
  assert.ok(!bad((r) => { r.identity.clock_kind = "monotonic" as "unavailable"; }));
  assert.ok(!bad((r) => { r.identity.worker_version_id = "00000000-0000-4000-8000-00000000B2A1"; }), "uppercase UUID");
  assert.ok(!bad((r) => { r.identity.bot_source_sha = "A".repeat(40); }), "uppercase hex");
  assert.ok(!bad((r) => { r.writer_received_utc = "2000-01-01T00:00:00Z"; }));
  assert.ok(bad((r) => { r.write_seq = null; r.writer_received_utc = null; }), "both are nullable");
  // Descriptor literals.
  const d = (mutate: (x: C.RunDescriptorV1) => void): boolean => { const x = clone(DESC); mutate(x); return C.validateDescriptor(x).ok; };
  assert.ok(d(() => {}));
  assert.ok(!d((x) => { x.writer_alias = x.reader_alias; }), "distinct intended principals");
  assert.ok(!d((x) => { x.mode = "live" as "offline"; }));
  assert.ok(!d((x) => { (x.schedule as { count: number }).count = 121; }));
  assert.ok(!d((x) => { (x.schedule as { step_seconds: number }).step_seconds = 61; }));
  assert.ok(!d((x) => { (x.fixture_bounds as { window_seconds: number }).window_seconds = 901; }));
  assert.ok(!d((x) => { (x.fixture_bounds as { expectation_cap: number }).expectation_cap = 21; }));
  assert.ok(!d((x) => { (x.fixture_bounds as { receipt_cap: number }).receipt_cap = 61; }));
  assert.ok(!d((x) => { (x.fixture_bounds as { full_receipt_cap_is_truncated: boolean }).full_receipt_cap_is_truncated = false; }));
  assert.ok(!d((x) => { (x as { logical_budget_bytes: number }).logical_budget_bytes = 1048577; }));
  assert.ok(!d((x) => { x.authority.storage_use = "not a hash"; }));
  assert.ok(d((x) => { x.authority.storage_use = "a".repeat(64); }));
  // Incident, seal and manifest closure.
  const inc: C.IncidentV1 = { schema: "b2proc.incident.v1", run_alias: DESC.run_alias, ordinal: 0, i: null, observed_utc: null, principal_alias: DESC.writer_alias, kind: "schema", flags_mask: F.INVALID_SCHEMA, candidate_sha256: null, persisted_sha256: null };
  for (const kind of C.INCIDENT_KINDS) {
    const terminal = kind === "stop" || kind === "budget";
    assert.ok(C.validateIncident({ ...inc, kind, ordinal: kind === "stop" ? 31 : 0 }).ok, kind);
    if (terminal) assert.ok(C.validateIncident({ ...inc, kind, ordinal: 31 }).ok, kind);
  }
  assert.ok(!C.validateIncident({ ...inc, kind: "schema", ordinal: 31 }).ok, "ordinal 31 is reserved for the terminal stop");
  assert.ok(!C.validateIncident({ ...inc, kind: "stop", ordinal: 5 }).ok);
  assert.ok(!C.validateIncident({ ...inc, kind: "free text" }).ok);
  assert.ok(!C.validateIncident({ ...inc, ordinal: 32 }).ok);
  assert.ok(!C.validateIncident({ ...inc, message: "x" }).ok);
  const seal: C.SealV1 = { schema: "b2proc.writer-seal.v1", run_alias: DESC.run_alias, principal_alias: DESC.writer_alias, last_i: 119, outcome: "finished", stopped_utc: null, flags_mask: 0, last_committed_record_sha256: "a".repeat(64), receipt_sha256: null };
  assert.ok(C.validateSeal(seal).ok);
  assert.ok(!C.validateSeal({ ...seal, last_i: 118 }).ok, "finished means slot 119");
  assert.ok(!C.validateSeal({ ...seal, last_committed_record_sha256: null }).ok);
  assert.ok(!C.validateSeal({ ...seal, outcome: "done" }).ok);
  assert.ok(C.validateSeal({ ...seal, last_i: null, last_committed_record_sha256: null, outcome: "stopped" }).ok);
});

test("manifest closure: counts, entry order, kind rules and coverage claims are all checked", () => {
  const { map, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  const claim = finishRun(writer).manifest;
  const read = readMap(map).manifest;
  assert.ok(C.validateManifest(claim).ok && C.validateManifest(read).ok);
  const ok = (m: C.ManifestV1, mutate: (x: C.ManifestV1) => void): boolean => { const x = clone(m); mutate(x); return C.validateManifest(x).ok; };
  assert.ok(!ok(read, (x) => { x.counts.present += 0; x.counts.verified = 119; }));
  assert.ok(!ok(read, (x) => { x.entries.reverse(); }));
  assert.ok(!ok(read, (x) => { x.entries.pop(); }));
  assert.ok(!ok(read, (x) => { x.budget.physical_storage_bytes = 5 as unknown as null; }));
  assert.ok(!ok(read, (x) => { x.budget.accounted_bytes += 1; }));
  assert.ok(!ok(read, (x) => { (x as { acceptance: string }).acceptance = "PASS"; }));
  assert.ok(!ok(read, (x) => { (x as { cpu_interval_boundary: string }).cpu_interval_boundary = "baseline_120"; }));
  assert.ok(!ok(read, (x) => { x.counts.qualified_adjacent_cpu = 120; }));
  assert.ok(!ok(read, (x) => { x.entries[5]!.read_status = "missing"; x.counts.verified = 119; x.counts.missing = 1; x.counts.present = 119; }), "complete_structural with a missing entry");
  assert.ok(!ok(read, (x) => { x.flags_mask = C.orMask(x.flags_mask, F.MISSING); }), "complete_structural with a blocking flag");
  assert.ok(!ok(claim, (x) => { x.coverage = "complete_structural"; }), "a writer claim is never complete");
  assert.ok(!ok(claim, (x) => { x.entries[2]!.read_status = "verified"; }), "a writer cannot verify itself");
  assert.ok(!ok(claim, (x) => { x.read_started_utc = READ_START; }));
  assert.ok(!ok(read, (x) => { x.read_started_utc = READ_END; x.read_finished_utc = READ_START; }));
  assert.ok(!ok(read, (x) => { (x as unknown as Record<string, unknown>).extra = 1; }));
  assert.ok(!ok(read, (x) => { x.entries[0]!.write_status = "committed " as "committed"; }));
});

test("coverage_classification: only baseline, jitter and rate-qualification gaps are tolerated", () => {
  const base = C.buildUnarmedManifest(DESC, sha, { kind: "independent_readback", principal: DESC.reader_alias, created_utc: CREATED });
  const verified: C.ManifestEntryV1[] = range(120).map((i) => ({ i, record_sha256: "a".repeat(64), write_status: "committed", read_status: "verified", flags_mask: 0 }));
  const seal = "b".repeat(64);
  assert.equal(base.coverage, "unknown");
  assert.equal(C.classifyCoverage("independent_readback", F.CPU_BASELINE_ONLY, verified, seal, DESC.T_utc), "complete_structural");
  const tolerated = [F.CPU_BASELINE_ONLY, F.JITTER_UNACCEPTED, F.CPU_HZ_UNKNOWN, F.CPU_WIDTH_UNKNOWN, F.VCPU_UNKNOWN];
  assert.equal(C.STRUCTURAL_TOLERATED_FLAGS, C.orMask(...tolerated));
  for (const name of C.FLAG_NAMES) {
    const out = C.classifyCoverage("independent_readback", F[name], verified, seal, DESC.T_utc);
    const expected = tolerated.includes(F[name]) ? "complete_structural" : name === "AUTHORITY_MISSING" || name === "OPEN_OR_UNSEALED" ? "unknown" : "incomplete";
    assert.equal(out, expected, name);
  }
  assert.equal(C.classifyCoverage("independent_readback", C.ALL_FLAGS, verified, seal, DESC.T_utc), "unknown");
  assert.equal(C.classifyCoverage("independent_readback", 0, verified, null, DESC.T_utc), "unknown", "no seal");
  assert.equal(C.classifyCoverage("independent_readback", 0, verified, seal, null), "unknown", "T unset");
  assert.equal(C.classifyCoverage("independent_readback", 0, verified.map((e, i) => (i === 3 ? { ...e, read_status: "not_attempted" as const } : e)), seal, DESC.T_utc), "incomplete");
  assert.equal(C.classifyCoverage("writer_claim", 0, verified, seal, DESC.T_utc), "unknown");
  assert.equal(C.classifyCoverage("writer_claim", 0, verified.map((e, i) => (i === 3 ? { ...e, write_status: "failed" as const } : e)), seal, DESC.T_utc), "incomplete");
});

test("armed only with verified T and every authority reference; nothing unarmed touches storage", () => {
  const cases: [string, C.RunDescriptorV1][] = [];
  const unarmed = clone(DESC); unarmed.T_utc = null; unarmed.T_evidence_sha256 = null;
  cases.push(["T unset", unarmed]);
  const noEvidence = clone(DESC); noEvidence.T_evidence_sha256 = null;
  cases.push(["T without its receipt", noEvidence]);
  const noTime = clone(DESC); noTime.T_utc = null;
  cases.push(["receipt without T", noTime]);
  const keys = Object.keys(DESC.authority) as (keyof C.RunDescriptorV1["authority"])[];
  assert.equal(keys.length, 9);
  for (const key of keys) {
    const staging = clone(DESC);
    staging.mode = "staging";
    for (const k of keys) staging.authority[k] = "a".repeat(64);
    staging.authority[key] = null;
    cases.push([`staging without ${key}`, staging]);
  }
  for (const [name, desc] of cases) {
    const { map, store, writer, opened } = newRun({ desc, reconcile: true });
    assert.deepEqual(opened, { opened: false, flags: F.AUTHORITY_MISSING }, name);
    assert.equal(writer.record(baseSlot(0)).status, "refused");
    const fin = finishRun(writer, false);
    assert.equal(store.log.length, 0, `${name}: no source or storage call`);
    assert.equal(map.size, 0);
    assert.equal(fin.outcome, "unknown");
    assert.equal(fin.manifest.coverage, "unknown");
    assert.ok(has(fin.manifest.flags_mask, "AUTHORITY_MISSING", "OPEN_OR_UNSEALED"));
    assert.deepEqual(fin.manifest.entries.map((e) => [e.write_status, e.read_status, e.flags_mask]), Array(120).fill(["not_attempted", "not_attempted", F.AUTHORITY_MISSING]));
    assertManifestShape(fin.manifest);
    const direct = C.buildUnarmedManifest(desc, sha, { kind: "independent_readback", principal: desc.reader_alias, created_utc: CREATED });
    assert.equal(direct.coverage, "unknown");
    assert.ok(direct.entries.every((e) => e.record_sha256 === null && e.read_status === "not_attempted"));
    assertManifestShape(direct);
  }
  const staging = clone(DESC);
  staging.mode = "staging";
  for (const k of keys) staging.authority[k] = "a".repeat(64);
  assert.equal(newRun({ desc: staging }).opened.opened, true, "mode alone does not block a fully authorized synthetic run");
  assert.equal(C.authorityFlags(DESC), 0, "an offline run needs no live reference");
});

test("events_bounds_preserved: one fixture, 900 s, 20 expectations, 60 receipts and a full 60 is truncated", () => {
  const ok = (window_seconds: number, expectations: number, receipts: number) => C.checkFixtureBounds({ window_seconds, expectations, receipts });
  assert.deepEqual(ok(900, 20, 59), { accepted: true, truncated: false, complete: true, flags: 0 });
  assert.deepEqual(ok(900, 20, 60), { accepted: true, truncated: true, complete: false, flags: F.TRUNCATED });
  assert.deepEqual(ok(900, 20, 61), { accepted: false, truncated: false, complete: false, flags: C.orMask(F.ROW_BUDGET, F.TRUNCATED) });
  note(ok(900, 20, 60).flags, ok(900, 20, 61).flags);
  assert.deepEqual(ok(901, 20, 0), { accepted: false, truncated: false, complete: false, flags: F.INVALID_SCHEMA });
  assert.deepEqual(ok(900, 21, 0), { accepted: false, truncated: false, complete: false, flags: F.ROW_BUDGET });
  assert.deepEqual(ok(0, 0, 0), { accepted: true, truncated: false, complete: true, flags: 0 });
  assert.equal(ok(-1, 0, 0).accepted, false);
  assert.equal(ok(900, 1.5, 0).accepted, false);
  assert.equal(ok(NaN, 0, 0).accepted, false);
});

test("source_failure_each_index: a tombstone is persisted and the run stops, for all 120 slots", () => {
  const variants: [string, (s: C.SlotInput) => void][] = [
    ["no observation", (s) => { s.observation = null; }],
    ["source unavailable", (s) => {
      s.observation = { ...obsOf(s), source: "unavailable", rss_bytes: null, cpu_user_ticks: null, cpu_system_ticks: null, cpu_anchor_mono_ns: null };
    }],
    ["incomplete observation", (s) => { obsOf(s).rss_bytes = null; }],
  ];
  for (let k = 0; k < 120; k++) {
    for (const [name, mutate] of variants) {
      const { map, store, writer } = newRun();
      feed(writer, range(k));
      const slot = baseSlot(k);
      mutate(slot);
      const r = writer.record(slot);
      const expected = C.orMask(F.SOURCE_UNAVAILABLE, k === 0 ? F.CPU_BASELINE_ONLY : 0);
      assert.deepEqual([r.status, r.flags, r.stopped], ["committed", expected, true], `${name} @${k}`);
      const rec = recordAt(map, k);
      assert.equal(rec.flags_mask, expected);
      assert.equal(rec.observation === null, name === "no observation");
      if (name !== "incomplete observation") assert.ok(Object.values(rec.cpu_interval).every((v) => v === null));
      const calls = store.log.length;
      assert.equal(writer.record(baseSlot(Math.min(k + 1, 119))).status, "refused");
      assert.equal(store.log.length, calls, "no retry source and no further I/O");
      const fin = finishRun(writer);
      assert.equal(fin.outcome, "stopped");
      assert.deepEqual(incidentsOf(map).map((e) => [e.ordinal, e.kind]), [[0, "source_failure"], [31, "stop"]]);
      const m = readMap(map).manifest;
      assert.equal(m.entries[k]!.read_status, "verified", `${name} @${k}: a tombstone is a persisted record`);
      assert.equal(m.counts.qualified_rss, k, "a slot without a resource value is never a qualified one");
      assert.equal(m.coverage, "incomplete");
      note(m.flags_mask);
    }
  }
});

test("write path edges: unresolved descriptor, taken keys, and a reconciliation that finds different bytes", () => {
  // Descriptor write outcomes.
  const reject = newRun({ script: (op, key) => (op === "create" && key.endsWith(":descriptor") ? { outcome: REJECTED } : undefined) });
  assert.deepEqual(reject.opened, { opened: false, flags: F.FAILED_WRITE });
  const timeout = newRun({ script: (op, key) => (op === "create" && key.endsWith(":descriptor") ? { outcome: TIMEOUT } : undefined) });
  assert.deepEqual(timeout.opened, { opened: false, flags: F.WRITE_OUTCOME_UNKNOWN });
  assert.equal(timeout.store.count("create"), 2);
  assert.equal(timeout.writer.record(baseSlot(0)).status, "refused");
  // A record key somebody else already created: no overwrite, concurrency stop.
  const taken = newRun();
  feed(taken.writer, range(3));
  assert.equal(taken.map.create(rkey(3), "foreign").kind, "created");
  const r = taken.writer.record(baseSlot(3));
  assert.deepEqual([r.status, r.flags, r.stopped], ["rejected", F.CONCURRENT_WRITER, true]);
  assert.equal(recordText(taken.map, 3), "foreign");
  assert.deepEqual(incidentsOf(taken.map).map((e) => e.kind), ["concurrency"]);
  note(r.flags);
  // Ambiguous write whose reconciliation read returns bytes that are not ours.
  const clash = newRun({ reconcile: true, script: (op, key) => (op === "create" && key === rkey(5) ? { outcome: TIMEOUT } : undefined) });
  feed(clash.writer, range(5));
  assert.equal(clash.map.create(rkey(5), "foreign").kind, "created");
  const c = clash.writer.record(baseSlot(5));
  assert.deepEqual([c.status, c.flags, c.stopped], ["rejected", C.orMask(F.CONCURRENT_WRITER, F.DUPLICATE), true]);
  assert.equal(recordText(clash.map, 5), "foreign", "never overwritten");
  assert.equal(clash.store.count("read", rkey(5)), 1);
  assert.equal(clash.writer.record(baseSlot(6)).status, "refused");
});

test("reader run-level checks: descriptor, seal, claim and incident faults are reported, not repaired", () => {
  const { map, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  finishRun(writer);
  const snapshot = map.snapshot();
  const pairs = JSON.parse(snapshot) as [string, string][];
  const drop = (suffix: string): string => JSON.stringify(pairs.filter(([k]) => !k.endsWith(suffix)));
  const edit = (suffix: string, fn: (text: string) => string): string => JSON.stringify(pairs.map(([k, v]) => (k.endsWith(suffix) ? [k, fn(v)] : [k, v])));
  const retag = (suffix: string, fn: (o: Record<string, unknown>) => void): string => edit(suffix, (v) => { const o = JSON.parse(v) as Record<string, unknown>; fn(o); return C.canonicalize(o); });

  const noManifest = readFrom(drop(":manifest:writer"));
  assert.ok(has(noManifest.manifest.flags_mask, "OPEN_OR_UNSEALED"));
  assert.equal(noManifest.manifest.coverage, "unknown", "verified records but an open run");
  assert.ok(noManifest.manifest.entries.every((e) => e.write_status === "unknown"));
  assert.ok(noManifest.manifest.writer_seal_sha256 !== null);

  const noSeal = readFrom(drop(":writer-seal"));
  assert.ok(has(noSeal.manifest.flags_mask, "OPEN_OR_UNSEALED", "READ_MISMATCH"), "the claim names a seal that is gone");
  assert.equal(noSeal.manifest.writer_seal_sha256, null);
  assert.equal(noSeal.manifest.coverage, "incomplete");

  const corruptSeal = readFrom(edit(":writer-seal", (v) => `${v} `));
  assert.ok(has(corruptSeal.manifest.flags_mask, "READ_MISMATCH", "OPEN_OR_UNSEALED"));

  const wrongLast = readFrom(retag(":writer-seal", (o) => { o.last_committed_record_sha256 = "0".repeat(64); }));
  assert.ok(has(wrongLast.manifest.flags_mask, "READ_MISMATCH"));
  assert.equal(wrongLast.manifest.coverage, "incomplete");
  // Re-pin the claim to the doctored seal so only the seal's own statement is wrong.
  const sealText = pairs.find(([k]) => k.endsWith(":writer-seal"))![1];
  const doctored = C.canonicalize({ ...(JSON.parse(sealText) as object), last_committed_record_sha256: "0".repeat(64) });
  const repinned = JSON.stringify(pairs.map(([k, v]) => {
    if (k.endsWith(":writer-seal")) return [k, doctored];
    if (!k.endsWith(":manifest:writer")) return [k, v];
    return [k, C.canonicalize({ ...(JSON.parse(v) as object), writer_seal_sha256: sha(doctored) })];
  }));
  const sealLies = readFrom(repinned);
  assert.equal(sealLies.manifest.entries[119]!.read_status, "verified", "the record itself is fine");
  assert.ok(has(sealLies.manifest.flags_mask, "READ_MISMATCH"), "the seal names a different last record than the one persisted");
  assert.equal(sealLies.manifest.coverage, "incomplete");

  const claimLies = readFrom(retag(":manifest:writer", (o) => { ((o.entries as Record<string, unknown>[])[7]!).record_sha256 = "0".repeat(64); }));
  assert.equal(claimLies.manifest.entries[7]!.read_status, "mismatch", "a record that contradicts the writer's claim is not verified");
  assert.ok(has(claimLies.manifest.entries[7]!.flags_mask, "READ_MISMATCH"));

  const badIncident = readFrom(JSON.stringify([...pairs, [ekey(3), "{}"]]));
  assert.ok(has(badIncident.manifest.flags_mask, "READ_MISMATCH"));
  assert.equal(badIncident.manifest.coverage, "incomplete");
  const incidentError = readFrom(snapshot, { script: (op, key) => (op === "read" && key === ekey(9) ? { outcome: { kind: "error", elapsed_ms: 1 } } : undefined) });
  assert.ok(has(incidentError.manifest.flags_mask, "FAILED_READ"));
  assert.equal(incidentError.manifest.coverage, "incomplete");

  // Descriptor faults: nothing can be bound, so there is no manifest.
  const descriptorKey = C.keyFor(DESC.run_alias, "descriptor");
  const failures: [string, string, Script | undefined, number][] = [
    ["non-canonical descriptor", edit(":descriptor", (v) => ` ${v}`), undefined, C.orMask(F.READ_MISMATCH, F.INVALID_SCHEMA)],
    ["another run's descriptor", retag(":descriptor", (o) => { o.run_alias = "f".repeat(32); }), undefined, F.READ_MISMATCH],
    ["unreadable descriptor", snapshot, (op, key) => (op === "read" && key === descriptorKey ? { outcome: { kind: "error", elapsed_ms: 1 } } : undefined), F.FAILED_READ],
    ["denied descriptor read", snapshot, (op, key) => (op === "read" && key === descriptorKey ? { outcome: DENIED } : undefined), F.FAILED_READ],
  ];
  for (const [name, snap, script, flags] of failures) {
    const rb = readFrom(snap, script ? { script } : {});
    assert.equal(rb.result.manifest, null, name);
    assert.equal(rb.result.flags, flags, name);
    assert.equal(rb.store.count(), 1, `${name}: nothing more is read`);
    note(rb.result.flags);
  }

  // A persisted descriptor that is not armed yields the unarmed manifest after the single descriptor read.
  const unarmed = clone(DESC);
  unarmed.T_utc = null;
  unarmed.T_evidence_sha256 = null;
  const seeded = new C.PersistedMap();
  const enc = C.encodeDescriptor(unarmed);
  assert.ok(enc.ok);
  seeded.create(descriptorKey, enc.value);
  const rb = readMap(seeded, { desc: unarmed });
  assert.equal(rb.result.reads, 1);
  assert.equal(rb.manifest.coverage, "unknown");
  assert.ok(rb.manifest.entries.every((e) => e.read_status === "not_attempted" && e.flags_mask === F.AUTHORITY_MISSING));
  assertManifestShape(rb.manifest);
});

test("a lost incident or a doctored record cannot turn a flagged run into a clean one", () => {
  // The writer saw a duplicate retry, but the incident write was rejected on both attempts.
  const retryRun = (script?: Script) => {
    const run = newRun(script ? { script } : {});
    feed(run.writer, range(6));
    assert.equal(run.writer.record(baseSlot(5)).status, "duplicate_retry");
    feed(run.writer, range(120).slice(6));
    finishRun(run.writer);
    return run;
  };
  const lost = retryRun((op, key) => (op === "create" && key === ekey(0) ? { outcome: REJECTED } : undefined));
  assert.equal(lost.writer.localOnlyIncidents().length, 1);
  assert.equal(incidentsOf(lost.map).length, 0);
  const a = readMap(lost.map).manifest;
  assert.equal(a.coverage, "incomplete", "the claim's duplicate fact is carried even though no incident survived");
  assert.ok(has(a.entries[5]!.flags_mask, "DUPLICATE") && has(a.flags_mask, "DUPLICATE"));
  // The incident was persisted and then removed from the store.
  const kept = retryRun();
  assert.equal(readMap(kept.map).manifest.coverage, "incomplete");
  const pairs = pairsOf(kept.map);
  const b = readFrom(JSON.stringify(pairs.filter(([k]) => k !== ekey(0)))).manifest;
  assert.equal(b.coverage, "incomplete");
  assert.ok(has(b.flags_mask, "READ_MISMATCH", "DUPLICATE"), "the claim counted an incident that is gone");
  assert.equal(b.counts.incident_keys_present, 0);
  note(b.flags_mask);

  // A record that omits the flag its own pair requires is not verified, even with the claim re-pinned to it.
  const reset = newRun({ reconcile: true });
  feed(reset.writer, range(120), (s, i) => { if (i === 10) obsOf(s).cpu_user_ticks = (BigInt(FIX.series[9]!.utime) - 1n).toString(); return s; });
  finishRun(reset.writer);
  const clean = readMap(reset.map).manifest;
  assert.ok(has(clean.entries[10]!.flags_mask, "COUNTER_RESET_OR_WRAP"));
  const all = pairsOf(reset.map);
  const original = all.find(([k]) => k === rkey(10))![1];
  const doctoredRecord = JSON.parse(original) as { flags_mask: number };
  doctoredRecord.flags_mask = (doctoredRecord.flags_mask & ~F.COUNTER_RESET_OR_WRAP) >>> 0;
  const doctored = C.canonicalize(doctoredRecord);
  const repinned = JSON.stringify(all.map(([k, v]) => {
    if (k === rkey(10)) return [k, doctored];
    if (!k.endsWith(":manifest:writer")) return [k, v];
    const claim = JSON.parse(v) as { entries: { i: number; record_sha256: string }[] };
    claim.entries[10]!.record_sha256 = sha(doctored);
    return [k, C.canonicalize(claim)];
  }));
  const forged = readFrom(repinned).manifest;
  assert.equal(forged.entries[10]!.read_status, "mismatch");
  assert.equal(forged.coverage, "incomplete");
});

test("writer lifecycle: stop before open latches, finish seals once, the seal names the highest slot", () => {
  const map = new C.PersistedMap();
  const store = new SpyStore(map);
  const writer = new C.ProcessHistoryWriter({ store, sha256: sha, reconcile_read_authorized: false });
  writer.stop();
  assert.deepEqual(writer.open(DESC), { opened: false, flags: F.COLLECTOR_STOP });
  assert.notEqual(writer.record(baseSlot(0)).status, "committed");
  assert.equal(store.log.length, 0, "a stopped writer never reaches storage");

  // finish() is idempotent: the second call performs no I/O and changes nothing.
  const run = newRun();
  feed(run.writer, range(120));
  const first = finishRun(run.writer);
  const calls = run.store.log.length;
  assert.equal(finishRun(run.writer), first);
  assert.equal(run.store.log.length, calls);
  assert.ok(first.seal_persisted && first.manifest_persisted);
  assert.equal(run.writer.localOnlyIncidents().length, 0);

  // Slot 119 arrives before slot 118: the run is still 120 slots, and the seal must be valid and name slot 119.
  const swap = newRun();
  feed(swap.writer, [...range(118), 119, 118]);
  const fin = finishRun(swap.writer);
  assert.equal(fin.outcome, "finished");
  assert.equal(fin.seal?.last_i, 119);
  assert.equal(fin.seal?.last_committed_record_sha256, sha(recordText(swap.map, 119)));
  const rb = readMap(swap.map);
  assert.equal(rb.manifest.counts.verified, 120);
  assert.ok(has(rb.manifest.flags_mask, "OUT_OF_ORDER"));

  // stop() after the last slot has nothing left to stop.
  const done = newRun();
  feed(done.writer, range(120));
  done.writer.stop();
  const doneFin = finishRun(done.writer);
  assert.equal(doneFin.outcome, "finished");
  assert.equal(readMap(done.map).manifest.coverage, "complete_structural");
});

test("finish reports the real cause of a refused open and a denial that lands during sealing", () => {
  const rejected = newRun({ script: (op, key) => (op === "create" && key.endsWith(":descriptor") ? { outcome: REJECTED } : undefined) });
  const r = finishRun(rejected.writer, false);
  assert.ok(has(r.manifest.flags_mask, "FAILED_WRITE") && !has(r.manifest.flags_mask, "AUTHORITY_MISSING"));
  assert.ok(r.manifest.entries.every((e) => e.flags_mask === F.FAILED_WRITE));
  const unknown = newRun({ script: (op, key) => (op === "create" && key.endsWith(":descriptor") ? { outcome: TIMEOUT } : undefined) });
  assert.ok(has(finishRun(unknown.writer, false).manifest.flags_mask, "WRITE_OUTCOME_UNKNOWN"));
  const first = newRun();
  const calls = first.store.log.length;
  const second = new C.ProcessHistoryWriter({ store: first.store, sha256: sha, reconcile_read_authorized: true });
  assert.equal(second.open(DESC).flags, F.CONCURRENT_WRITER);
  const s2 = finishRun(second, false);
  assert.ok(has(s2.manifest.flags_mask, "CONCURRENT_WRITER") && !has(s2.manifest.flags_mask, "AUTHORITY_MISSING"));
  assert.equal(first.store.log.length, calls + 1, "only the refused descriptor create reached the store");

  // Denial on the terminal incident: the outcome is unknown, not stopped.
  const t = newRun({ script: (op, key) => (op === "create" && key === ekey(31) ? { outcome: DENIED } : undefined) });
  feed(t.writer, range(10));
  t.writer.stop();
  const tf = finishRun(t.writer, false);
  assert.deepEqual([tf.outcome, tf.seal?.outcome, tf.seal_persisted], ["unknown", "unknown", false]);
  // Denial on the seal of an otherwise finished run.
  const s = newRun({ script: (op, key) => (op === "create" && key.endsWith(":writer-seal") ? { outcome: DENIED } : undefined) });
  feed(s.writer, range(120));
  const sf = finishRun(s.writer, false);
  assert.deepEqual([sf.outcome, sf.seal?.outcome, sf.seal_persisted, sf.manifest_persisted], ["unknown", "unknown", false, false]);
  assert.equal(s.store.count("create", C.keyFor(DESC.run_alias, "manifest:writer")), 0);
});

test("rates stand alone: an overflowing allocated rate keeps the one-core rate, and bad allocations are refused", () => {
  assert.deepEqual(C.cpuRates(1_000_000_000n, 1_000_000_000n, 1, 1), { one_core: 100_000_000_000_000_000n, allocated: null, overflow: true });
  assert.deepEqual(C.cpuRates(1_000_000_000n, 1_000_000_000n, 1, 1000), { one_core: 100_000_000_000_000_000n, allocated: 100_000_000_000_000_000n, overflow: false });
  for (const bad of [0, -5, 1.5, NaN]) assert.equal(C.cpuRates(1n, 1n, 100, bad), null, String(bad));
  const p1 = baseSlot(0);
  const p2 = clone(p1);
  p1.identity.cpu_ticks_per_second = 1; p2.identity.cpu_ticks_per_second = 1;
  p1.identity.allocated_vcpu_milli = 1; p2.identity.allocated_vcpu_milli = 1;
  obsOf(p1).cpu_anchor_mono_ns = "5000000000"; obsOf(p1).cpu_user_ticks = "0"; obsOf(p1).cpu_system_ticks = "0";
  obsOf(p2).cpu_anchor_mono_ns = "6000000000"; obsOf(p2).cpu_user_ticks = "1000000000"; obsOf(p2).cpu_system_ticks = "0";
  const out = C.computeCpuInterval(1, p2, p1);
  assert.equal(out.flags, F.INVALID_SCHEMA);
  assert.equal(out.interval.one_core_micropercent, "100000000000000000");
  assert.equal(out.interval.allocated_micropercent, null);
  assert.equal(out.interval.delta_cpu_ticks, "1000000000");
});

test("parser and heuristics edges: container depth, lone surrogates, process-id style keys", () => {
  const nest = (n: number, open: string, close: string) => open.repeat(n) + close.repeat(n);
  assert.ok(C.parseStrictJson(nest(6, "[", "]")).ok, "six levels of arrays");
  assert.ok(!C.parseStrictJson(nest(7, "[", "]")).ok, "seven levels of arrays");
  assert.ok(C.parseStrictJson("{\"a\":".repeat(5) + "1" + "}".repeat(5)).ok);
  assert.ok(!C.parseStrictJson("{\"a\":".repeat(7) + "1" + "}".repeat(7)).ok);
  assert.ok(!C.parseStrictJson("[".repeat(6) + "[]" + "]".repeat(6)).ok);
  assert.equal(C.utf8Length("\ud800\ud800"), 6, "two lone high surrogates are two replacement characters");
  assert.equal(C.utf8Length("😀"), 4, "a real pair is one 4-byte character");
  assert.equal(C.utf8Length("\udc00"), 3);
  assert.equal(C.utf8Length("\ud800"), 3);
  assert.equal(C.utf8Length("aé€"), 6);
  const record = recordFor(4);
  for (const key of ["process_id", "processId", "api_key", "apiKey", "userId", "free_text", "freeText", "note_text"]) {
    const out = C.validateRecord({ ...record, [key]: "x" });
    assert.ok(!out.ok && has(out.flags, "INVALID_SCHEMA", "PRIVACY_REJECTED"), key);
  }
  // The unarmed pair is allowed: a time without its receipt (or the reverse) is a descriptor that simply cannot arm.
  const half = clone(DESC);
  half.T_evidence_sha256 = null;
  assert.ok(C.validateDescriptor(half).ok);
  assert.equal(C.authorityFlags(half), F.AUTHORITY_MISSING);
});

test("wire numbering: each failure name keeps its bit, as written in the doc", () => {
  const wire: [string, number][] = [
    ["MISSING", 0], ["DUPLICATE", 1], ["OUT_OF_ORDER", 2], ["CPU_BASELINE_ONLY", 3], ["COUNTER_RESET_OR_WRAP", 4], ["IDENTITY_UNKNOWN", 5],
    ["IDENTITY_CHANGE", 6], ["CLOCK_UNKNOWN", 7], ["CLOCK_RESET", 8], ["CLOCK_DISAGREEMENT", 9], ["OUTSIDE_SLOT", 10],
    ["JITTER_UNACCEPTED", 11], ["SOURCE_UNAVAILABLE", 12], ["FAILED_WRITE", 13], ["WRITE_OUTCOME_UNKNOWN", 14], ["FAILED_READ", 15],
    ["READ_MISMATCH", 16], ["TRUNCATED", 17], ["COLLECTOR_STOP", 18], ["BYTE_BUDGET", 19], ["ROW_BUDGET", 20], ["EVENT_BUDGET", 21],
    ["CONCURRENT_WRITER", 22], ["INVALID_SCHEMA", 23], ["PRIVACY_REJECTED", 24], ["CPU_HZ_UNKNOWN", 25], ["CPU_WIDTH_UNKNOWN", 26],
    ["VCPU_UNKNOWN", 27], ["NONADJACENT_CPU", 28], ["DEADLINE", 29], ["AUTHORITY_MISSING", 30], ["OPEN_OR_UNSEALED", 31],
  ];
  assert.equal(wire.length, 32);
  for (const [name, bit] of wire) assert.equal((C.F as Record<string, number>)[name], 2 ** bit, name);
  // The doc table is the normative assignment: it must say the same thing.
  const doc = readFileSync(new URL("../../docs/process-history-contract.md", import.meta.url), "utf8");
  const rows = [...doc.matchAll(/^\| (\d+) \| `([A-Z_]+)` \|/gm)].map((m) => [m[2]!, Number(m[1])] as [string, number]);
  assert.deepEqual(rows, wire, "docs/process-history-contract.md lists the same names on the same bits");
});

test("write edges: delta-sum overflow, a denied or late reconciliation, late acknowledgements and refused reader outputs", () => {
  // The two deltas fit a u64 on their own but their sum does not: a reset, not a wrap-around.
  const a = baseSlot(0);
  const b = clone(a);
  obsOf(a).cpu_user_ticks = "0"; obsOf(a).cpu_system_ticks = "0"; obsOf(a).cpu_anchor_mono_ns = "5000000000";
  obsOf(b).cpu_user_ticks = (1n << 63n).toString(); obsOf(b).cpu_system_ticks = (1n << 63n).toString(); obsOf(b).cpu_anchor_mono_ns = "6000000000";
  const sum = C.computeCpuInterval(1, b, a);
  assert.equal(sum.flags, F.COUNTER_RESET_OR_WRAP);
  assert.ok(Object.values(sum.interval).every((v) => v === null));

  // Permission denial on the reconciliation read is absorbing too.
  const denied = newRun({ reconcile: true, script: (op, key) => {
    if (key !== rkey(5)) return undefined;
    return op === "create" ? { outcome: TIMEOUT } : { outcome: DENIED };
  } });
  feed(denied.writer, range(5));
  const d = denied.writer.record(baseSlot(5));
  assert.deepEqual([d.status, d.flags], ["denied", mask("FAILED_WRITE", "AUTHORITY_MISSING")]);
  const calls = denied.store.log.length;
  assert.equal(denied.writer.record(baseSlot(6)).status, "refused");
  finishRun(denied.writer, false);
  assert.equal(denied.store.log.length, calls, "nothing is initiated after the denied read");
  assert.equal(denied.store.count("create", rkey(5)), 2);
  assert.equal(denied.store.count("read", rkey(5)), 1);

  // An acknowledgement later than 2 s does not count as one; the same-key read settles it.
  const late = newRun({ reconcile: true, script: (op, key, nth) => (op === "create" && key === rkey(7) && nth === 1 ? { outcome: { kind: "created", elapsed_ms: 2500 }, land: true } : undefined) });
  const results = feed(late.writer, range(10));
  assert.deepEqual(results[7]!.status, "committed");
  assert.equal(late.store.count("create", rkey(7)), 2);
  assert.equal(late.store.count("read", rkey(7)), 1);
  assert.deepEqual(incidentsOf(late.map).map((e) => e.kind), ["duplicate_retry"]);
  // A reconciliation answer later than 2 s is unusable: the outcome stays unknown.
  const slowRead = newRun({ reconcile: true, script: (op, key, nth) => {
    if (key !== rkey(7)) return undefined;
    if (op === "create") return nth === 1 ? { outcome: TIMEOUT, land: true } : undefined;
    return { outcome: { kind: "value", value: "irrelevant", elapsed_ms: 2500 } };
  } });
  assert.equal(feed(slowRead.writer, range(8))[7]!.status, "unknown");

  // A denied reader manifest write blocks the seal write.
  const run = newRun({ reconcile: true });
  feed(run.writer, range(120));
  finishRun(run.writer);
  const rb = readMap(run.map);
  const spy = new SpyStore(run.map, (op, key) => (op === "create" && key.endsWith(":manifest:reader") ? { outcome: DENIED } : undefined));
  assert.deepEqual(C.persistReaderOutputs(spy, rb.result, DESC.run_alias), { manifest: "denied", seal: "skipped" });
  assert.equal(spy.count("create", C.keyFor(DESC.run_alias, "reader-seal")), 0);
});

test("reader bindings: alias, schedule, flag, sequence and claim mismatches are all refused", () => {
  const { map, writer } = newRun({ reconcile: true });
  feed(writer, range(120));
  finishRun(writer);
  const pairs = pairsOf(map);
  const claimKey = C.keyFor(DESC.run_alias, "manifest:writer");
  // Edit one record and re-pin the writer's claim (and seal) to the edited bytes, so only the reader's own
  // record-level checks can object. Without the re-pin the claim's hash would catch every edit first.
  const repinned = (k: number, fn: (o: Record<string, unknown>) => void): string => {
    const edited = C.canonicalize((() => { const o = JSON.parse(pairs.find(([key]) => key === rkey(k))![1]) as Record<string, unknown>; fn(o); return o; })());
    return JSON.stringify(pairs.map(([key, v]) => {
      if (key === rkey(k)) return [key, edited];
      if (key === claimKey) {
        const claim = JSON.parse(v) as C.ManifestV1;
        claim.entries[k]!.record_sha256 = sha(edited);
        return [key, C.canonicalize(claim)];
      }
      return [key, v];
    }));
  };
  for (const k of [0, 59, 119]) {
    const cases: [string, string][] = [
      ["fixture alias", repinned(k, (o) => { o.fixture_alias = "e".repeat(32); })],
      ["writer alias", repinned(k, (o) => { o.writer_alias = "e".repeat(32); })],
      ["scheduled_utc", repinned(k, (o) => { o.scheduled_utc = C.formatUtc(T_MS + (7200 + 60 * k) * 1000 + 1000); })],
      ["run alias", repinned(k, (o) => { o.run_alias = "e".repeat(32); })],
      ["slot index", repinned(k, (o) => { o.i = (k + 1) % 120; o.scheduled_offset_seconds = 7200 + 60 * ((k + 1) % 120); })],
      ["forged flag", repinned(k, (o) => { o.flags_mask = C.orMask(o.flags_mask as number, F.PRIVACY_REJECTED, F.BYTE_BUDGET); })],
    ];
    if (k > 0) cases.push(["reused write_seq", repinned(k, (o) => { o.write_seq = 0; })]);
    for (const [name, snapshot] of cases) {
      const e = readFrom(snapshot).manifest.entries[k]!;
      assert.equal(e.read_status, "mismatch", `${name} @${k}`);
      assert.ok(has(e.flags_mask, "READ_MISMATCH"));
    }
  }

  // Claim faults: the claim is cross-checked, never trusted over the records.
  const patchClaim = (fn: (claim: C.ManifestV1) => void): string => JSON.stringify(pairs.map(([k, v]) => {
    if (k !== claimKey) return [k, v];
    const claim = JSON.parse(v) as C.ManifestV1;
    fn(claim);
    claim.counts = { ...claim.counts, ...C.deriveCounts(claim.kind, claim.entries) };
    claim.counts.qualified_rss = Math.min(claim.counts.qualified_rss, claim.counts.present);
    claim.counts.qualified_adjacent_cpu = Math.min(claim.counts.qualified_adjacent_cpu, claim.counts.present);
    claim.flags_mask = C.orMask(claim.flags_mask, ...claim.entries.map((e) => e.flags_mask));
    return [k, C.canonicalize(claim)];
  }));
  const failedClaim = readFrom(patchClaim((c) => { c.entries[33] = { ...c.entries[33]!, write_status: "failed", record_sha256: null, flags_mask: F.FAILED_WRITE }; })).manifest;
  assert.equal(failedClaim.entries[33]!.read_status, "mismatch", "the writer says it failed, yet the record is there");
  assert.ok(has(failedClaim.flags_mask, "FAILED_WRITE"), "and the claimed fact is carried");
  for (const [name, edit] of [
    ["descriptor hash", (c: C.ManifestV1) => { c.descriptor_sha256 = "0".repeat(64); }],
    ["contract hash", (c: C.ManifestV1) => { c.contract_sha256 = "0".repeat(64); }],
    ["fixture alias", (c: C.ManifestV1) => { c.fixture_alias = "0".repeat(32); }],
    ["run alias", (c: C.ManifestV1) => { c.run_alias = "0".repeat(32); }],
    ["principal", (c: C.ManifestV1) => { c.principal_alias = DESC.reader_alias; }],
    ["seal hash", (c: C.ManifestV1) => { c.writer_seal_sha256 = "0".repeat(64); }],
  ] as [string, (c: C.ManifestV1) => void][]) {
    const m = readFrom(patchClaim(edit)).manifest;
    assert.ok(has(m.flags_mask, "READ_MISMATCH", "OPEN_OR_UNSEALED"), `claim with another ${name}`);
    assert.ok(m.entries.every((e) => e.write_status === "unknown"), `claim with another ${name} is ignored`);
    assert.equal(m.coverage, "incomplete");
  }

  // Manifest closure: qualified counts cannot exceed what is present.
  const gap = readFrom(JSON.stringify(pairs.filter(([k]) => k !== rkey(5)))).manifest;
  assert.equal(gap.counts.present, 119);
  const overstated = clone(gap);
  overstated.counts.qualified_rss = 120;
  assert.ok(!C.validateManifest(overstated).ok, "qualified_rss above present");
  const overstatedCpu = clone(gap);
  overstatedCpu.counts.qualified_adjacent_cpu = 119;
  assert.ok(C.validateManifest(overstatedCpu).ok, "119 adjacent deltas fit 119 present records");
  overstatedCpu.entries[7]!.read_status = "missing";
  overstatedCpu.counts = { ...overstatedCpu.counts, ...C.deriveCounts("independent_readback", overstatedCpu.entries) };
  overstatedCpu.coverage = "incomplete";
  assert.ok(!C.validateManifest(overstatedCpu).ok, "qualified_adjacent_cpu above present");
});

/** Edit record k in a snapshot and re-pin the writer's claim to the edited bytes (only reader-side checks can object). */
function repinRecord(pairs: [string, string][], k: number, fn: (o: Record<string, unknown>) => void): string {
  const original = JSON.parse(pairs.find(([key]) => key === rkey(k))![1]) as Record<string, unknown>;
  fn(original);
  const edited = C.canonicalize(original);
  return JSON.stringify(pairs.map(([key, v]) => {
    if (key === rkey(k)) return [key, edited];
    if (!key.endsWith(":manifest:writer")) return [key, v];
    const claim = JSON.parse(v) as C.ManifestV1;
    claim.entries[k]!.record_sha256 = sha(edited);
    return [key, C.canonicalize(claim)];
  }));
}

test("pair flags: an honest late predecessor is not a mismatch, an omitted pair flag is, and claimed facts survive", () => {
  // Slot 11 arrives before slot 10 and the pair also holds a counter reset. The writer held no predecessor, so it
  // could neither pair them nor see the reset: the reader must not demand a flag the writer could not know.
  const order = [...range(10), 11, 10, ...range(120).slice(12)];
  const lateRun = newRun();
  feed(lateRun.writer, order, (s, i) => { if (i === 11) obsOf(s).cpu_user_ticks = (BigInt(FIX.series[10]!.utime) - 1n).toString(); return s; });
  finishRun(lateRun.writer);
  const late = readMap(lateRun.map).manifest;
  assert.equal(late.entries[11]!.read_status, "verified");
  assert.equal(late.entries[11]!.flags_mask, F.NONADJACENT_CPU);
  assert.equal(late.entries[10]!.flags_mask, F.OUT_OF_ORDER);
  assert.equal(late.entries[12]!.read_status, "verified", "the next slot keeps its own pair");
  assert.equal(late.counts.verified, 120);
  // Forging NONADJACENT_CPU onto a record whose predecessor arrived first hides nothing.
  const clean = newRun();
  feed(clean.writer, range(120));
  finishRun(clean.writer);
  const cleanPairs = pairsOf(clean.map);
  const forgedNull = readFrom(repinRecord(cleanPairs, 20, (o) => {
    o.cpu_interval = { previous_i: null, start_anchor_mono_ns: null, end_anchor_mono_ns: null, delta_mono_ns: null, delta_cpu_ticks: null, one_core_micropercent: null, allocated_micropercent: null };
    o.flags_mask = F.NONADJACENT_CPU;
  })).manifest;
  assert.equal(forgedNull.entries[20]!.read_status, "mismatch");

  // A record may not drop the flag its own pair (or identity) requires, one flag at a time.
  const scenarios: [string, number, number, (s: C.SlotInput, i: number) => void][] = [
    ["COUNTER_RESET_OR_WRAP", F.COUNTER_RESET_OR_WRAP, 10, (s, i) => { if (i === 10) obsOf(s).cpu_user_ticks = (BigInt(FIX.series[9]!.utime) - 1n).toString(); }],
    ["CLOCK_RESET", F.CLOCK_RESET, 10, (s, i) => { if (i === 10) { const a = BigInt(FIX.series[9]!.anchor_ns) - 1n; obsOf(s).cpu_anchor_mono_ns = a.toString(); obsOf(s).mono_start_ns = (a - 1n).toString(); obsOf(s).mono_end_ns = (a + 1n).toString(); } }],
    ["INVALID_SCHEMA", F.INVALID_SCHEMA, 9, (s, i) => { if (i === 9) obsOf(s).cpu_user_ticks = U64_MAX.toString(); if (i === 10) obsOf(s).cpu_user_ticks = "0"; }],
    ["IDENTITY_CHANGE", F.IDENTITY_CHANGE, 10, (s, i) => { if (i === 10) s.identity.worker_version_id = "00000000-0000-4000-8000-00000000b2a9"; }],
  ];
  for (const [name, flag, k, mutate] of scenarios) {
    const run = newRun();
    feed(run.writer, range(120), (s, i) => { mutate(s, i); return s; });
    finishRun(run.writer);
    const pairs = pairsOf(run.map);
    assert.ok(has(recordAt(run.map, k).flags_mask, name as C.FlagName), `${name} is on record ${k}`);
    const honest = readFrom(JSON.stringify(pairs)).manifest.entries[k]!;
    assert.equal(honest.read_status, "verified", name);
    const dropped = readFrom(repinRecord(pairs, k, (o) => { o.flags_mask = ((o.flags_mask as number) & ~flag) >>> 0; })).manifest.entries[k]!;
    assert.equal(dropped.read_status, "mismatch", `${name} dropped from record ${k}`);
  }

  // Facts only the writer saw are carried into the reader's entry, even when the same entry also mismatches.
  const facts: C.FlagName[] = ["DUPLICATE", "FAILED_WRITE", "WRITE_OUTCOME_UNKNOWN", "COLLECTOR_STOP", "CONCURRENT_WRITER", "EVENT_BUDGET", "BYTE_BUDGET", "ROW_BUDGET", "PRIVACY_REJECTED"];
  const claimKey = C.keyFor(DESC.run_alias, "manifest:writer");
  const withClaimFlag = (flag: number): string => JSON.stringify(cleanPairs.map(([k, v]) => {
    if (k !== claimKey) return [k, v];
    const claim = JSON.parse(v) as C.ManifestV1;
    claim.entries[40]!.flags_mask = flag;
    claim.flags_mask = C.orMask(claim.flags_mask, flag);
    return [k, C.canonicalize(claim)];
  }));
  for (const name of facts) {
    const e = readFrom(withClaimFlag(F[name])).manifest.entries[40]!;
    assert.equal(e.read_status, "verified");
    assert.ok(has(e.flags_mask, name), `${name} claimed by the writer reaches the reader's entry`);
  }
  const notFact = readFrom(withClaimFlag(F.AUTHORITY_MISSING)).manifest;
  assert.ok(!has(notFact.entries[40]!.flags_mask, "AUTHORITY_MISSING"), "only the writer-fact set is carried");
  assert.equal(notFact.coverage, "complete_structural");
  // Duplicate fact plus a pair mismatch on the same entry: both survive.
  const dupReset = newRun();
  feed(dupReset.writer, range(120), (s, i) => { if (i === 10) obsOf(s).cpu_user_ticks = (BigInt(FIX.series[9]!.utime) - 1n).toString(); return s; });
  assert.equal(dupReset.writer.record((() => { const s = baseSlot(10); obsOf(s).cpu_user_ticks = (BigInt(FIX.series[9]!.utime) - 1n).toString(); return s; })()).status, "duplicate_retry");
  finishRun(dupReset.writer);
  const both = readFrom(repinRecord(pairsOf(dupReset.map), 10, (o) => { o.flags_mask = ((o.flags_mask as number) & ~F.COUNTER_RESET_OR_WRAP) >>> 0; })).manifest.entries[10]!;
  assert.equal(both.read_status, "mismatch");
  assert.ok(has(both.flags_mask, "READ_MISMATCH", "DUPLICATE"));

  // The incident-count check only applies when every incident key was actually read.
  const dup = newRun();
  feed(dup.writer, range(6));
  dup.writer.record(baseSlot(5));
  feed(dup.writer, range(120).slice(6));
  finishRun(dup.writer);
  const unread = readMap(dup.map, { script: (op, key) => (op === "read" && key === ekey(0) ? { outcome: { kind: "error", elapsed_ms: 1 } } : undefined) }).manifest;
  assert.ok(has(unread.flags_mask, "FAILED_READ", "DUPLICATE"));
  assert.ok(!has(unread.flags_mask, "READ_MISMATCH"), "an unread incident key is not a count mismatch");
});

test("terminal slot and stop edges: the terminal incident names the highest slot, and a stopped writer says so", () => {
  const run = newRun();
  feed(run.writer, [0, 1, 2, 3, 4, 6, 5]);
  run.writer.stop();
  const fin = finishRun(run.writer);
  assert.equal(fin.seal?.last_i, 6);
  const terminal = incidentsOf(run.map).find((e) => e.ordinal === 31)!;
  assert.deepEqual([terminal.kind, terminal.i], ["stop", 6]);
  assert.equal(fin.seal?.last_committed_record_sha256, sha(recordText(run.map, 6)));

  const early = new C.ProcessHistoryWriter({ store: new C.PersistedMap(), sha256: sha, reconcile_read_authorized: false });
  early.stop();
  assert.deepEqual(early.record(baseSlot(0)), { status: "refused", i: null, flags: F.COLLECTOR_STOP, record_sha256: null, stopped: true });

  // An overflowing one-core rate takes the allocated rate with it (the schema ties them); the exact deltas stay.
  const a = baseSlot(0);
  const b = clone(a);
  a.identity.cpu_ticks_per_second = 1; b.identity.cpu_ticks_per_second = 1;
  a.identity.allocated_vcpu_milli = 1_000_000; b.identity.allocated_vcpu_milli = 1_000_000;
  obsOf(a).cpu_anchor_mono_ns = "5000000000"; obsOf(a).cpu_user_ticks = "0"; obsOf(a).cpu_system_ticks = "0";
  obsOf(b).cpu_anchor_mono_ns = "5000000001"; obsOf(b).cpu_user_ticks = "1000"; obsOf(b).cpu_system_ticks = "0";
  assert.deepEqual(C.cpuRates(1000n, 1n, 1, 1_000_000), { one_core: null, allocated: 100_000_000_000_000_000n, overflow: true });
  const out = C.computeCpuInterval(1, b, a);
  assert.equal(out.flags, F.INVALID_SCHEMA);
  assert.deepEqual([out.interval.delta_cpu_ticks, out.interval.one_core_micropercent, out.interval.allocated_micropercent], ["1000", null, null]);
});

test("no_live_side_effects: the contract imports nothing, reaches nothing, and no runtime entrypoint imports it", () => {
  const read = (rel: string): string => readFileSync(new URL(rel, import.meta.url), "utf8");
  const stripComments = (text: string): string => text.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:"'`])\/\/.*$/gm, "$1");
  const source = stripComments(read("../src/process-history-contract.ts"));
  const testText = stripComments(read("./process-history-contract.test.ts"));
  // Words are assembled so this file does not trip its own scan.
  const w = (...parts: string[]): string => parts.join("");
  const forbiddenEverywhere = [
    w("fe", "tch"), w("Web", "Socket"), w("XMLHttp", "Request"), w("Date", "\\."), w("new ", "Date"), w("perfor", "mance"), w("set", "Timeout"),
    w("set", "Interval"), w("queue", "Microtask"), w("process", "\\."), w("global", "This"), w("Durable", "Object"), w("ctx", "\\.storage"),
    w("cloud", "flare:"), w("node:", "net"), w("node:", "http"), w("node:", "child_process"), w("node:", "dns"), w("node:", "os"),
    w("node:", "tls"), w("node:", "worker_threads"), w("req", "uire\\("), w("import", "\\("), w("De", "no"), w("D", "1Database"),
    w("KV", "Namespace"), w("R", "2Bucket"), w("\\bpg", "\\b"), w("Atom", "ics"), w("Math\\.", "random"),
  ];
  for (const pattern of forbiddenEverywhere) {
    assert.ok(!new RegExp(pattern).test(source), `contract source must not use /${pattern}/`);
    assert.ok(!new RegExp(pattern).test(testText), `test source must not use /${pattern}/`);
  }
  for (const shape of [/\bimport\b/, /\bfrom\s*["']/, /\brequire\b/, /\bnode:/, /readFileSync|readdirSync|writeFileSync|createReadStream/]) {
    assert.ok(!shape.test(source), `the contract has no import, re-export, node: reference or file access (${shape})`);
  }
  const imports = [...testText.matchAll(/^import\s.*from\s+"([^"]+)"/gm)].map((m) => m[1]!).sort();
  assert.deepEqual(imports, ["../src/process-history-contract.ts", "node:assert/strict", "node:crypto", "node:fs", "node:test"]);
  // No Worker source imports the module: it stays unreachable from every runtime entrypoint.
  const walk = (rel: string): string[] => readdirSync(new URL(rel, import.meta.url), { withFileTypes: true })
    .flatMap((entry) => (entry.isDirectory() ? walk(`${rel}${entry.name}/`) : [`${rel}${entry.name}`]));
  const others = walk("../src/").filter((file) => !file.endsWith("/process-history-contract.ts"));
  assert.ok(others.length >= 8, "the scan really covers the Worker sources");
  for (const file of others) assert.ok(!read(file).includes("process-history-contract"), `${file} must not import the contract`);
  assert.ok(!read("../wrangler.toml").includes("process-history"), "no binding, route, timer or config mentions it");
  assert.ok(!read("../package.json").includes("process-history"));
  // Public hygiene for every file in this change.
  for (const rel of ["../src/process-history-contract.ts", "./process-history-contract.test.ts", "../../docs/process-history-contract.md"]) {
    const text = read(rel);
    assert.ok(!INTERNAL_ID.test(text), `${rel} carries no internal tracker id`);
    if (!rel.endsWith(".test.ts")) assert.ok(!ANY_URL.test(text), `${rel} carries no URL`);
  }
});

test("flag ledger: every one of the 32 failure bits was produced by a real scenario above", (t) => {
  const total = (readFileSync(new URL(import.meta.url), "utf8").match(/^test\(/gm) ?? []).length;
  if (seen.executed < total - 1) { t.skip("the ledger needs every scenario above to have run in this process"); return; }
  const missing = C.FLAG_NAMES.filter((name) => !C.hasFlag(seen.mask, F[name]));
  assert.deepEqual(missing, [], "bits never exercised by a scenario");
  assert.equal(seen.mask, C.ALL_FLAGS);
});
