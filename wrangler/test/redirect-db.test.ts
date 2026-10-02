/**
 * REDIRECT_DB store path (TOG-12194): fake DbClient only, no database or
 * endpoint is contacted. The Worker-level case binds an unparseable
 * connection string, so node-postgres fails before opening any socket.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";
import {
  clickIdempotencyKey,
  handleRedirect,
  type RedirectClick,
} from "../src/redirect.ts";
import {
  DbTimeoutError,
  RedirectStore,
  type ConnectFn,
  type DbClient,
} from "../src/redirect-store.ts";

const GUILD = "111222333444555666";
const FALLBACK = "fallbackCode";
const SECRET_URL = "postgres://fixture-user:fixture-secret@fixture-host.invalid:5432/fixture";
const ROW = {
  slug: "reddit",
  invite_code: "aB3xY9",
  label: "r/MMORPG sidebar",
  disabled_at: null,
  created_at: "2026-08-01T00:00:00.000Z",
};

interface Fake {
  connect: ConnectFn;
  connects: string[];
  queries: { text: string; params: unknown[] }[];
  ends: number;
}

function fakeDb(opts: {
  rows?: Record<string, unknown>[];
  connect?: () => Promise<void>;
  query?: () => Promise<void>;
  end?: () => Promise<void>;
} = {}): Fake {
  const fake: Fake = { connects: [], queries: [], ends: 0, connect: async () => { throw new Error("unset"); } };
  fake.connect = async (connectionString) => {
    fake.connects.push(connectionString);
    await opts.connect?.();
    const client: DbClient = {
      async query(text, params) {
        fake.queries.push({ text, params });
        await opts.query?.();
        return { rows: opts.rows ?? [] };
      },
      async end() {
        fake.ends++;
        await opts.end?.();
      },
    };
    return client;
  };
  return fake;
}

const store = (fake: Fake, timeoutMs?: number) =>
  new RedirectStore({ connectionString: SECRET_URL }, fake.connect, [], timeoutMs);

/** handleRedirect with the store as its source, as index.ts wires it. */
async function redirectThrough(s: RedirectStore, path = "/reddit") {
  const errors: string[] = [];
  const res = await handleRedirect("GET", path, "caller-1", {
    guildId: GUILD,
    fallbackInviteCode: FALLBACK,
    lookup: (slug) => s.lookup(slug),
    recordClick: (click) => s.recordClick(click),
    onError: (msg, detail) => errors.push(`${msg} ${JSON.stringify(detail)}`),
  });
  return { res, errors };
}

function assertNoSecrets(text: string) {
  for (const leak of ["fixture-secret", "fixture-user", "fixture-host", "postgres://", "caller-1"]) {
    assert.ok(!text.includes(leak), `log leaked ${leak}: ${text}`);
  }
}

describe("lookup through a bound REDIRECT_DB", () => {
  test("hit maps the row, queries by slug and ends the client", async () => {
    const fake = fakeDb({ rows: [ROW] });
    assert.deepEqual(await store(fake).lookup("reddit"), {
      slug: "reddit",
      inviteCode: "aB3xY9",
      label: "r/MMORPG sidebar",
      disabledAt: null,
    });
    assert.deepEqual(fake.connects, [SECRET_URL]);
    assert.equal(fake.queries.length, 1);
    assert.match(fake.queries[0]!.text, /FROM invite_campaigns WHERE slug = \$1/);
    assert.deepEqual(fake.queries[0]!.params, ["reddit"]);
    assert.equal(fake.ends, 1);
  });

  test("miss is null (a 404, not an outage) and still ends the client", async () => {
    const fake = fakeDb({ rows: [] });
    assert.equal(await store(fake).lookup("never-created"), null);
    assert.equal(fake.ends, 1);
    const { res, errors } = await redirectThrough(store(fakeDb()), "/never-created");
    assert.equal(res.status, 404);
    assert.equal(res.headers["location"], undefined);
    assert.deepEqual(errors, []);
  });

  test("a hit redirects to the stored invite and the snapshot is ignored", async () => {
    const fake = fakeDb({ rows: [ROW] });
    const s = new RedirectStore({ connectionString: SECRET_URL }, fake.connect, [
      { slug: "reddit", inviteCode: "snapshotCode" },
    ]);
    const { res } = await redirectThrough(s);
    assert.equal(res.status, 302);
    assert.equal(res.headers["location"], "https://discord.gg/aB3xY9");
    assert.equal(res.click?.campaign, "reddit");
  });
});

describe("click insert through a bound REDIRECT_DB", () => {
  test("inserts with the idempotency key and ends the client", async () => {
    const fake = fakeDb();
    const click: RedirectClick = {
      guildId: GUILD,
      code: "aB3xY9",
      campaign: "reddit",
      occurredAt: "2026-09-03T12:00:00.000Z",
      dedupeToken: "token-1",
      source: "invite:aB3xY9",
    };
    await store(fake).recordClick(click);
    assert.equal(fake.queries.length, 1);
    const { text, params } = fake.queries[0]!;
    assert.match(text, /INSERT INTO events/);
    assert.match(text, /ON CONFLICT \(idempotency_key\) DO NOTHING/);
    assert.deepEqual(params, [
      GUILD,
      "2026-09-03T12:00:00.000Z",
      "invite:aB3xY9",
      JSON.stringify({ campaign: "reddit" }),
      `${GUILD}:anon:invite_click:2026-09-03T12:00:00.000Z:token-1`,
    ]);
    assert.equal(params[4], clickIdempotencyKey(click));
    assert.equal(fake.ends, 1);
  });

  test("a failed insert rejects (logged by the Worker) and still ends the client", async () => {
    const fake = fakeDb({ query: async () => { throw new Error(SECRET_URL); } });
    await assert.rejects(store(fake).recordClick({
      guildId: GUILD, code: "aB3xY9", campaign: "reddit",
      occurredAt: "2026-09-03T12:00:00.000Z", dedupeToken: "t", source: "invite:aB3xY9",
    }));
    assert.equal(fake.ends, 1);
  });
});

