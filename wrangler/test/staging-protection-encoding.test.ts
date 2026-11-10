/** Deny-by-default JSON and stored-ZIP decoding for the migration protection Worker. */
import { test } from "node:test";
import assert from "node:assert/strict";
import { EncodingError, crc32, parseJson, storedArtifact } from "../src/staging-protection/encoding.ts";

const enc = (s: string) => new TextEncoder().encode(s);
const refused = (fn: () => unknown) =>
  assert.throws(fn, (e: unknown) => e instanceof EncodingError && e.message === "encoding refused");

// ------------------------------------------------------------------ JSON

test("parseJson keeps every integer token lossless as bigint", () => {
  assert.equal(parseJson(enc("9007199254740993")), 9007199254740993n);
  assert.equal(parseJson(enc("-9007199254740993")), -9007199254740993n);
  assert.equal(parseJson(enc("18446744073709551615")), 18446744073709551615n); // 20 digits
  assert.equal(parseJson(enc("-99999999999999999999")), -99999999999999999999n);
  assert.equal(parseJson(enc("0")), 0n);
  assert.deepEqual(parseJson(enc(' [1, true, null, "x", false] ')), [1n, true, null, "x", false]);
});

test("parseJson builds null-prototype objects and keeps proto-like keys as data", () => {
  const v = parseJson(enc('{"a":{"b":[2]},"__proto__":{"x":1}}')) as Record<string, unknown>;
  assert.equal(Object.getPrototypeOf(v), null);
  assert.equal(Object.getPrototypeOf(v["a"]), null);
  assert.ok(Object.hasOwn(v, "__proto__"));
  assert.equal(({} as Record<string, unknown>)["x"], undefined);
});

test("parseJson decodes escapes and surrogate pairs", () => {
  assert.equal(parseJson(enc('"a\\n\\u0041\\ud83d\\ude00\\/"')), "a\nA\u{1f600}/");
  assert.equal(parseJson(enc('"é"')), "é");
});

test("parseJson refuses duplicate keys including escaped aliases", () => {
  refused(() => parseJson(enc('{"a":1,"a":2}')));
  refused(() => parseJson(enc('{"a":1,"\\u0061":2}')));
  refused(() => parseJson(enc('{"\\u0061":1,"a":2}')));
  refused(() => parseJson(enc('{"a\\/b":1,"a/b":2}')));
  refused(() => parseJson(enc('{"x":{"k":1,"\\u006b":2}}')));
  // Same key in different objects is fine.
  assert.ok(parseJson(enc('[{"a":1},{"a":2}]')));
});

test("parseJson refuses non-integer numbers", () => {
  for (const t of ["1.0", "1.5", "1e3", "1E3", "1e-2", "-0", "01", "-", "+1", ".5", "1.", "0x10", "NaN", "Infinity", "[1.0]", '{"a":2e1}']) {
    refused(() => parseJson(enc(t)));
  }
});

test("parseJson refuses integers over 20 digits without a giant conversion", () => {
  refused(() => parseJson(enc("1".repeat(21))));
  refused(() => parseJson(enc("-" + "1".repeat(21))));
  refused(() => parseJson(enc("[" + "9".repeat(1000) + "]")));
  refused(() => parseJson(enc("1".repeat(1024 * 1024 - 1))));
});

test("parseJson enforces the nesting limit of 32", () => {
  const nest = (n: number) => "[".repeat(n) + "]".repeat(n);
  assert.ok(parseJson(enc(nest(32))));
  refused(() => parseJson(enc(nest(33))));
  refused(() => parseJson(enc('{"a":'.repeat(33) + "1" + "}".repeat(33))));
  assert.ok(parseJson(enc('{"a":'.repeat(32) + "1" + "}".repeat(32))));
});

test("parseJson refuses invalid UTF-8, BOM and lone surrogates", () => {
  refused(() => parseJson(new Uint8Array([0x22, 0xff, 0x22])));
  refused(() => parseJson(new Uint8Array([0x22, 0xc0, 0xaf, 0x22])));
  refused(() => parseJson(new Uint8Array([0x22, 0xed, 0xa0, 0x80, 0x22]))); // encoded surrogate
  refused(() => parseJson(new Uint8Array([0xef, 0xbb, 0xbf, 0x31])));
  refused(() => parseJson(enc('"\\ud800"')));
  refused(() => parseJson(enc('"\\udc00"')));
  refused(() => parseJson(enc('"\\ud800\\u0041"')));
});

