/**
 * B3 redirect-mapping table test (TOG-9696 acceptance).
 *
 * Pins the legacy resolution contract from two-bot's test/unit.redirect.test.ts
 * (TOG-116) against the Worker port (`src/redirect.ts`): every mapping in the
 * fixture table must resolve to a byte-identical 302 target, with identical
 * status codes, headers, click records, and privacy behavior. The fixture rows
 * mirror the `invite_campaigns` row shape (migration 0006 — including the
 * hand-edited bad-code row the legacy suite inserts directly); the live table
 * is verified separately on the staging host before DNS (card acceptance).
 *
 * Runs under plain node:test via Node's type stripping — no extra deps:
 *   node --test test/redirect.test.ts
 * (from wrangler/; Node >= 22.6).
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  TokenBuckets,
  canonicalCallerKey,
  clickIdempotencyKey,
  handleRedirect,
  inviteUrl,
  isReservedInternal,
  isValidInviteCode,
  isValidFallback,
  isValidSlug,
  type Campaign,
  type RedirectClick,
  type RedirectDeps,
} from "../src/redirect.ts";

import { parseMappingsSnapshot, RedirectStore } from "../src/redirect-store.ts";

const GUILD = "111222333444555666";
const CODE = "aB3xY9";
const FALLBACK = "fallbackCode";

/**
 * Mapping snapshot: same rows the legacy suite works with — two campaigns
 * sharing one code (per-place breakdown still separates them), a retired row
 * (still redirects), and a hand-edited invalid code (500, never in Location).
 */
const MAPPINGS: Campaign[] = [
  { slug: "reddit", inviteCode: CODE, label: "r/MMORPG sidebar", disabledAt: null },
  { slug: "twitch", inviteCode: CODE, label: "Twitch panel", disabledAt: null },
  { slug: "retired", inviteCode: CODE, label: "old post", disabledAt: "2026-08-01T00:00:00.000Z" },
  { slug: "badcode", inviteCode: "has space", label: "hand-edited", disabledAt: null },
];

interface Harness {
  deps: RedirectDeps;
  clicks: RedirectClick[];
  errors: string[];
  call: (method: string, path: string, caller?: string) => Promise<{
    status: number;
    headers: Record<string, string>;
    body: string;
    click?: RedirectClick;
  }>;
}

function harness(opts: {
  outage?: boolean;
  recorderThrows?: boolean;
  fallback?: string | null;
  bucket?: { capacity: number; refillPerSecond: number };
  limits?: ConstructorParameters<typeof TokenBuckets>[2];
  now?: () => number;
} = {}): Harness {
  const clicks: RedirectClick[] = [];
  const errors: string[] = [];
  const buckets = new TokenBuckets(
    opts.bucket ?? { capacity: 60, refillPerSecond: 1 },
    opts.now ?? Date.now,
    opts.limits,
  );
  let token = 0;
  const deps: RedirectDeps = {
    guildId: GUILD,
    fallbackInviteCode: opts.fallback === undefined ? FALLBACK : opts.fallback,
    lookup: async (slug) => {
      if (opts.outage) throw new Error("simulated outage");
      return MAPPINGS.find((c) => c.slug === slug) ?? null;
    },
    recordClick: async (click) => {
      if (opts.recorderThrows) throw new Error("simulated store outage");
      clicks.push(click);
    },
    onError: (msg, detail) => errors.push(`${msg} ${JSON.stringify(detail)}`),
    now: () => new Date("2026-09-03T12:00:00.000Z").getTime(),
    uuid: () => `token-${++token}`,
    throttle: (key) => buckets.take(key),
  };
  return {
    deps,
    clicks,
    errors,
    call: async (method, path, caller = "caller-1") => {
      const res = await handleRedirect(method, path, caller, deps);
      // Mirror the Worker entry: record after the 302, swallow failures.
      if (res.click) {
        await deps
          .recordClick(res.click)
          .catch((err: unknown) =>
            errors.push(`invite_click_record_failed ${String(err)}`),
          );
      }
      return res;
    },
  };
}