describe("store failures fall back without leaking the connection string", () => {
  for (const [name, opts, errorClass] of [
    ["connect failure", { connect: async () => { throw new Error(SECRET_URL); } }, "Error"],
    ["connect TypeError", { connect: async () => { throw new TypeError(`Invalid URL ${SECRET_URL}`); } }, "TypeError"],
    ["query failure", { query: async () => { throw Object.assign(new Error(SECRET_URL), { name: SECRET_URL }); } }, "Error"],
  ] as const) {
    test(`${name} redirects to REDIRECT_FALLBACK_CODE with a bounded errorClass`, async () => {
      const fake = fakeDb(opts);
      const { res, errors } = await redirectThrough(store(fake));
      assert.equal(res.status, 302);
      assert.equal(res.headers["location"], `https://discord.gg/${FALLBACK}`);
      assert.equal(res.click, undefined);
      assert.deepEqual(errors, [
        `invite_redirect_lookup_failed ${JSON.stringify({ slug: "reddit", errorClass })}`,
      ]);
      assertNoSecrets(errors.join("\n") + JSON.stringify(res));
      // A client that connected is ended; a failed connect has none to end.
      assert.equal(fake.ends, name === "query failure" ? 1 : 0);
    });
  }

  test("a hung connect times out, and a late client is ended on arrival", async () => {
    let arrive: () => void = () => undefined;
    const fake = fakeDb({ connect: () => new Promise<void>((resolve) => { arrive = resolve; }) });
    const started = Date.now();
    const { res, errors } = await redirectThrough(store(fake, 20));
    assert.ok(Date.now() - started < 1_000);
    assert.equal(res.headers["location"], `https://discord.gg/${FALLBACK}`);
    assert.deepEqual(errors, [`invite_redirect_lookup_failed ${JSON.stringify({ slug: "reddit", errorClass: "Error" })}`]);
    assert.equal(fake.ends, 0);
    arrive();
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(fake.ends, 1);
  });

  test("a hung query times out with DbTimeoutError and ends the client", async () => {
    const fake = fakeDb({ query: () => new Promise<void>(() => undefined) });
    await assert.rejects(store(fake, 20).lookup("reddit"), DbTimeoutError);
    assert.equal(fake.ends, 1);
  });

  test("a failing end() does not turn a hit into an outage", async () => {
    const fake = fakeDb({ rows: [ROW], end: async () => { throw new Error(SECRET_URL); } });
    assert.equal((await store(fake).lookup("reddit"))?.inviteCode, "aB3xY9");
    assert.equal(fake.ends, 1);
  });
});

describe("binding presence decides the source", () => {
  test("unbound REDIRECT_DB never connects and drops clicks", async () => {
    const fake = fakeDb({ rows: [ROW] });
    const s = new RedirectStore(undefined, fake.connect, [{ slug: "reddit", inviteCode: "snapshotCode" }]);
    assert.equal(s.live, false);
    assert.equal((await s.lookup("reddit"))?.inviteCode, "snapshotCode");
    await s.recordClick({
      guildId: GUILD, code: "snapshotCode", campaign: "reddit",
      occurredAt: "2026-09-03T12:00:00.000Z", dedupeToken: "t", source: "invite:snapshotCode",
    });
    assert.deepEqual(fake.connects, []);
  });

  test("Worker with a bound REDIRECT_DB uses node-postgres and falls back on failure", async (t) => {
    const logs: string[] = [];
    t.mock.method(console, "error", (msg: string) => { logs.push(msg); });
    const env: Env = {
      TWO_BOT: { getByName() { throw new Error("redirect must not touch the Container"); } } as unknown as Env["TWO_BOT"],
      GUILD_ID: GUILD,
      REDIRECT_FALLBACK_CODE: FALLBACK,
      REDIRECT_MAPPINGS_JSON: JSON.stringify([{ slug: "reddit", invite_code: "snapshotCode" }]),
      // Unparseable: pg rejects it in the Client constructor, before any socket.
      REDIRECT_DB: { connectionString: "postgres://fixture-user:fixture-secret@[fixture-host/db" } as unknown as Hyperdrive,
    };
    const ctx = { waitUntil() { throw new Error("a fallback redirect must not record a click"); } } as unknown as ExecutionContext;
    const res = await worker.fetch(new Request("https://fixture.invalid/reddit"), env, ctx);
    assert.equal(res.status, 302);
    assert.equal(res.headers.get("location"), `https://discord.gg/${FALLBACK}`);
    assert.deepEqual(logs, [`invite_redirect_lookup_failed ${JSON.stringify({ slug: "reddit", errorClass: "TypeError" })}`]);
    assertNoSecrets(logs.join("\n"));
  });
});
