/** Deny-by-default decoding for the independent migration protection Worker.
 *
 * `parseJson` is a strict JSON parser: every integer token becomes a `bigint`
 * (lossless), and duplicate keys (including escaped aliases), invalid UTF-8,
 * non-finite numbers, negative zero, lone surrogates, nesting deeper than 32,
 * integer parts over 20 digits, input over 1 MiB and malformed text are refused.
 * Fraction/exponent tokens remain `number`, never evidence integers (`bigint`).
 *
 * `storedArtifact` extracts the content of an archive that holds exactly one
 * stored (method 0) entry with an exact name, and refuses everything else.
 *
 * Every refusal throws the same `EncodingError` with a fixed message, so
 * neither the message nor the type leaks any input bytes.
 */

export type Json = null | boolean | string | bigint | number | Json[] | { [key: string]: Json };

export const ENCODING_ERROR_MESSAGE = "encoding refused";

export class EncodingError extends Error {
  constructor() {
    super(ENCODING_ERROR_MESSAGE);
    this.name = "EncodingError";
  }
}

const refuse = (): never => {
  throw new EncodingError();
};

export const MAX_JSON_BYTES = 1024 * 1024;
export const MAX_JSON_DEPTH = 32;
export const MAX_JSON_INTEGER_DIGITS = 20;
export const MAX_ARTIFACT_CONTENT_BYTES = 1024 * 1024;
export const MAX_ARTIFACT_RAW_BYTES = 1024 * 1024 + 4096;

// ---------------------------------------------------------------- JSON

class Reader {
  private pos = 0;
  private readonly s: string;
  constructor(s: string) {
    this.s = s;
  }

  document(): Json {
    this.ws();
    const v = this.value(0);
    this.ws();
    if (this.pos !== this.s.length) refuse();
    return v;
  }

  private ws(): void {
    for (;;) {
      const c = this.s.charCodeAt(this.pos);
      if (c === 0x20 || c === 0x09 || c === 0x0a || c === 0x0d) this.pos += 1;
      else return;
    }
  }

  private literal(word: string): void {
    if (!this.s.startsWith(word, this.pos)) refuse();
    this.pos += word.length;
  }

  private value(depth: number): Json {
    if (this.pos >= this.s.length) return refuse();
    const c = this.s[this.pos];
    if (c === "{") return this.object(depth + 1);
    if (c === "[") return this.array(depth + 1);
    if (c === '"') return this.string();
    if (c === "t") return this.literal("true"), true;
    if (c === "f") return this.literal("false"), false;
    if (c === "n") return this.literal("null"), null;
    return this.number();
  }

  private number(): bigint | number {
    const start = this.pos;
    if (this.s[this.pos] === "-") this.pos += 1;
    const first = this.s.charCodeAt(this.pos);
    if (!(first >= 0x30 && first <= 0x39)) return refuse();
    if (first === 0x30) {
      this.pos += 1;
    } else {
      while (this.isDigit(this.pos)) this.pos += 1;
    }
    if (this.isDigit(this.pos)) refuse(); // leading zero
    const integerEnd = this.pos;
    if (integerEnd - start - (this.s[start] === "-" ? 1 : 0) > MAX_JSON_INTEGER_DIGITS) refuse();
    if (this.s[this.pos] === ".") {
      this.pos += 1;
      if (!this.isDigit(this.pos)) return refuse();
      while (this.isDigit(this.pos)) this.pos += 1;
    }
    if (this.s[this.pos] === "e" || this.s[this.pos] === "E") {
      this.pos += 1;
      if (this.s[this.pos] === "+" || this.s[this.pos] === "-") this.pos += 1;
      if (!this.isDigit(this.pos)) return refuse();
      while (this.isDigit(this.pos)) this.pos += 1;
    }
    const token = this.s.slice(start, this.pos);
    if (this.pos !== integerEnd) {
      // Audit metrics use floats. Never promote even integral floats to bigint:
      // every evidence integer validator requires the original integer syntax.
      const value = Number(token);
      if (!Number.isFinite(value) || Object.is(value, -0)) refuse();
      return value;
    }
    if (token === "-0") refuse();
    return BigInt(token);
  }

  private isDigit(i: number): boolean {
    const c = this.s.charCodeAt(i);
    return c >= 0x30 && c <= 0x39;
  }

  private hex4(): number {
    const h = this.s.slice(this.pos, this.pos + 4);
    if (!/^[0-9a-fA-F]{4}$/.test(h)) return refuse();
    this.pos += 4;
    return parseInt(h, 16);
  }