describe("mapping table: every row resolves byte-identically", () => {
  test("tracked link 302s to the invite and records one click", async () => {
    const h = harness();
    const res = await h.call("GET", "/reddit");
    assert.equal(res.status, 302);
    assert.equal(res.headers["location"], `https://discord.gg/${CODE}`);
    assert.equal(h.clicks.length, 1);
    assert.equal(h.clicks[0]?.source, `invite:${CODE}`);
    assert.equal(h.clicks[0]?.campaign, "reddit");
    assert.equal(h.clicks[0]?.guildId, GUILD);
  });

  test("302 is uncacheable with no referrer; a second click still counts", async () => {
    const h = harness();
    const res = await h.call("GET", "/reddit");
    assert.equal(res.status, 302);
    assert.match(res.headers["cache-control"] ?? "", /no-store/);
    assert.equal(res.headers["referrer-policy"], "no-referrer");
    await h.call("GET", "/reddit");
    assert.equal(h.clicks.length, 2);
  });

  test("two campaigns on one code are told apart by campaign", async () => {
    const h = harness();
    await h.call("GET", "/reddit");
    await h.call("GET", "/twitch");
    assert.equal(h.clicks.length, 2);
    assert.deepEqual(
      h.clicks.map((c) => c.campaign).sort(),
      ["reddit", "twitch"],
    );
    assert.ok(h.clicks.every((c) => c.source === `invite:${CODE}`));
  });

  test("retired campaign still redirects, because the post cannot be edited", async () => {
    const h = harness();
    const res = await h.call("GET", "/retired");
    assert.equal(res.status, 302);
    assert.equal(h.clicks.length, 1);
  });

  test("same-millisecond clicks get distinct idempotency keys", () => {
    const base: RedirectClick = {
      guildId: GUILD,
      code: CODE,
      campaign: "reddit",
      occurredAt: "2026-09-03T12:00:00.000Z",
      dedupeToken: "",
      source: `invite:${CODE}`,
    };
    assert.equal(
      clickIdempotencyKey(base),
      clickIdempotencyKey({ ...base }),
    );
    assert.notEqual(
      clickIdempotencyKey({ ...base, dedupeToken: "a" }),
      clickIdempotencyKey({ ...base, dedupeToken: "b" }),
    );
    assert.match(
      clickIdempotencyKey({ ...base, dedupeToken: "one" }),
      new RegExp(`^${GUILD}:anon:invite_click:2026-09-03T12:00:00\\.000Z:one$`),
    );
  });

  test("slug lookup is case-insensitive and tolerates a trailing slash", async () => {
    const h = harness();
    assert.equal((await h.call("GET", "/Reddit")).status, 302);
    assert.equal((await h.call("GET", "/reddit/")).status, 302);
    assert.equal(h.clicks.length, 2);
  });

  test("encoded known slug still resolves only to the fixed invite host", async () => {
    const h = harness();
    const res = await h.call("GET", "/%72eddit"); // %72 == 'r'
    assert.equal(res.status, 302);
    assert.equal(res.headers["location"], `https://discord.gg/${CODE}`);
    assert.equal(h.clicks.length, 1);
  });

  test("query string is dropped, never recorded", async () => {
    const h = harness();
    const res = await h.call("GET", "/reddit?fbclid=abc123&utm_source=x");
    assert.equal(res.status, 302);
    assert.equal(h.clicks.length, 1);
    assert.ok(!JSON.stringify(h.clicks[0]).includes("fbclid"));
  });
});

describe("privacy: nothing about the visitor survives", () => {
  test("click carries campaign + timestamps only; error paths leak nothing", async () => {
    const h = harness();
    await h.call("GET", "/reddit");
    const blob = JSON.stringify(h.clicks[0]);
    for (const leak of ["Mozilla", "secret", "203.0.113.44"]) {
      assert.ok(!blob.includes(leak));
    }
    // Error paths log slug only — caller key never reaches onError/record.
    const h2 = harness({ outage: true, fallback: null });
    await h2.call("GET", "/reddit");
    assert.ok(h2.errors.join(" ").includes("invite_redirect_lookup_failed"));
    for (const leak of ["127.0.0.1", "caller-1"]) {
      assert.ok(!h2.errors.join(" ").includes(leak));
    }
  });
});

describe("outages and misconfiguration degrade, never strand a member", () => {
  test("failed click write still redirects and records nothing", async () => {
    const h = harness({ recorderThrows: true });
    const res = await h.call("GET", "/reddit");
    assert.equal(res.status, 302);
    assert.equal(h.clicks.length, 0);
  });

  test("lookup outage redirects to fallback and records nothing", async () => {
    const h = harness({ outage: true });
    const res = await h.call("GET", "/reddit");
    assert.equal(res.status, 302);
    assert.equal(res.headers["location"], "https://discord.gg/fallbackCode");
    assert.equal(h.clicks.length, 0);
  });

  test("lookup outage with no fallback is a 503 that records nothing", async () => {
    const h = harness({ outage: true, fallback: null });
    const res = await h.call("GET", "/reddit");
    assert.equal(res.status, 503);
    assert.equal(res.headers["retry-after"], "30");
    assert.equal(h.clicks.length, 0);
  });

  test("misconfigured campaign code is a 500 that records nothing", async () => {
    const h = harness();
    const res = await h.call("GET", "/badcode");
    assert.equal(res.status, 500);
    assert.equal(h.clicks.length, 0);
  });
});

describe("things that are not people", () => {
  test("HEAD redirects but does not count", async () => {
    const h = harness();
    const res = await h.call("HEAD", "/reddit");
    assert.equal(res.status, 302);
    assert.equal(h.clicks.length, 0);
  });

  test("favicon and robots.txt are 404s with no lookup side effects", async () => {
    const h = harness();
    assert.equal((await h.call("GET", "/favicon.ico")).status, 404);
    assert.equal((await h.call("GET", "/robots.txt")).status, 404);
    assert.equal(h.clicks.length, 0);
  });

  test("healthz is a plain 200 and not a click", async () => {
    const h = harness();
    assert.equal((await h.call("GET", "/healthz")).status, 200);
    assert.equal(h.clicks.length, 0);
  });

  test("non-GET method is refused", async () => {
    const h = harness();
    const res = await h.call("POST", "/reddit");
    assert.equal(res.status, 405);
    assert.equal(res.headers["allow"], "GET, HEAD");
    assert.equal(h.clicks.length, 0);
  });
});