test("parseJson refuses malformed JSON and oversize input", () => {
  for (const t of ["", " ", "{", "[1,]", "[1 2]", '{"a":1,}', '{"a" 1}', "{a:1}", "'x'", '"x', '"\\x"', '"\\u12"', '"a\nb"', "1 2", "tru", "nul", "[]]", '{"a":1}x']) {
    refused(() => parseJson(enc(t)));
  }
  const max = 1024 * 1024;
  const pad = (n: number) => enc('"' + "a".repeat(n - 2) + '"');
  assert.equal(parseJson(pad(max)), "a".repeat(max - 2));
  refused(() => parseJson(pad(max + 1)));
});

test("refusals carry no input data", () => {
  try {
    parseJson(enc('{"secret-token":1,"secret-token":2}'));
    assert.fail("expected refusal");
  } catch (e) {
    assert.ok(e instanceof EncodingError);
    assert.doesNotMatch(String((e as Error).message), /secret/);
  }
});

// ------------------------------------------------------------------- ZIP

const NAME = "artifact.json";
const CONTENT = enc('{"ok":1}');

interface Opts {
  name?: string;
  content?: Uint8Array;
  flags?: number;
  localFlags?: number;
  descriptor?: "none" | "sig" | "nosig";
  version?: number;
  method?: number;
  localMethod?: number;
  crc?: number;
  localCrc?: number;
  localSizes?: [number, number];
  centralSizes?: [number, number];
  localName?: string;
  extra?: Uint8Array;
  centralExtra?: Uint8Array;
  comment?: Uint8Array;
  centralComment?: Uint8Array;
  offset?: number;
  eocdEntries?: number;
  eocdTotal?: number;
  disk?: number;
  cdSizeDelta?: number;
  cdOffsetDelta?: number;
  prefix?: Uint8Array;
  suffix?: Uint8Array;
  gapBytes?: Uint8Array;
}

function cat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}
function le(bytes: number, n: number): Uint8Array {
  const out = new Uint8Array(bytes);
  for (let i = 0; i < bytes; i += 1) out[i] = Math.floor(n / 2 ** (8 * i)) & 0xff;
  return out;
}
const u16 = (n: number) => le(2, n);
const u32 = (n: number) => le(4, n);

function zip(o: Opts = {}): Uint8Array {
  const name = enc(o.name ?? NAME);
  const content = o.content ?? CONTENT;
  const crc = o.crc ?? crc32(content);
  const flags = o.flags ?? 0;
  const lflags = o.localFlags ?? flags;
  const desc = o.descriptor ?? "none";
  const [lc, ls] = o.localSizes ?? (desc === "none" ? [content.length, content.length] : [0, 0]);
  const [cc, cs] = o.centralSizes ?? [content.length, content.length];
  const lcrc = o.localCrc ?? (desc === "none" ? crc : 0);
  const extra = o.extra ?? new Uint8Array(0);
  const local = cat(
    u32(0x04034b50), u16(o.version ?? 20), u16(lflags), u16(o.localMethod ?? o.method ?? 0), u16(0), u16(0x21),
    u32(lcrc), u32(lc), u32(ls), u16(enc(o.localName ?? o.name ?? NAME).length), u16(extra.length),
    enc(o.localName ?? o.name ?? NAME), extra,
  );
  const dsc =
    desc === "sig" ? cat(u32(0x08074b50), u32(crc), u32(cc), u32(cs)) : desc === "nosig" ? cat(u32(crc), u32(cc), u32(cs)) : new Uint8Array(0);
  const body = cat(local, content, dsc, o.gapBytes ?? new Uint8Array(0));
  const cExtra = o.centralExtra ?? new Uint8Array(0);
  const cComment = o.centralComment ?? new Uint8Array(0);
  const central = cat(
    u32(0x02014b50), u16(20), u16(o.version ?? 20), u16(flags), u16(o.method ?? 0), u16(0), u16(0x21),
    u32(crc), u32(cc), u32(cs), u16(name.length), u16(cExtra.length), u16(cComment.length),
    u16(o.disk ?? 0), u16(0), u32(0), u32(o.offset ?? 0), name, cExtra, cComment,
  );
  const comment = o.comment ?? new Uint8Array(0);
  const eocd = cat(
    u32(0x06054b50), u16(0), u16(0), u16(o.eocdEntries ?? 1), u16(o.eocdTotal ?? 1),
    u32(central.length + (o.cdSizeDelta ?? 0)), u32(body.length + (o.cdOffsetDelta ?? 0)), u16(comment.length), comment,
  );
  return cat(o.prefix ?? new Uint8Array(0), body, central, eocd, o.suffix ?? new Uint8Array(0));
}