  private string(): string {
    this.pos += 1; // opening quote
    let out = "";
    for (;;) {
      if (this.pos >= this.s.length) return refuse();
      const code = this.s.charCodeAt(this.pos);
      const ch = this.s[this.pos] as string;
      this.pos += 1;
      if (ch === '"') return out;
      if (code < 0x20) return refuse();
      if (ch !== "\\") {
        out += ch;
        continue;
      }
      const e = this.s[this.pos];
      this.pos += 1;
      switch (e) {
        case '"': out += '"'; break;
        case "\\": out += "\\"; break;
        case "/": out += "/"; break;
        case "b": out += "\b"; break;
        case "f": out += "\f"; break;
        case "n": out += "\n"; break;
        case "r": out += "\r"; break;
        case "t": out += "\t"; break;
        case "u": {
          const u = this.hex4();
          if (u >= 0xd800 && u <= 0xdbff) {
            if (this.s[this.pos] !== "\\" || this.s[this.pos + 1] !== "u") return refuse();
            this.pos += 2;
            const lo = this.hex4();
            if (!(lo >= 0xdc00 && lo <= 0xdfff)) return refuse();
            out += String.fromCharCode(u, lo);
          } else if (u >= 0xdc00 && u <= 0xdfff) {
            return refuse();
          } else {
            out += String.fromCharCode(u);
          }
          break;
        }
        default:
          return refuse();
      }
    }
  }

  private array(depth: number): Json[] {
    if (depth > MAX_JSON_DEPTH) refuse();
    this.pos += 1;
    const out: Json[] = [];
    this.ws();
    if (this.s[this.pos] === "]") {
      this.pos += 1;
      return out;
    }
    for (;;) {
      this.ws();
      out.push(this.value(depth));
      this.ws();
      const c = this.s[this.pos];
      this.pos += 1;
      if (c === "]") return out;
      if (c !== ",") return refuse();
    }
  }

  private object(depth: number): { [key: string]: Json } {
    if (depth > MAX_JSON_DEPTH) refuse();
    this.pos += 1;
    const out: { [key: string]: Json } = Object.create(null);
    this.ws();
    if (this.s[this.pos] === "}") {
      this.pos += 1;
      return out;
    }
    for (;;) {
      this.ws();
      if (this.s[this.pos] !== '"') return refuse();
      const key = this.string();
      if (Object.hasOwn(out, key)) return refuse();
      this.ws();
      if (this.s[this.pos] !== ":") return refuse();
      this.pos += 1;
      this.ws();
      out[key] = this.value(depth);
      this.ws();
      const c = this.s[this.pos];
      this.pos += 1;
      if (c === "}") return out;
      if (c !== ",") return refuse();
    }
  }
}

export function parseJson(raw: Uint8Array): Json {
  if (!(raw instanceof Uint8Array) || raw.length > MAX_JSON_BYTES) return refuse();
  let text: string;
  try {
    // ignoreBOM keeps a leading BOM in the text so the grammar refuses it.
    text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(raw);
  } catch {
    return refuse();
  }
  return new Reader(text).document();
}

// ----------------------------------------------------------------- ZIP

const SIG_LOCAL = 0x04034b50;
const SIG_CENTRAL = 0x02014b50;
const SIG_EOCD = 0x06054b50;
const SIG_DESCRIPTOR = 0x08074b50;
const LOCAL_FIXED = 30;
const CENTRAL_FIXED = 46;
const EOCD_FIXED = 22;
const FLAG_DESCRIPTOR = 0x0008;

const CRC_TABLE: Uint32Array = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