describe("reserved internal slugs never become campaigns", () => {
  const METRICS_CAMPAIGN: Campaign = {
    slug: "metrics",
    inviteCode: "synthetic",
  };
  function metricsHarness() {
    const h = harness();
    const lookupCalls: string[] = [];
    h.deps.lookup = async (slug) => {
      lookupCalls.push(slug);
      return MAPPINGS.find((c) => c.slug === slug) ?? METRICS_CAMPAIGN;
    };
    h.deps.recordClick = async () => {
      throw new Error("reserved slug must not record clicks");
    };
    return { h, lookupCalls };
  }

  test("canonical aliases are reserved for every method", async () => {
    for (const path of [
      "/metrics",
      "/METRICS",
      "/%6detrics",
      "//metrics",
      "/metrics/",
      "/metrics/extra",
      "/metrics//extra",
      "/metrics?token=synthetic",
    ]) {
      assert.ok(isReservedInternal(path), `${path} must be reserved`);
      for (const method of ["GET", "HEAD", "POST"]) {
        const { h, lookupCalls } = metricsHarness();
        const res = await h.call(method, path);
        assert.equal(res.status, 404, `${method} ${path} must 404, got ${res.status}`);
        assert.equal(res.headers["location"], undefined);
        assert.ok(!("click" in res) || res.click === undefined);
        assert.equal(lookupCalls.length, 0, `${method} ${path} must skip lookup`);
        assert.equal(h.clicks.length, 0);
      }
    }
  });

  test("near-miss slugs still resolve as campaigns", async () => {
    // `metricsfoo` is not reserved: GET redirects and records, POST is 405.
    assert.ok(!isReservedInternal("/metricsfoo"));
    const { h } = metricsHarness();
    const lookup = h.deps.lookup;
    h.deps.lookup = async (slug) =>
      slug === "metricsfoo"
        ? { slug: "metricsfoo", inviteCode: CODE }
        : lookup(slug);
    const clicks: RedirectClick[] = [];
    h.deps.recordClick = async (click) => { clicks.push(click); };
    const res = await h.call("GET", "/metricsfoo");
    assert.equal(res.status, 302);
    assert.equal(res.headers["location"], `https://discord.gg/${CODE}`);
    assert.equal(clicks.length, 1);
    const post = await h.call("POST", "/metricsfoo");
    assert.equal(post.status, 405);
  });
});

describe("abuse and malformed input fail closed", () => {
  test("one caller's burst throttles at 429 before the store, then refills", async () => {
    let now = 1_000_000;
    const h = harness({
      bucket: { capacity: 5, refillPerSecond: 1 },
      now: () => now,
    });
    const statuses: number[] = [];
    for (let i = 0; i < 8; i++) {
      statuses.push((await h.call("GET", "/reddit")).status);
    }
    assert.deepEqual(statuses, [302, 302, 302, 302, 302, 429, 429, 429]);
    const throttled = await h.call("GET", "/no-such-campaign");
    assert.equal(throttled.status, 429);
    assert.equal(throttled.headers["retry-after"], "1");
    assert.equal(h.clicks.length, 5);
    now += 61_000;
    assert.equal((await h.call("GET", "/reddit")).status, 302);
    assert.equal(h.clicks.length, 6);
  });

  test("unknown slug is a 404 with no redirect target", async () => {
    const h = harness();
    const res = await h.call("GET", "/never-created");
    assert.equal(res.status, 404);
    assert.equal(res.headers["location"], undefined);
    assert.equal(h.clicks.length, 0);
  });

  test("bare domain redirects to fallback without counting", async () => {
    const h = harness();
    const res = await h.call("GET", "/");
    assert.equal(res.status, 302);
    assert.equal(
      res.headers["location"],
      "https://discord.gg/fallbackCode",
    );
    assert.equal(h.clicks.length, 0);
  });

  test("malformed percent-escape and traversal are 404s", async () => {
    const h = harness();
    assert.equal((await h.call("GET", "/%E0%A4%A")).status, 404);
    const traversal = await h.call("GET", "/../../etc/passwd");
    assert.ok(traversal.status === 404 || traversal.status === 400);
    assert.equal(h.clicks.length, 0);
  });

  test("malicious variants are 404s with no redirect target (TOG-7196 set)", async () => {
    const h = harness();
    const malicious = [
      "//evil.example",
      "///evil.example",
      "/%2fevil.example",
      "/%2Fevil.example",
      "/%252fevil.example",
      "/javascript:alert(1)",
      "/JaVaScRiPt:alert(1)",
      "/https://evil.example",
      "/http://evil.example/reddit",
      "/reddit%2f..",
      "/reddit%00",
      "/reddit%0d%0aLocation:https://evil.example",
      "/.evil.example",
      "/-evil",
      "/a",
    ];
    for (const path of malicious) {
      const res = await h.call("GET", path);
      assert.equal(res.status, 404, `${path} must 404, got ${res.status}`);
      assert.equal(
        res.headers["location"],
        undefined,
        `${path} must not redirect`,
      );
    }
    assert.equal(h.clicks.length, 0);
  });
});

