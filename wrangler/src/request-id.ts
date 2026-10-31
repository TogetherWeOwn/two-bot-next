/**
 * End-to-end correlation id shared with the container
 * (`crates/core/src/internal_actions.rs`: `REQUEST_ID_HEADER`,
 * `REQUEST_ID_LEN`, `is_request_id`, `new_request_id`).
 *
 * The Worker stamps `x-request-id` on every forwarded request, the Durable
 * Object forwards it to the container, and the container honors it when it
 * has a generated shape. Only that shape is ever honored or logged, so a
 * caller-supplied bearer token, URL or free text can never become a join
 * key in logs or metrics. The drift test in `test/request-id.test.ts` pins
 * the name, length and alphabet against the Rust source.
 */

/** Wire header carrying the correlation id. Lowercase on the wire. */
export const REQUEST_ID_HEADER = "x-request-id";
/** ULID-ish ids are 26 Crockford characters. */
export const REQUEST_ID_LEN = 26;
/** Crockford base-32 (I/L/O/U excluded), mirroring the Rust core. */
const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/** True only for values the generator could have produced. */
export function isRequestId(value: string): boolean {
  if (value.length !== REQUEST_ID_LEN) return false;
  for (const char of value) {
    if (!CROCKFORD.includes(char)) return false;
  }
  return true;
}

/**
 * A ULID-ish request id: 10 time-ordered Crockford chars plus 16 random
 * ones, so a grep of the log sorts naturally. Randomness defaults to
 * `crypto.getRandomValues` and stays a parameter so tests stay deterministic.
 */
export function newRequestId(
  nowMs: number = Date.now(),
  rand: Uint8Array = crypto.getRandomValues(new Uint8Array(16)),
): string {
  let time = Math.max(0, Math.floor(nowMs));
  let head = "";
  for (let i = 0; i < 10; i++) {
    head = CROCKFORD[time % 32] + head;
    time = Math.floor(time / 32);
  }
  let tail = "";
  for (const byte of rand) tail += CROCKFORD[byte % 32];
  return head + tail;
}

/**
 * The id this hop forwards: the inbound header when it has a generated
 * shape, else a fresh one. Never returns the attacker value.
 */
export function ensureRequestId(inbound: string | null): string {
  return inbound !== null && isRequestId(inbound) ? inbound : newRequestId();
}
