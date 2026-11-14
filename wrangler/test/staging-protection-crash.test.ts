// Run by the hosted Worker Actions job, not as a signal experiment on the shared controller.
// Kill only a newly spawned, detached fixture process group (Node + workerd), with no graceful
// disposal. Restart over its unique disk directory; never manually fire/re-arm the restored alarm.
// This proves local workerd disk/automatic-alarm recovery, not Cloudflare's hosted storage service
// or a live GitHub decision. The real verifier deliberately rejects missing producer evidence.
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { fork } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const within = async <T>(promise: Promise<T>, ms: number, label: string): Promise<T> => {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} exceeded ${ms}ms`)), ms);
    })]);
  } finally { clearTimeout(timer); }
};
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
async function until(check: () => boolean | Promise<boolean>, ms: number, label: string) {
  const end = Date.now() + ms;
  while (Date.now() < end) { if (await check()) return; await pause(100); }
  throw new Error(`timed out waiting for ${label}`);
}
type Receipt = { event?: string; id?: number; value?: any; error?: string; method?: string; path?: string; body?: any };
type State = { queued?: { delivery: string }; fence?: { state: string }; alarm: number | null };

async function ownedStorage(t: TestContext) {
  const path = await mkdtemp(join(tmpdir(), "staging-protection-crash-"));
  const cleanups: (() => Promise<void>)[] = [];
  t.after(async () => {
    // Never remove SQLite files while a restarted fixture still owns them.
    for (const cleanup of cleanups.toReversed()) await cleanup();
    await rm(path, { recursive: true, force: true });
  });
  return { path, cleanups };
}
async function start(storage: Awaited<ReturnType<typeof ownedStorage>>, mode = "normal") {
  const child = fork(new URL("./fixtures/staging-protection-process.ts", import.meta.url), [storage.path, mode], {
    detached: true, stdio: ["ignore", "pipe", "pipe", "ipc"],
    execArgv: ["--import", new URL("./cloudflare-loader.mjs", import.meta.url).href],
  });
  assert.ok(child.pid && child.pid > 1);
  const pid = child.pid;
  const receipts: Receipt[] = [];
  let sequence = 0, exited = false, output = "";
  child.stdout!.on("data", bytes => { output = (output + bytes.toString()).slice(-8000); });
  child.stderr!.on("data", bytes => { output = (output + bytes.toString()).slice(-8000); });
  child.on("message", message => receipts.push(message as Receipt));
  const exit = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => { exited = true; resolve({ code, signal }); });
  });
  const crash = async () => {
    assert.equal(exited, false, "only kill the owned, still-live fixture");
    // Node's process.kill accepts a numeric PGID directly, with no shell option ambiguity.
    process.kill(-pid, "SIGKILL");
    assert.deepEqual(await within(exit, 10000, "fixture crash"), { code: null, signal: "SIGKILL" });
  };
  async function call(action: string) {
    const id = ++sequence;
    child.send({ id, action });
    await until(() => {
      if (receipts.some(message => message.id === id)) return true;
      if (exited) throw new Error(`fixture exited during ${action}: ${output}`);
      return false;
    }, 15000, action);
    const response = receipts.find(message => message.id === id)!;
    assert.equal(response.error, undefined, output);
    return response.value;
  }
  storage.cleanups.push(async () => {
    if (!exited) {
      try {
        assert.equal(await call("dispose"), "disposed");
        assert.deepEqual(await within(exit, 10000, "fixture disposal"), { code: 0, signal: null });
      } finally { if (!exited) await crash(); }
    }
  });
  await until(() => {
    if (exited) throw new Error(`fixture exited before ready: ${output}`);
    return receipts.some(message => message.event === "ready");
  }, 15000, "fixture startup");
  const state = () => call("state") as Promise<State>;
  const tokens = () => receipts.filter(message => message.event === "outbound" && message.path === "/app/installations/2/access_tokens");
  const decisions = () => receipts.filter(message => message.event === "decision");
  const revocations = () => receipts.filter(message => message.event === "outbound" && message.method === "DELETE");
  return { call, crash, state, tokens, decisions, revocations };
}

test("workerd disk crash: acknowledged inbox and original alarm recover without a new delivery", { timeout: 60000 }, async t => {
  const path = await ownedStorage(t);
  const first = await start(path);
  assert.equal(await first.call("defer"), 200);
  assert.deepEqual(await first.call("enqueue"), { status: 202, body: "accepted" });
  const before = await first.state();
  assert.ok(before.queued?.delivery);
  assert.ok(before.alarm! > Date.now(), "alarm committed before killing the process");
  assert.equal(first.tokens().length, 0);
  await first.crash();

  const restored = await start(path);
  // No state/ingress access before this: the persisted alarm must instantiate the cold DO.
  await until(() => restored.decisions().length === 1, 25000, "automatic restored alarm decision");
  await until(async () => !(await restored.state()).queued, 10000, "restored queue drain");
  assert.equal(restored.tokens().length, 1);
  assert.equal(restored.revocations().length, 1);
  assert.equal(restored.decisions()[0]!.body.state, "rejected", "missing evidence never approves");
  const after = await restored.state();
  assert.equal(after.fence?.state, "rejected");
  assert.ok(after.alarm! > Date.now() + 24 * 3600000, "normal retention alarm follows recovery");
});

test("workerd disk crash: uncertain callback fence survives and automatic recovery never repeats it", { timeout: 120000 }, async t => {
  const path = await ownedStorage(t);
  const first = await start(path, "hold-callback");
  assert.deepEqual(await first.call("enqueue"), { status: 202, body: "accepted" });
  await until(() => first.decisions().length === 1, 15000, "callback sent with response held");
  assert.equal(first.decisions()[0]!.body.state, "rejected");
  const before = await first.state();
  assert.ok(before.queued?.delivery);
  assert.equal(before.fence?.state, "rejecting", "write-ahead fence persisted before outbound callback");
  assert.ok(before.alarm! > Date.now(), "recovery alarm persists during uncertain I/O");
  await first.crash();

  const restored = await start(path);
  // Wait for the production one-minute recovery alarm, without touching the DO or re-arming it.
  await until(() => restored.revocations().length === 1, 75000, "automatic recovery honors uncertain fence");
  await until(async () => !(await restored.state()).queued, 10000, "uncertain queue drain");
  assert.equal(restored.tokens().length, 1);
  assert.equal(restored.decisions().length, 0, "no repeated callback after process crash");
  assert.equal((await restored.state()).fence?.state, "rejecting");

  assert.deepEqual(await restored.call("enqueue"), { status: 202, body: "accepted" });
  await until(() => restored.revocations().length === 2, 15000, "redelivery evaluation");
  await until(async () => !(await restored.state()).queued, 10000, "redelivery drain");
  assert.equal(restored.tokens().length, 2);
  assert.equal(restored.decisions().length, 0, "redelivery cannot erase the uncertain fence");
  assert.equal((await restored.state()).fence?.state, "rejecting");
});