describe("configuration and campaign ingress fail closed", () => {
  test("invalid fallback prevents serving even a healthy campaign or probe", async () => {
    for (const fallback of ["has space", "https://discord.gg/code", "x".repeat(65), "code\n"]) {
      for (const path of ["/", "/reddit", "/healthz"]) {
        const h = harness({ fallback });
        let lookups = 0;
        h.deps.lookup = async () => { lookups++; return MAPPINGS[0]!; };
        const res = await h.call("GET", path);
        assert.equal(res.status, 503);
        assert.equal(res.headers.location, undefined);
        assert.equal(lookups, 0);
        assert.equal(h.clicks.length, 0);
        assert.deepEqual(h.errors, ['invite_redirect_invalid_config {"errorClass":"invalid_fallback"}']);
      }
    }
    assert.equal((await harness({ fallback: "" }).call("GET", "/")).status, 404);
    assert.equal((await harness({ fallback: null }).call("GET", "/healthz")).status, 200);
  });

  test("reserved paths reject every method before invalid fallback configuration", async () => {
    for (const path of [
      "/metrics", "/METRICS", "/%6detrics", "//metrics", "/metrics/", "/metrics/extra",
      "/HEALTHZ", "/%68ealthz", "//healthz", "/healthz/", "/%2fhealthz", "/healthz/extra",
      "/healthz/?visitor=synthetic",
    ]) {
      for (const method of ["GET", "HEAD", "POST", "OPTIONS"]) {
        const h = harness({ fallback: "has space" });
        h.deps.lookup = async () => { assert.fail("reserved paths must skip lookup"); };
        h.deps.throttle = () => { assert.fail("reserved paths must skip rate limiting"); };
        const res = await h.call(method, path);
        assert.equal(res.status, 404, `${method} ${path}`);
        assert.equal(res.headers.location, undefined);
        assert.equal(res.click, undefined);
        assert.equal(h.clicks.length, 0);
        assert.deepEqual(h.errors, []);
      }
    }
    for (const method of ["GET", "HEAD", "POST", "OPTIONS"]) {
      const h = harness({ fallback: "has space" });
      assert.equal((await h.call(method, "/healthz?visitor=synthetic")).status, 503);
      assert.deepEqual(h.errors, ['invite_redirect_invalid_config {"errorClass":"invalid_fallback"}']);
    }
  });

  test("reserved prefixes reject malformed suffixes without configuration or throttle effects", async () => {
    for (const path of [
      "/healthz/%", "/metrics/%FF", "/%68ealthz/%", "/%6detrics/%FF",
      "/HEALTHZ/%E0%A4", "//METRICS//%", "/%2fhealthz/%FF", "/%2F%6detrics%2F%",
      "/healthz%2f%", "/metrics%2F%FF", "/healthz/%?visitor=synthetic",
    ]) {
      assert.ok(isReservedInternal(path), path);
      for (const fallback of [FALLBACK, "has space"]) {
        for (const method of ["GET", "HEAD", "POST", "OPTIONS"]) {
          const h = harness({ fallback });
          h.deps.lookup = async () => { assert.fail("reserved prefixes must skip lookup"); };
          h.deps.throttle = () => { assert.fail("reserved prefixes must skip throttling"); };
          const res = await h.call(method, path);
          assert.equal(res.status, 404, `${method} ${path}`);
          assert.equal(res.headers.location, undefined);
          assert.equal(res.click, undefined);
          assert.equal(h.clicks.length, 0);
          assert.deepEqual(h.errors, []);
        }
      }
    }
    for (const path of ["/healthz-campaign/%", "/metricsfoo/%FF", "/%252fhealthz/%", "/hea%FFlthz/%"]) {
      assert.ok(!isReservedInternal(path), path);
    }
  });

  test("fallback bindings reject non-string values without coercion", async () => {
    for (const value of [123, 0, false, true, [], [FALLBACK], {}, { toString() { assert.fail("must not coerce fallback"); } }, Symbol("fixture")]) {
      assert.equal(isValidFallback(value), false);
      for (const path of ["/", "/reddit", "/healthz"]) {
        const h = harness();
        h.deps.fallbackInviteCode = value as string;
        h.deps.lookup = async () => { assert.fail("invalid fallback must skip lookup"); };
        h.deps.throttle = () => { assert.fail("invalid fallback must skip throttling"); };
        const res = await h.call("GET", path);
        assert.equal(res.status, 503);
        assert.equal(res.headers.location, undefined);
        assert.equal(res.click, undefined);
        assert.equal(h.clicks.length, 0);
        assert.deepEqual(h.errors, ['invite_redirect_invalid_config {"errorClass":"invalid_fallback"}']);
      }
    }
    for (const value of [undefined, null, "", FALLBACK]) assert.ok(isValidFallback(value));
  });

  test("snapshot parser rejects non-string input without coercion", () => {
    let coercions = 0;
    const object = { toString() { coercions++; return "[]"; } };
    for (const value of [undefined, null, false, 0, [], ["[]"], {}, object]) {
      assert.throws(() => parseMappingsSnapshot(value), { message: "Invalid redirect mappings snapshot" });
    }
    assert.equal(coercions, 0);
  });

  test("healthz aliases cannot become a campaign, even with a polluted lookup", async () => {
    for (const path of ["/healthz", "/HEALTHZ", "/%68ealthz", "//healthz", "/healthz/", "/%2fhealthz", "/healthz/extra"]) {
      for (const method of ["GET", "HEAD", "POST"]) {
        const h = harness();
        let lookups = 0;
        h.deps.lookup = async () => { lookups++; return { slug: "healthz", inviteCode: CODE }; };
        const res = await h.call(method, path);
        assert.equal(res.status, path === "/healthz" ? (method === "POST" ? 405 : 200) : 404);
        assert.equal(res.headers.location, undefined);
        assert.equal(lookups, 0);
        assert.equal(h.clicks.length, 0);
      }
    }
    assert.ok(isValidSlug("healthz-campaign"));
    assert.ok(!isValidSlug("healthz"));
  });

  test("snapshot rejects malformed, reserved and duplicate campaigns without echoing values", () => {
    const row = { slug: "reddit", invite_code: CODE, label: "sidebar", disabled_at: null };
    for (const input of [
      "null", "{}", "[null]", "[1]", '"secret-value"', "not json secret-value",
      JSON.stringify([{ ...row, slug: "healthz" }]),
      ...["HEALTHZ", "%68ealthz", "/healthz/", "metrics", "a", "a".repeat(41), "reddit\n"].map(slug => JSON.stringify([{ ...row, slug }])),
      ...[undefined, null, 123, "has space", "x".repeat(65), "secret-value\n"].map(invite_code => JSON.stringify([{ ...row, invite_code }])),
      JSON.stringify([{ ...row, label: 123 }]),
      JSON.stringify([{ ...row, disabled_at: {} }]),
      JSON.stringify([row, row]),
    ]) {
      assert.throws(() => parseMappingsSnapshot(input), { message: "Invalid redirect mappings snapshot" });
    }
    assert.deepEqual(parseMappingsSnapshot("[]"), []);
    assert.deepEqual(parseMappingsSnapshot(JSON.stringify([row])), [{ slug: "reddit", inviteCode: CODE, label: "sidebar", disabledAt: null }]);
  });

  test("reserved and malformed slugs never reach the database", async () => {
    let connections = 0;
    const store = new RedirectStore({ connectionString: "fixture" }, async () => {
      connections++;
      throw new Error("must not connect");
    });
    for (const slug of ["healthz", "HEALTHZ", "%68ealthz", "metrics", "invalid/slug"]) {
      assert.equal(await store.lookup(slug), null);
    }
    assert.equal(connections, 0);
  });

  test("lookup error logs only a validated slug and a fixed error class", async () => {
    const secret = "postgres://fixture:secret-value@fixture/db?visitor=203.0.113.44";
    for (const [thrown, errorClass] of [
      [new Error(secret), "Error"], [new TypeError(secret), "TypeError"],
      [new RangeError(secret), "RangeError"], [secret, "Unknown"],
      [{ name: secret, message: secret }, "Unknown"],
      [Object.assign(new Error(secret), { name: secret }), "Error"],
    ] as const) {
      const h = harness({ fallback: null });
      h.deps.lookup = async () => { throw thrown; };
      assert.equal((await h.call("GET", "/reddit?visitor=203.0.113.44", secret)).status, 503);
      assert.deepEqual(h.errors, [`invite_redirect_lookup_failed ${JSON.stringify({ slug: "reddit", errorClass })}`]);
    }
    const h = harness({ outage: true });
    for (const path of ["/postgres:secret-value", "/reddit%0a", "/" + "x".repeat(100)]) {
      assert.equal((await h.call("GET", path)).status, 404);
    }
    assert.deepEqual(h.errors, []);
  });
});

