/** Worker ingress regressions: synthetic bindings only, no endpoint/DB probes. */
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

const ROW = { slug: "reddit", invite_code: "fixtureCode", label: "sidebar", disabled_at: null };
const env = (values: Partial<Env> = {}): Env => ({
  TWO_BOT: { getByName() { throw new Error("redirect must not touch the Container"); } } as unknown as Env["TWO_BOT"],
  GUILD_ID: "111222333444555666",
  REDIRECT_FALLBACK_CODE: "fallbackCode",
  REDIRECT_MAPPINGS_JSON: JSON.stringify([ROW]),
  ...values,
});

const ctx = { waitUntil() { throw new Error("invalid config/probes must not record clicks"); } } as unknown as ExecutionContext;

test("invalid fallback or snapshot returns a sanitized 503 before serving", async (t) => {
  const logs: string[] = [];
  t.mock.method(console, "error", (msg: string) => { logs.push(msg); });
  for (const [values, errorClass] of [
    [{ REDIRECT_FALLBACK_CODE: "fixture-secret\n" }, "invalid_fallback"],
    [{ REDIRECT_FALLBACK_CODE: "https://fixture.invalid/fixture-secret" }, "invalid_fallback"],
    [{ REDIRECT_MAPPINGS_JSON: "malformed fixture-secret" }, "invalid_snapshot"],
    [{ REDIRECT_MAPPINGS_JSON: "null" }, "invalid_snapshot"],
    [{ REDIRECT_MAPPINGS_JSON: JSON.stringify([ROW, { ...ROW, slug: "healthz" }]) }, "invalid_snapshot"],
  ] as const) {
    for (const path of ["/", "/reddit", "/healthz"]) {
      logs.length = 0;
      const res = await worker.fetch(new Request(`https://fixture.invalid${path}`), env(values), ctx);
      assert.equal(res.status, 503);
      assert.equal(res.headers.get("location"), null);
      assert.equal(res.headers.get("retry-after"), "30");
      assert.equal(await res.text(), "redirect service misconfigured\n");
      assert.deepEqual(logs, [`invite_redirect_invalid_config ${JSON.stringify({ errorClass })}`]);
    }
  }
});

test("reserved healthz aliases never redirect, and the exact probe stays healthy", async () => {
  // A reserved row cannot be accepted as a mapping; aliases are still rejected
  // before parsing configuration, with no Container or click side effects.
  const polluted = env({ REDIRECT_MAPPINGS_JSON: JSON.stringify([{ ...ROW, slug: "healthz" }]) });
  for (const path of ["/HEALTHZ", "/%68ealthz", "//healthz", "/healthz/", "/%2fhealthz", "/healthz/extra"]) {
    for (const method of ["GET", "HEAD", "POST"]) {
      const res = await worker.fetch(new Request(`https://fixture.invalid${path}`, { method }), polluted, ctx);
      assert.equal(res.status, 404);
      assert.equal(res.headers.get("location"), null);
    }
  }
  for (const method of ["GET", "HEAD"]) {
    const res = await worker.fetch(new Request("https://fixture.invalid/healthz", { method }), env(), ctx);
    assert.equal(res.status, 200);
  }
});

test("valid configuration preserves fallback, HEAD and disabled campaigns", async () => {
  const configured = env({ REDIRECT_MAPPINGS_JSON: JSON.stringify([{ ...ROW, disabled_at: "2026-08-01T00:00:00Z" }]) });
  for (const [path, target] of [["/", "fallbackCode"], ["/reddit", "fixtureCode"]]) {
    const res = await worker.fetch(new Request(`https://fixture.invalid${path}`, { method: "HEAD" }), configured, ctx);
    assert.equal(res.status, 302);
    assert.equal(res.headers.get("location"), `https://discord.gg/${target}`);
    assert.match(res.headers.get("cache-control") ?? "", /no-store/);
    assert.equal(res.headers.get("referrer-policy"), "no-referrer");
  }
  assert.equal((await worker.fetch(new Request("https://fixture.invalid/"), env({ REDIRECT_FALLBACK_CODE: "" }), ctx)).status, 404);
});