test("storedArtifact returns exact content for plain and descriptor layouts", () => {
  assert.deepEqual(storedArtifact(zip(), NAME), CONTENT);
  assert.deepEqual(storedArtifact(zip({ flags: 8, descriptor: "sig" }), NAME), CONTENT);
  assert.deepEqual(storedArtifact(zip({ flags: 8, descriptor: "nosig" }), NAME), CONTENT);
  // Descriptor-flagged local header may already carry the real values.
  assert.deepEqual(
    storedArtifact(zip({ flags: 8, descriptor: "sig", localCrc: crc32(CONTENT), localSizes: [CONTENT.length, CONTENT.length] }), NAME),
    CONTENT,
  );
  assert.deepEqual(storedArtifact(zip({ content: new Uint8Array(0) }), NAME), new Uint8Array(0));
});

test("storedArtifact returns a copy, not a view of the input", () => {
  const raw = zip();
  const out = storedArtifact(raw, NAME);
  raw.fill(0);
  assert.deepEqual(out, CONTENT);
});

test("storedArtifact crc32 matches the known check value", () => {
  assert.equal(crc32(enc("123456789")), 0xcbf43926);
});

test("storedArtifact refuses name mismatches and unsafe names", () => {
  refused(() => storedArtifact(zip(), "other.json"));
  refused(() => storedArtifact(zip(), "Artifact.json"));
  refused(() => storedArtifact(zip({ localName: "artifacts.json" }), NAME));
  for (const bad of ["a/b", "../x", "..", ".", "", "a\\b", "/abs", "dir/", "naéme"]) {
    refused(() => storedArtifact(zip(), bad));
    if (bad !== "" && /^[\x20-\x7e]+$/.test(bad)) refused(() => storedArtifact(zip({ name: bad }), bad));
  }
  refused(() => storedArtifact(zip({ name: "../artifact.json" }), "../artifact.json"));
  refused(() => storedArtifact(zip({ name: "x/" , content: new Uint8Array(0) }), "x/"));
});

test("storedArtifact refuses unsupported methods, flags and versions", () => {
  refused(() => storedArtifact(zip({ method: 8, localMethod: 8 }), NAME));
  refused(() => storedArtifact(zip({ method: 8 }), NAME));
  refused(() => storedArtifact(zip({ localMethod: 8 }), NAME));
  refused(() => storedArtifact(zip({ flags: 1 }), NAME)); // encrypted
  refused(() => storedArtifact(zip({ flags: 0x800 }), NAME));
  refused(() => storedArtifact(zip({ flags: 0x40 }), NAME));
  refused(() => storedArtifact(zip({ flags: 0, localFlags: 8 }), NAME));
  refused(() => storedArtifact(zip({ flags: 8, descriptor: "sig", localFlags: 0 }), NAME));
  refused(() => storedArtifact(zip({ version: 45 }), NAME)); // ZIP64 version
});

test("storedArtifact refuses ZIP64 markers", () => {
  refused(() => storedArtifact(zip({ centralSizes: [0xffffffff, 0xffffffff] }), NAME));
  refused(() => storedArtifact(zip({ cdOffsetDelta: 0xffffffff }), NAME));
  refused(() => storedArtifact(zip({ extra: cat(u16(1), u16(0)), centralExtra: cat(u16(1), u16(0)) }), NAME));
  // ZIP64 EOCD locator/record bytes before the EOCD are trailing data.
  refused(() => storedArtifact(zip({ gapBytes: enc("PK\u0006\u0007") }), NAME));
});

test("storedArtifact refuses multiple entries and entry-count disagreement", () => {
  const one = zip();
  refused(() => storedArtifact(cat(one, one), NAME));
  refused(() => storedArtifact(zip({ eocdEntries: 2 }), NAME));
  refused(() => storedArtifact(zip({ eocdTotal: 2 }), NAME));
  refused(() => storedArtifact(zip({ eocdEntries: 0, eocdTotal: 0 }), NAME));
  refused(() => storedArtifact(zip({ disk: 1 }), NAME));
});