describe("idle buckets expire but throttles never reset", () => {
  test("many one-time keys expire after the fully-refilled idle window", async () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 5, refillPerSecond: 1 },
      () => now,
    );
    for (let i = 0; i < 200; i++) {
      assert.ok(buckets.take(`synthetic-key-${i}`).allowed);
    }
    assert.equal(buckets.size, 200);
    // Below the 5-second full-refill window: nothing may expire yet.
    now += 4_000;
    assert.ok(buckets.take("synthetic-key-200").allowed);
    assert.equal(buckets.size, 201);
    // Past the window: four live takes reap 64+64+64+8 = 200 stale entries;
    // only the five live keys (200–204) remain.
    now += 2_000;
    assert.ok(buckets.take("synthetic-key-201").allowed);
    assert.ok(buckets.take("synthetic-key-202").allowed);
    assert.ok(buckets.take("synthetic-key-203").allowed);
    assert.ok(buckets.take("synthetic-key-204").allowed);
    assert.equal(buckets.size, 5);
  });

  test("an exhausted key stays throttled until refill, never via eviction", async () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 2, refillPerSecond: 1 },
      () => now,
    );
    assert.ok(buckets.take("hot").allowed);
    assert.ok(buckets.take("hot").allowed);
    assert.ok(!buckets.take("hot").allowed);
    // New keys churn around it; the hot bucket must keep its debt.
    for (let i = 0; i < 10; i++) {
      assert.ok(buckets.take(`churn-${i}`).allowed);
      assert.ok(!buckets.take("hot").allowed);
    }
    now += 1_100;
    assert.ok(buckets.take("hot").allowed);
    now += 1_100;
    assert.ok(buckets.take("hot").allowed);
  });

  test("a sub-refill idle TTL is clamped to the full-refill window", async () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 4, refillPerSecond: 1 },
      () => now,
      { idleTtlMs: 500 },
    );
    assert.ok(buckets.take("drained").allowed);
    for (let i = 0; i < 4; i++) buckets.take("drained");
    assert.ok(!buckets.take("drained").allowed);
    // 500ms would have expired the TTL; clamped to 4s, it must not.
    now += 600;
    assert.ok(!buckets.take("drained").allowed);
    now += 3_500;
    assert.ok(buckets.take("drained").allowed);
  });

  test("past the cardinality cap new keys shed load, tracked keys keep theirs", async () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 1, refillPerSecond: 1 },
      () => now,
      { maxBuckets: 3 },
    );
    assert.ok(buckets.take("a").allowed);
    assert.ok(buckets.take("b").allowed);
    assert.ok(buckets.take("c").allowed);
    assert.equal(buckets.size, 3);
    const shed = buckets.take("overflow");
    assert.ok(!shed.allowed);
    assert.ok(shed.retryAfter >= 1);
    assert.equal(buckets.size, 3);
    // Tracked keys still spend and throttle exactly as before.
    assert.ok(!buckets.take("a").allowed);
    assert.ok(!buckets.take("b").allowed);
    now += 1_100;
    assert.ok(buckets.take("a").allowed);
  });

  test("a full map of idle keys drains on new-key traffic alone", async () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 60, refillPerSecond: 1 },
      () => now,
      { maxBuckets: 100 },
    );
    for (let i = 0; i < 100; i++) {
      assert.ok(buckets.take(`filler-${i}`).allowed);
    }
    assert.equal(buckets.size, 100);
    // No tracked key ever returns. Every new caller must still be admitted
    // once the fillers have sat idle past the full-refill window.
    now += 24 * 60 * 60 * 1000;
    for (let i = 0; i < 50; i++) {
      assert.ok(buckets.take(`newcomer-${i}`).allowed);
    }
    assert.equal(buckets.size, 50);
  });

  test("the reap before an at-cap refusal stays within the sweep budget", async () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 1, refillPerSecond: 1 },
      () => now,
      { maxBuckets: 10, sweepBudget: 2 },
    );
    for (let i = 0; i < 10; i++) {
      assert.ok(buckets.take(`filler-${i}`).allowed);
    }
    now += 2_000;
    // Reap 2 to make room, write the newcomer, reap 2 more: 10 - 2 + 1 - 2.
    assert.ok(buckets.take("newcomer").allowed);
    assert.equal(buckets.size, 7);
  });

  test("at the cap a live map still refuses and a depleted key keeps its debt", async () => {
    const t0 = 1_000_000;
    let now = t0;
    const buckets = new TokenBuckets(
      { capacity: 4, refillPerSecond: 1 },
      () => now,
      { maxBuckets: 3 },
    );
    assert.ok(buckets.take("old").allowed);
    now = t0 + 3_500;
    for (let i = 0; i < 4; i++) assert.ok(buckets.take("hot").allowed);
    assert.ok(!buckets.take("hot").allowed);
    assert.ok(buckets.take("a").allowed);
    assert.equal(buckets.size, 3);
    // "old" has sat idle for the 4s full-refill window; "hot" owes tokens.
    // The newcomer may take only the idle slot.
    now = t0 + 4_000;
    assert.ok(buckets.take("new").allowed);
    assert.equal(buckets.size, 3);
    const hot = buckets.take("hot");
    assert.ok(!hot.allowed);
    assert.equal(hot.retryAfter, 1);
    // Every remaining entry is live: fail closed.
    const shed = buckets.take("late");
    assert.ok(!shed.allowed);
    assert.ok(shed.retryAfter >= 1);
    assert.equal(buckets.size, 3);
    // 0.5 + 1 refilled tokens: one take, not the four a reset would grant.
    now = t0 + 5_000;
    assert.ok(buckets.take("hot").allowed);
    assert.ok(!buckets.take("hot").allowed);
  });

  test("sweep work per call is bounded regardless of map size", async () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 60, refillPerSecond: 1 },
      () => now,
      { idleTtlMs: 60_000, sweepBudget: 4 },
    );
    for (let i = 0; i < 100; i++) {
      assert.ok(buckets.take(`idle-${i}`).allowed);
    }
    now += 61_000;
    // One call reaps at most 4 of the 100 idle entries, then writes its own.
    assert.ok(buckets.take("fresh").allowed);
    assert.equal(buckets.size, 97);
    // Repeat live calls a bounded number of times: each reaps at most 4
    // stale entries (stale front stays front while idle), and every new key
    // written 60s+ after the burst is itself live — so after 24 more writes
    // all 100 stale entries are gone with the 25 live keys remaining.
    for (let i = 0; i < 24; i++) {
      now += 1_000;
      assert.ok(buckets.take(`fresh-${i}`).allowed);
    }
    assert.equal(buckets.size, 25);
  });
});

