import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";
import { boundRead, releaseUpstream } from "./internal-actions.ts";
import { readControlBody } from "./ownership.ts";

interface Env {
  TWO_BOT: DurableObjectNamespace<PreQualificationContainer>;
  CF_VERSION_METADATA: WorkerVersionMetadata;
  PRE_START_ID: string;
  PRE_CUTOFF_MS: string;
  PRE_SHAPE: string;
  PRE_IMAGE: string;
}

interface Actor {
  startId: string;
  version: string;
  objectId: string;
}

type Phase = "armed" | "issued" | "closing" | "stop-requested" | "never-issued";
interface Fence {
  actor: Actor;
  cutoff: number;
  phase: Phase;
}

const UUID = /^(?!00000000-0000-0000-0000-000000000000$)[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
const IMAGE = /^registry\.cloudflare\.com\/[0-9a-f]{32}\/two-bot-next-pre-[0-9]{1,20}-[0-9]{1,10}-c@sha256:[0-9a-f]{64}$/;
const FENCE = "pre-fence";
const MAX_LEASE_MS = 3_600_000;
const STOP_RETRY_MS = 15_000;
const PROBE_TIMEOUT_MS = 10_000;
const PROBE_BODY_CAP = 64 << 10;

function refused(): Error {
  return new Error("pre_native_lifecycle_unconfirmed");
}

function binding(env: Env) {
  const startId = env.PRE_START_ID;
  const version = env.CF_VERSION_METADATA?.id;
  if (typeof startId !== "string" || typeof version !== "string" || !UUID.test(startId) || !UUID.test(version)) {
    throw refused();
  }
  const id = env.TWO_BOT.idFromName(startId);
  return { actor: { startId, version, objectId: id.toString() }, id };
}

function sameActor(actual: Actor, expected: Actor): boolean {
  return actual?.startId === expected.startId && actual.version === expected.version
    && actual.objectId === expected.objectId;
}

// This Worker exposes only named private Service Binding entrypoints.
export default { fetch: () => new Response(null, { status: 404 }) };

export class PreQualification extends WorkerEntrypoint<Env> {
  identity() {
    return binding(this.env).actor;
  }

  start(expected: Actor) {
    const { id } = binding(this.env);
    return this.env.TWO_BOT.get(id).start(expected);
  }

  probe(expected: Actor, path: string) {
    const { id } = binding(this.env);
    return this.env.TWO_BOT.get(id).probe(expected, path);
  }
}

export class PreStop extends WorkerEntrypoint<Env> {
  arm(expected: Actor) {
    const { id } = binding(this.env);
    return this.env.TWO_BOT.get(id).arm(expected);
  }

  stop(expected: Actor) {
    const { id } = binding(this.env);
    return this.env.TWO_BOT.get(id).stop(expected);
  }

  status(expected: Actor) {
    const { id } = binding(this.env);
    return this.env.TWO_BOT.get(id).status(expected);
  }
}

export class PreQualificationContainer extends DurableObject<Env> {
  #configuration(expected: Actor) {
    const { actor } = binding(this.env);
    const cutoff = Number(this.env.PRE_CUTOFF_MS);
    if (actor.objectId !== this.ctx.id.toString() || !sameActor(expected, actor)
        || !Number.isSafeInteger(cutoff) || cutoff <= 0 || String(cutoff) !== this.env.PRE_CUTOFF_MS) {
      throw refused();
    }
    return { actor, cutoff };
  }

  async #read(actor: Actor, cutoff: number) {
    const fence = await this.ctx.storage.get<Fence>(FENCE);
    if (fence && (!sameActor(fence.actor, actor) || fence.cutoff !== cutoff)) throw refused();
    return fence;
  }

  async arm(expected: Actor) {
    const { actor, cutoff } = this.#configuration(expected);
    const armed = await this.ctx.blockConcurrencyWhile(async () => {
      if (await this.#read(actor, cutoff)) return false;
      const now = Date.now();
      if (cutoff <= now || cutoff - now > MAX_LEASE_MS) return false;
      await this.ctx.storage.transaction(async txn => {
        await txn.setAlarm(cutoff);
        await txn.put<Fence>(FENCE, { actor, cutoff, phase: "armed" });
      });
      await this.ctx.storage.sync();
      return Date.now() < cutoff;
    });
    if (!armed) throw refused();
    return { actor, cutoff };
  }

  async start(expected: Actor) {
    const { actor, cutoff } = this.#configuration(expected);
    const shape = this.env.PRE_SHAPE;
    const container = this.ctx.container;
    const image = container?.images?.fixture;
    if (!container || typeof image !== "string" || Object.keys(container.images).length !== 1
        || !IMAGE.test(image) || image !== this.env.PRE_IMAGE
        || (shape !== "0473/0465" && shape !== "0475/0465")) throw refused();
    const issued = await this.ctx.blockConcurrencyWhile(async () => {
      if ((await this.#read(actor, cutoff))?.phase !== "armed" || Date.now() >= cutoff) return false;
      // Persist intent before start(), which can enqueue a placement and then
      // lose its reply. Even a non-running container then needs explicit stop.
      await this.ctx.storage.put<Fence>(FENCE, { actor, cutoff, phase: "issued" });
      await this.ctx.storage.sync();
      if (Date.now() >= cutoff) return false;
      try {
        container.start({
          image, enableInternet: false,
          entrypoint: ["/opt/pre/run"],
          env: {
            PRE_SHAPE: shape, PRE_START_ID: actor.startId, PRE_CUTOFF_MS: String(cutoff),
            DISCORD_GATEWAY_URL: "ws://127.0.0.1:8081", DISCORD_API_BASE: "http://127.0.0.1:8081",
          },
          labels: { pre_start_id: actor.startId },
        });
        await container.setInactivityTimeout(Math.max(1, cutoff - Date.now()));
      } catch {
        return false;
      }
      return true;
    });
    if (!issued) throw refused();
    return actor;
  }

  async probe(expected: Actor, path: string) {
    if (path !== "/health" && path !== "/readyz" && path !== "/version") throw refused();
    const { actor, cutoff } = this.#configuration(expected);
    const container = this.ctx.container;
    if (!container || (await this.#read(actor, cutoff))?.phase !== "issued" || Date.now() >= cutoff) throw refused();
    const signal = AbortSignal.timeout(Math.min(PROBE_TIMEOUT_MS, Math.max(0, cutoff - Date.now())));
    try {
      const running = await boundRead(container.inspect(), signal);
      if (!running || running.image !== this.env.PRE_IMAGE || running.labels?.pre_start_id !== actor.startId
          || (await this.#read(actor, cutoff))?.phase !== "issued" || Date.now() >= cutoff) throw refused();
      const request = container.getTcpPort(8080).fetch(`http://127.0.0.1:8080${path}`, { redirect: "manual", signal });
      const reply = await boundRead(request.then(response => {
        if (signal.aborted || (response.status >= 300 && response.status < 400)) {
          releaseUpstream(response);
          throw refused();
        }
        return response;
      }), signal);
      const body = await readControlBody(reply, bytes => new Blob([bytes]), PROBE_BODY_CAP, signal);
      if (!(body instanceof Blob) || (await this.#read(actor, cutoff))?.phase !== "issued"
          || Date.now() >= cutoff) throw refused();
      return new Response(body, {
        status: reply.status,
        headers: { "content-type": "application/json", "x-two-worker-version": actor.version },
      });
    } catch {
      throw refused();
    }
  }

  async status(expected: Actor) {
    const fence = await this.ctx.storage.get<Fence>(FENCE);
    const actor = fence?.actor ?? this.#configuration(expected).actor;
    if (actor.objectId !== this.ctx.id.toString() || !sameActor(expected, actor)) throw refused();
    return fence ?? null;
  }

  stop(expected: Actor) {
    if (!expected) throw refused();
    return this.#close(expected);
  }

  async alarm() {
    await this.#close();
  }

  async #close(expected?: Actor) {
    const result = await this.ctx.blockConcurrencyWhile(async () => {
      const current = await this.ctx.storage.get<Fence>(FENCE);
      // An alarm keeps custody of the stored actor even after a Worker version change.
      if (current && (current.actor.objectId !== this.ctx.id.toString()
          || (expected !== undefined && !sameActor(expected, current.actor)))) return undefined;
      if (current?.phase === "stop-requested" || current?.phase === "never-issued") return current;
      const config = current ?? (expected === undefined ? undefined : this.#configuration(expected));
      if (!config) return undefined;
      const { actor, cutoff } = config;
      if (expected === undefined && current?.phase !== "closing" && Date.now() < cutoff) {
        await this.ctx.storage.setAlarm(cutoff);
        return current;
      }
      const issued = current?.phase === "issued" || current?.phase === "closing";
      const closed: Fence = { actor, cutoff, phase: issued ? "closing" : "never-issued" };
      await this.ctx.storage.transaction(async txn => {
        await txn.put(FENCE, closed);
        if (issued) await txn.setAlarm(Date.now() + STOP_RETRY_MS);
        else await txn.deleteAlarm();
      });
      await this.ctx.storage.sync();
      if (!issued) return closed;
      try {
        const container = this.ctx.container;
        if (!container) throw refused();
        // A snapshot of running=false can race a pending placement.
        await container.destroy();
        if (await container.inspect() !== null) throw refused();
      } catch {
        return expected === undefined ? closed : undefined;
      }
      // Local stop readback leaves provider placement settlement to handback.
      const stopped: Fence = { ...closed, phase: "stop-requested" };
      await this.ctx.storage.transaction(async txn => {
        await txn.put(FENCE, stopped);
        await txn.deleteAlarm();
      });
      return stopped;
    });
    if (!result) throw refused();
    return result;
  }
}