export function crc32(data: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < data.length; i += 1) {
    c = (CRC_TABLE[(c ^ (data[i] as number)) & 0xff] as number) ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

function validName(name: string): Uint8Array {
  if (typeof name !== "string" || name.length === 0 || name.length > 255) return refuse();
  if (name === "." || name === "..") return refuse();
  // Printable ASCII only, no separators of either style.
  if (!/^[\x20-\x7e]+$/.test(name) || name.includes("/") || name.includes("\\")) return refuse();
  return new TextEncoder().encode(name);
}

export function storedArtifact(raw: Uint8Array, filename: string): Uint8Array {
  const name = validName(filename);
  if (!(raw instanceof Uint8Array) || raw.length > MAX_ARTIFACT_RAW_BYTES) return refuse();
  const len = raw.length;
  if (len < LOCAL_FIXED + CENTRAL_FIXED + EOCD_FIXED) return refuse();
  const v = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
  const u16 = (o: number): number => v.getUint16(o, true);
  const u32 = (o: number): number => v.getUint32(o, true);

  // End of central directory: fixed position because the comment must be empty.
  const eocd = len - EOCD_FIXED;
  if (u32(eocd) !== SIG_EOCD) return refuse();
  if (u16(eocd + 4) !== 0 || u16(eocd + 6) !== 0) return refuse();
  if (u16(eocd + 8) !== 1 || u16(eocd + 10) !== 1) return refuse();
  const cdSize = u32(eocd + 12);
  const cdOffset = u32(eocd + 16);
  if (u16(eocd + 20) !== 0) return refuse();
  if (cdSize === 0xffffffff || cdOffset === 0xffffffff) return refuse();
  if (cdSize !== CENTRAL_FIXED + name.length) return refuse();
  if (cdOffset + cdSize !== eocd) return refuse();

  // Central header.
  if (u32(cdOffset) !== SIG_CENTRAL) return refuse();
  const cVersionNeeded = u16(cdOffset + 6);
  const cFlags = u16(cdOffset + 8);
  const cMethod = u16(cdOffset + 10);
  const cCrc = u32(cdOffset + 16);
  const cCompressed = u32(cdOffset + 20);
  const cSize = u32(cdOffset + 24);
  const cNameLen = u16(cdOffset + 28);
  const cExtraLen = u16(cdOffset + 30);
  const cCommentLen = u16(cdOffset + 32);
  const cDisk = u16(cdOffset + 34);
  const cLocalOffset = u32(cdOffset + 42);
  if (cVersionNeeded !== 10 && cVersionNeeded !== 20) return refuse();
  if (cFlags !== 0 && cFlags !== FLAG_DESCRIPTOR) return refuse();
  if (cMethod !== 0) return refuse();
  if (cNameLen !== name.length || cExtraLen !== 0 || cCommentLen !== 0 || cDisk !== 0) return refuse();
  if (cLocalOffset !== 0) return refuse();
  if (cCompressed === 0xffffffff || cSize === 0xffffffff) return refuse();
  if (cCompressed !== cSize || cSize > MAX_ARTIFACT_CONTENT_BYTES) return refuse();
  for (let i = 0; i < name.length; i += 1) {
    if (raw[cdOffset + CENTRAL_FIXED + i] !== name[i]) return refuse();
  }

  // Local header.
  if (u32(0) !== SIG_LOCAL) return refuse();
  if (u16(4) !== cVersionNeeded || u16(6) !== cFlags || u16(8) !== 0) return refuse();
  if (u16(26) !== name.length || u16(28) !== 0) return refuse();
  for (let i = 0; i < name.length; i += 1) {
    if (raw[LOCAL_FIXED + i] !== name[i]) return refuse();
  }
  const hasDescriptor = (cFlags & FLAG_DESCRIPTOR) !== 0;
  const lCrc = u32(14);
  const lCompressed = u32(18);
  const lSize = u32(22);
  if (hasDescriptor) {
    // Local values are deferred: zero, or already correct.
    if (lCrc !== 0 && lCrc !== cCrc) return refuse();
    if (lCompressed !== 0 && lCompressed !== cCompressed) return refuse();
    if (lSize !== 0 && lSize !== cSize) return refuse();
  } else if (lCrc !== cCrc || lCompressed !== cCompressed || lSize !== cSize) {
    return refuse();
  }

  // Data and optional descriptor must fill the gap to the central directory exactly.
  const dataStart = LOCAL_FIXED + name.length;
  const dataEnd = dataStart + cSize;
  if (dataEnd > cdOffset) return refuse();
  const gap = cdOffset - dataEnd;
  if (!hasDescriptor) {
    if (gap !== 0) return refuse();
  } else if (gap === 16) {
    if (u32(dataEnd) !== SIG_DESCRIPTOR) return refuse();
    if (u32(dataEnd + 4) !== cCrc || u32(dataEnd + 8) !== cCompressed || u32(dataEnd + 12) !== cSize) return refuse();
  } else if (gap === 12) {
    if (u32(dataEnd) !== cCrc || u32(dataEnd + 4) !== cCompressed || u32(dataEnd + 8) !== cSize) return refuse();
  } else {
    return refuse();
  }

  const content = raw.slice(dataStart, dataEnd);
  if (crc32(content) !== cCrc) return refuse();
  return content;
}