describe("F7 residual: canonical quota, unknown budget, terminal 429s (TOG-12469)", () => {
  test("canonicalCallerKey folds aliases and unknowns", () => {
    assert.equal(canonicalCallerKey("203.0.113.44"), "203.0.113.44");
    assert.equal(canonicalCallerKey("  203.0.113.44  "), "203.0.113.44");
    assert.equal(canonicalCallerKey("FE80::1"), "fe80::1");
    assert.equal(canonicalCallerKey("fe80::1%eth0"), "fe80::1");
    assert.equal(canonicalCallerKey("::ffff:192.0.2.1"), "192.0.2.1");
    assert.equal(canonicalCallerKey("::FFFF:192.0.2.1"), "192.0.2.1");
    for (const u of ["unknown", "UNKNOWN", " Unknown ", "", "   "]) {
      assert.equal(canonicalCallerKey(u), "unknown", JSON.stringify(u));
    }
    assert.equal(canonicalCallerKey(undefined), "unknown");
    assert.equal(canonicalCallerKey(null), "unknown");
    assert.equal(canonicalCallerKey(42), "unknown");
    // Long keys merge into a shared 256-char bucket instead of minting more.
    const long = `k-${"x".repeat(300)}`;
    assert.equal(canonicalCallerKey(long), canonicalCallerKey(`${long}-suffix`));
    assert.equal(canonicalCallerKey(long).length, 256);
  });

  test("caller aliases share one quota bucket", () => {
    const now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 2, refillPerSecond: 1 },
      () => now,
      { maxConsecutiveDenials: 100 },
    );
    assert.ok(buckets.take("10.0.0.1").allowed);
    assert.ok(buckets.take("  10.0.0.1 ").allowed);
    // Same caller under another spelling: the budget is shared, not doubled.
    assert.ok(!buckets.take("10.0.0.1").allowed);
    assert.equal(buckets.size, 1);
  });

  test("unknown-key flood cannot fill the map or starve valid callers", () => {
    const now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 1, refillPerSecond: 1 },
      () => now,
      { maxBuckets: 3, maxConsecutiveDenials: 1000 },
    );
    // Distinct raw spellings that all mean "no edge signal".
    const spellings = ["unknown", "UNKNOWN", " Unknown ", "", "   "];
    for (let i = 0; i < 20; i++) buckets.take(spellings[i % spellings.length]!);
    assert.equal(buckets.size, 1, "no-signal traffic mints exactly one entry");
    assert.ok(buckets.take("203.0.113.7").allowed, "valid caller admitted");
    assert.ok(buckets.take("203.0.113.8").allowed, "second valid caller admitted");
    assert.equal(buckets.size, 3);
  });

  test("sustained denials earn a terminal hold that retries cannot extend", () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 1, refillPerSecond: 1 },
      () => now,
      { maxConsecutiveDenials: 3, terminalCooldownMs: 60_000 },
    );
    assert.ok(buckets.take("hammer").allowed);
    assert.ok(!buckets.take("hammer").allowed);
    assert.ok(!buckets.take("hammer").allowed);
    const terminal = buckets.take("hammer");
    assert.ok(!terminal.allowed);
    assert.equal(terminal.retryAfter, 60);
    // Retries during the hold stay refused and the hold end never moves.
    now += 10_000;
    const retry = buckets.take("hammer");
    assert.ok(!retry.allowed);
    assert.equal(retry.retryAfter, 50);
    now += 10_000;
    const retry2 = buckets.take("hammer");
    assert.ok(!retry2.allowed);
    assert.equal(retry2.retryAfter, 40);
    // Other callers are unaffected by one key's hold.
    assert.ok(buckets.take("innocent").allowed);
    // Past the hold the streak is cleared: normal quota resumes, and a single
    // fresh denial does not instantly re-hold.
    now += 41_000;
    assert.ok(buckets.take("hammer").allowed);
    const fresh = buckets.take("hammer");
    assert.ok(!fresh.allowed);
    assert.equal(fresh.retryAfter, 1);
  });

  test("an elapsed hold resumes normal quota without idle expiry", () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 1, refillPerSecond: 1 },
      () => now,
      { idleTtlMs: 3_600_000, maxConsecutiveDenials: 2, terminalCooldownMs: 5_000 },
    );
    assert.ok(buckets.take("k").allowed);
    assert.ok(!buckets.take("k").allowed);
    const terminal = buckets.take("k");
    assert.ok(!terminal.allowed);
    assert.equal(terminal.retryAfter, 5);
    now += 6_000;
    // Hold elapsed but the bucket is far from idle: quota resumes and the
    // next over-budget streak must re-earn a hold, not inherit one.
    assert.ok(buckets.take("k").allowed);
    const r1 = buckets.take("k");
    assert.ok(!r1.allowed);
    assert.equal(r1.retryAfter, 1);
    const r2 = buckets.take("k");
    assert.ok(!r2.allowed);
    assert.equal(r2.retryAfter, 5);
    assert.equal(buckets.size, 1);
  });

  test("a live hold survives idle expiry and the sweep", () => {
    let now = 1_000_000;
    const buckets = new TokenBuckets(
      { capacity: 60, refillPerSecond: 1 },
      () => now,
      { idleTtlMs: 60_000, sweepBudget: 64, maxConsecutiveDenials: 2, terminalCooldownMs: 120_000 },
    );
    for (let i = 0; i < 60; i++) assert.ok(buckets.take("held").allowed);
    assert.ok(!buckets.take("held").allowed);
    const terminal = buckets.take("held");
    assert.ok(!terminal.allowed);
    assert.equal(terminal.retryAfter, 120);
    // Past the idle window with only unrelated traffic: the hold is
    // enforcement state, not idle, so it is neither reaped nor reset.
    now += 61_000;
    for (let i = 0; i < 10; i++) assert.ok(buckets.take(`other-${i}`).allowed);
    assert.equal(buckets.size, 11);
    const still = buckets.take("held");
    assert.ok(!still.allowed);
    assert.equal(still.retryAfter, 59);
  });

  test("redirect quota follows caller aliases, not spellings", async () => {
    const h = harness({
      bucket: { capacity: 2, refillPerSecond: 1 },
      now: () => 1_000_000,
    });
    assert.equal((await h.call("GET", "/reddit", "203.0.113.44")).status, 302);
    assert.equal((await h.call("GET", "/reddit", "  203.0.113.44 ")).status, 302);
    // Same caller under another spelling: the budget is shared, not doubled.
    const throttled = await h.call("GET", "/reddit", "203.0.113.44");
    assert.equal(throttled.status, 429);
    assert.equal(h.clicks.length, 2);
  });

  test("refill accrues across a hold, capped at capacity (deliberate, TOG-12533)", () => {
    let now = 1_000_000;
    // Long idle TTL: the post-hold quota below comes from refill, not expiry.
    const limits = { idleTtlMs: 3_600_000, maxConsecutiveDenials: 2 };
    const short = new TokenBuckets(
      { capacity: 60, refillPerSecond: 1 },
      () => now,
      { ...limits, terminalCooldownMs: 3_000 },
    );
    for (let i = 0; i < 60; i++) assert.ok(short.take("k").allowed);
    assert.ok(!short.take("k").allowed);
    assert.equal(short.take("k").retryAfter, 3);
    now += 1_000;
    assert.equal(short.take("k").retryAfter, 2, "mid-hold takes spend nothing");
    now += 2_000;
    // The 3-second hold refilled exactly 3 tokens: obeying the advertised
    // wait earns what any idle caller would, never more.
    for (let i = 0; i < 3; i++) assert.ok(short.take("k").allowed, `admit ${i}`);
    const after = short.take("k");
    assert.ok(!after.allowed);
    assert.equal(after.retryAfter, 1);

    const long = new TokenBuckets(
      { capacity: 5, refillPerSecond: 1 },
      () => now,
      { ...limits, terminalCooldownMs: 30_000 },
    );
    for (let i = 0; i < 5; i++) assert.ok(long.take("k").allowed);
    assert.ok(!long.take("k").allowed);
    assert.equal(long.take("k").retryAfter, 30);
    now += 30_000;
    // A hold longer than the refill window still banks only one bucket.
    for (let i = 0; i < 5; i++) assert.ok(long.take("k").allowed, `admit ${i}`);
    assert.ok(!long.take("k").allowed);
  });

  test("a held caller's redirect 429 carries the remaining hold, counting down", async () => {
    let now = 1_000_000;
    const h = harness({
      bucket: { capacity: 2, refillPerSecond: 1 },
      limits: { maxConsecutiveDenials: 3, terminalCooldownMs: 60_000 },
      now: () => now,
    });
    const lookup = h.deps.lookup;
    let lookups = 0;
    h.deps.lookup = (slug) => { lookups += 1; return lookup(slug); };
    assert.equal((await h.call("GET", "/reddit")).status, 302);
    assert.equal((await h.call("GET", "/reddit")).status, 302);
    // Plain refill denials before the streak earns a hold: a short wait.
    for (let i = 0; i < 2; i++) {
      const refill = await h.call("GET", "/reddit");
      assert.equal(refill.status, 429);
      assert.equal(refill.headers["retry-after"], "1");
    }
    // The denial that earns the hold advertises it, not a one-second retry.
    const held = await h.call("GET", "/reddit");
    assert.equal(held.status, 429);
    assert.equal(held.headers["retry-after"], "60");
    // Retries during the hold, on any path or caller alias, see the same hold
    // end counting down and never reach the store.
    now += 15_000;
    const later = await h.call("GET", "/no-such-campaign", " CALLER-1 ");
    assert.equal(later.status, 429);
    assert.equal(later.headers["retry-after"], "45");
    now += 44_500;
    const last = await h.call("HEAD", "/reddit");
    assert.equal(last.status, 429);
    assert.equal(last.headers["retry-after"], "1", "a partial second rounds up");
    assert.equal(lookups, 2);
    assert.equal(h.clicks.length, 2);
    // Hold over: the caller is served again.
    now += 500;
    assert.equal((await h.call("GET", "/reddit")).status, 302);
    assert.equal(h.clicks.length, 3);
  });
});

describe("validators agree with the shapes we accept", () => {
  test("slugs and codes", () => {
    for (const good of ["reddit", "r-mmorpg", "twitch-panel-2", "ab"]) {
      assert.ok(isValidSlug(good), good);
    }
    for (const bad of [
      "a",
      "UPPER",
      "has space",
      "-lead",
      "trail-",
      "a".repeat(41),
      "",
    ]) {
      assert.ok(!isValidSlug(bad), bad);
    }
    for (const good of ["aB3xY9", "two-gaming"]) {
      assert.ok(isValidInviteCode(good), good);
    }
    for (const bad of ["has space", "a/b", "", "x".repeat(65)]) {
      assert.ok(!isValidInviteCode(bad), bad);
    }
    assert.equal(inviteUrl(CODE), `https://discord.gg/${CODE}`);
  });
});