test("storedArtifact refuses comments, extra fields, prefix and trailing bytes", () => {
  refused(() => storedArtifact(zip({ comment: enc("hi") }), NAME));
  refused(() => storedArtifact(zip({ centralComment: enc("hi") }), NAME));
  refused(() => storedArtifact(zip({ extra: enc("ab") }), NAME));
  refused(() => storedArtifact(zip({ centralExtra: enc("ab") }), NAME));
  refused(() => storedArtifact(zip({ prefix: enc("x") }), NAME));
  refused(() => storedArtifact(zip({ suffix: enc("x") }), NAME));
  refused(() => storedArtifact(zip({ suffix: new Uint8Array(1) }), NAME));
  refused(() => storedArtifact(zip({ gapBytes: enc("x") }), NAME));
});

test("storedArtifact refuses crc, size and offset disagreements", () => {
  refused(() => storedArtifact(zip({ crc: crc32(CONTENT) ^ 1, localCrc: crc32(CONTENT) ^ 1 }), NAME));
  refused(() => storedArtifact(zip({ localCrc: crc32(CONTENT) ^ 1 }), NAME));
  refused(() => storedArtifact(zip({ centralSizes: [CONTENT.length + 1, CONTENT.length + 1] }), NAME));
  refused(() => storedArtifact(zip({ centralSizes: [CONTENT.length, CONTENT.length - 1] }), NAME));
  refused(() => storedArtifact(zip({ localSizes: [CONTENT.length, CONTENT.length + 1] }), NAME));
  refused(() => storedArtifact(zip({ offset: 1 }), NAME));
  refused(() => storedArtifact(zip({ cdOffsetDelta: 1 }), NAME));
  refused(() => storedArtifact(zip({ cdOffsetDelta: -1 }), NAME));
  refused(() => storedArtifact(zip({ cdSizeDelta: 1 }), NAME));
  refused(() => storedArtifact(zip({ cdSizeDelta: -1 }), NAME));
  // Tampered content bytes with otherwise valid headers.
  const raw = zip();
  raw[30 + NAME.length] = (raw[30 + NAME.length] as number) ^ 0xff;
  refused(() => storedArtifact(raw, NAME));
});

test("storedArtifact refuses malformed descriptors", () => {
  const good = crc32(CONTENT);
  const n = CONTENT.length;
  const base = { flags: 8, descriptor: "sig" as const };
  refused(() => storedArtifact(zip({ ...base, crc: good, centralSizes: [n, n] , localCrc: 1 }), NAME));
  // Missing descriptor although flagged.
  refused(() => storedArtifact(zip({ flags: 8, descriptor: "none", localSizes: [0, 0], localCrc: 0 }), NAME));
  // Wrong gap width.
  refused(() => storedArtifact(zip({ ...base, gapBytes: enc("xxxx") }), NAME));
  refused(() => storedArtifact(zip({ flags: 8, descriptor: "nosig", gapBytes: enc("x") }), NAME));
  // Descriptor values that disagree with the central directory.
  const sig = zip(base);
  const dOff = 30 + NAME.length + n;
  const badCrc = sig.slice();
  badCrc[dOff + 4] = (badCrc[dOff + 4] as number) ^ 1;
  refused(() => storedArtifact(badCrc, NAME));
  const badSize = sig.slice();
  badSize[dOff + 12] = (badSize[dOff + 12] as number) ^ 1;
  refused(() => storedArtifact(badSize, NAME));
  const noSig = zip({ flags: 8, descriptor: "nosig" });
  const badSize2 = noSig.slice();
  badSize2[dOff + 8] = (badSize2[dOff + 8] as number) ^ 1;
  refused(() => storedArtifact(badSize2, NAME));
  // Sixteen bytes without the signature.
  const wrongSig = sig.slice();
  wrongSig[dOff] = 0;
  refused(() => storedArtifact(wrongSig, NAME));
});

test("storedArtifact refuses truncation, garbage and size bounds", () => {
  const raw = zip();
  for (let cut = 0; cut < raw.length; cut += 1) refused(() => storedArtifact(raw.slice(0, cut), NAME));
  refused(() => storedArtifact(new Uint8Array(200), NAME));
  refused(() => storedArtifact(enc("not a zip at all, just plain text bytes, long enough to clear minimum size...."), NAME));

  const max = 1024 * 1024;
  const big = new Uint8Array(max).fill(0x61);
  assert.equal(storedArtifact(zip({ content: big }), NAME).length, max);
  refused(() => storedArtifact(zip({ content: new Uint8Array(max + 1).fill(0x61) }), NAME));
  refused(() => storedArtifact(new Uint8Array(max + 4096 + 1), NAME));
});
