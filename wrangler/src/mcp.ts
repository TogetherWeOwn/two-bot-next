/**
 * Bot HTTP MCP server (Streamable HTTP, read-only tools first).
 *
 * Stateless `POST /mcp` JSON-RPC handler on the Worker (ops host). No
 * session state, no SSE stream, no McpAgent/Durable Object session: each
 * request is an independent JSON-RPC message closed with its response.
 * The bearer gate lives in `index.ts` (before this handler runs), so every
 * call here is already authenticated with the `mcp:read` bearer.
 *
 * Phase 1 tools (all read-only). Two separate budgets share the 6 s / 64 KiB
 * shape but never each other: inbound intake collects at most 64 KiB under a
 * six-second deadline before JSON parsing (measured on bytes read, never on
 * Content-Length), while every outbound container read keeps its own 6 s
 * timeout + 64 KiB cap (same as the metrics scrape in `index.ts`):
 * - `get_status` → `GET http://c/health` (single container read)
 * - `get_gateway` → `GET http://c/readyz`, gateway excerpt only
 * - `get_backfill_progress` → `GET http://c/readyz` jobs excerpt, with an
 *   explicit `implemented: false` marker (no backfill JSON API exists yet)
 * - `get_config` → redacted non-secret Worker snapshot (no container read,
 *   never secrets)
 * - `get_audit` → `GET http://c/ops/audit`, bounded redacted audit excerpt
 *
 * Control tools are out of scope (deferred behind step-up scope + control
 * token + audit write). Unknown tool names return JSON-RPC `-32601`.
 */

import { DEPLOYMENT_HEADER, deploymentId } from "./ownership.ts";
import { withSecurityHeaders } from "./security-headers.ts";

/** Minimal view of the Worker env this handler needs. */
export interface McpEnv {
  TWO_BOT: DurableObjectNamespace;
  CF_VERSION_METADATA?: { id: string };
}

export const MCP_PROTOCOL_VERSION = "2025-06-18";
/** Protocol versions this handler implements; initialize never echoes others. */
export const MCP_SUPPORTED_PROTOCOL_VERSIONS = ["2025-06-18", "2025-03-26"];
export const MCP_SERVER_NAME = "two-bot-next-mcp";
export const MCP_SERVER_VERSION = "0.1.0";
/** Container-fetch bound for every MCP tool call (mirrors the scrape bound). */
export const MCP_FETCH_TIMEOUT_MS = 6000;
export const MCP_MAX_BODY_BYTES = 64 * 1024;
/** Inbound intake bound for authenticated request bodies (a separate budget
 * from the outbound container bound above): at most 64 KiB collected under a
 * six-second deadline before JSON parsing. */
export const MCP_INBOUND_MAX_BODY_BYTES = 64 * 1024;
export const MCP_INBOUND_BODY_TIMEOUT_MS = 6000;

// Internal binding dispatch, not a credential. Public ingress strips this
// header; only the authenticated MCP handler stamps it on its fixed GET.
export const MCP_AUDIT_READ_HEADER = "x-two-bot-mcp-audit-read";
const SINGLETON_NAME = "two-bot";

export const MCP_TOOL_NAMES = [
  "get_status",
  "get_gateway",
  "get_backfill_progress",
  "get_config",
  "get_audit",
] as const;

export type McpToolName = (typeof MCP_TOOL_NAMES)[number];

function isToolName(value: unknown): value is McpToolName {
  return (
    typeof value === "string" &&
    (MCP_TOOL_NAMES as readonly string[]).includes(value)
  );
}

const TOOL_DESCRIPTIONS: Record<McpToolName, string> = {
  get_status: "Bot status (container /health, read-only)",
  get_gateway: "Gateway connection state excerpt from /readyz (read-only)",
  get_backfill_progress:
    "Backfill jobs excerpt from /readyz (phase 1: implemented=false; no backfill JSON API yet)",
  get_config: "Redacted non-secret Worker config snapshot (read-only, never secrets)",
  get_audit: "Bounded redacted audit excerpt from /ops/audit (read-only, newest 50 rows)",
};

function toolEntries(): unknown[] {
  return MCP_TOOL_NAMES.map((name) => ({
    name,
    description: TOOL_DESCRIPTIONS[name],
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
  }));
}

/** Bound awaits even when the DO/container stream does not observe abort. */
async function abortable<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  let onAbort = () => {};
  const aborted = new Promise<never>((_, reject) => {
    onAbort = () => reject(new Error("mcp container fetch aborted"));
    signal.addEventListener("abort", onAbort, { once: true });
    if (signal.aborted) onAbort();
  });
  try {
    return await Promise.race([work, aborted]);
  } finally {
    signal.removeEventListener("abort", onAbort);
  }
}

/** Read at most `limit` bytes; oversize returns null without buffering. */
async function readBoundedText(
  response: Response,
  limit: number,
  signal?: AbortSignal,
): Promise<string | null> {
  const reader = response.body?.getReader();
  if (!reader) return "";
  const chunks: Uint8Array[] = [];
  let size = 0;
  let complete = false;
  try {
    for (;;) {
      const read = reader.read();
      const { done, value } = await (signal ? abortable(read, signal) : read);
      if (done) {
        complete = true;
        break;
      }
      size += value.byteLength;
      if (size > limit) return null;
      chunks.push(value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return new TextDecoder().decode(bytes);
  } finally {
    if (!complete) void reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

function jsonHeaders(): Record<string, string> {
  return withSecurityHeaders({
    "content-type": "application/json",
    "cache-control": "no-store",
  });
}

function jsonResult(id: unknown, result: unknown): Response {
  return Response.json({ jsonrpc: "2.0", id, result }, { status: 200, headers: jsonHeaders() });
}

function jsonError(id: unknown, code: number, message: string): Response {
  return Response.json(
    { jsonrpc: "2.0", id, error: { code, message } },
    { status: 200, headers: jsonHeaders() },
  );
}

function toolResult(id: unknown, data: unknown): Response {
  const text = typeof data === "string" ? data : JSON.stringify(data);
  const structuredContent =
    typeof data === "object" && data !== null ? data : { text };
  return jsonResult(id, {
    content: [{ type: "text", text }],
    structuredContent,
  });
}

/** One bounded container GET through the fenced singleton DO. */
async function containerGet(
  env: McpEnv,
  path: "/health" | "/readyz" | "/ops/audit",
  signal: AbortSignal,
): Promise<Response> {
  const deployment = deploymentId(
    (env as { CF_VERSION_METADATA?: { id: string } }).CF_VERSION_METADATA?.id,
  );
  const stub = (env as unknown as Record<string, unknown>).TWO_BOT as {
    getByName: (name: string) => { fetch: (r: Request) => Promise<Response> };
  };
  const upstream = stub
    .getByName(SINGLETON_NAME)
    .fetch(
      new Request(`http://c${path}`, {
        method: "GET",
        headers: {
          [DEPLOYMENT_HEADER]: deployment,
          ...(path === "/ops/audit" ? { [MCP_AUDIT_READ_HEADER]: "1" } : {}),
        },
        signal,
      }),
    );
  return abortable(upstream.then((response) => {
    if (signal.aborted) void response.body?.cancel().catch(() => {});
    return response;
  }), signal);
}

function combineSignals(request: Request, timeoutMs: number): AbortSignal {
  const timeout = AbortSignal.timeout(timeoutMs);
  const client = request.signal;
  if (!client) return timeout;
  const anySignal = (AbortSignal as unknown as {
    any?: (signals: AbortSignal[]) => AbortSignal;
  }).any;
  if (typeof anySignal !== "function") return timeout;
  try {
    return anySignal.call(AbortSignal, [client, timeout]);
  } catch {
    return timeout;
  }
}

/** The bot's /readyz answers 200 when ready and 503 JSON when any component
 * is not ready (see crates/bot/src/server.rs). Both carry the full report;
 * only bot-shaped responses may be parsed, mirroring isBotProbeResponse in
 * index.ts. Other statuses (SDK-synthesized text, 429/500) stay failures. */
function isReadyzBotResponse(status: number, contentType: string | null): boolean {
  if (status === 200) return true;
  return (
    status === 503 && contentType?.split(";")[0]?.trim().toLowerCase() === "application/json"
  );
}

/** A parsed /readyz body is only a readiness report when it carries the
 * HealthReport shape (components is an array of [name, status] pairs). The
 * Worker's ownership/storage fence also answers 503 application/json with
 * `{error:"ownership_fenced"}` (see ownership.ts refused()); that body must
 * stay a generic failure, never an empty gateway/jobs success. */
function isReadyzReport(data: unknown): boolean {
  return (
    typeof data === "object" &&
    data !== null &&
    Array.isArray((data as Record<string, unknown>)["components"])
  );
}

/** Reject schema drift rather than accidentally proxying private audit columns. */
function isAuditExcerpt(data: unknown): boolean {
  if (typeof data !== "object" || data === null || Array.isArray(data)) return false;
  const excerpt = data as Record<string, unknown>;
  if (Object.keys(excerpt).sort().join(",") !== "available,rows,truncated" ||
      typeof excerpt.available !== "boolean" || typeof excerpt.truncated !== "boolean" ||
      !Array.isArray(excerpt.rows) || excerpt.rows.length > 50) return false;
  if (!excerpt.available && (excerpt.rows.length !== 0 || excerpt.truncated)) return false;
  const keys = [
    "action", "actor_id", "delivery_state", "destination_channel_id", "entry_id",
    "guild_id", "kind", "message_id", "occurred_at", "source_channel_id", "target_id",
  ].join(",");
  return excerpt.rows.every((value: unknown) => {
    if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
    const row = value as Record<string, unknown>;
    if (Object.keys(row).sort().join(",") !== keys) return false;
    const nullableIds = ["actor_id", "target_id", "source_channel_id", "destination_channel_id", "message_id"];
    const snowflake = (field: unknown) => typeof field === "string" && /^[0-9]{1,20}$/.test(field);
    return typeof row.entry_id === "string" && /^[A-Za-z0-9_:.-]{1,128}$/.test(row.entry_id)
      && snowflake(row.guild_id)
      && nullableIds.every((key) => row[key] === null || snowflake(row[key]))
      && typeof row.occurred_at === "string" && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(row.occurred_at)
      && typeof row.kind === "string" && ["message_edit", "message_delete", "member_update", "voice_join", "voice_leave", "voice_move", "moderation_action"].includes(row.kind)
      && typeof row.delivery_state === "string" && ["none", "pending", "delivering", "delivered", "quarantined"].includes(row.delivery_state)
      && (row.action === null || (typeof row.action === "string" && [
        "member_kick", "member_prune", "member_ban", "member_unban", "member_update",
        "member_role_update", "member_move", "member_disconnect", "message_delete",
        "message_bulk_delete", "channel_update", "channel_overwrite_create",
        "channel_overwrite_update", "channel_overwrite_delete", "moderation.ban",
        "moderation.tempban", "moderation.kick", "moderation.timeout", "moderation.warn",
        "moderation.purge", "moderation.slowmode", "moderation.lockdown", "moderation.unlock",
        "moderation.unban_scheduled",
      ].includes(row.action)));
  });
}

async function readContainerJson(
  env: McpEnv,
  path: "/health" | "/readyz" | "/ops/audit",
  signal: AbortSignal,
): Promise<{ ok: true; data: unknown } | { ok: false; oversize: boolean }> {
  let upstream: Response;
  try {
    upstream = await containerGet(env, path, signal);
  } catch {
    return { ok: false, oversize: false };
  }
  const contentType = upstream.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase();
  const botResponse = path === "/ops/audit"
    ? upstream.status === 200 && contentType === "application/json"
    : path === "/health" ? upstream.ok : isReadyzBotResponse(upstream.status, contentType ?? null);
  if (signal.aborted || !botResponse) {
    void upstream.body?.cancel().catch(() => {});
    return { ok: false, oversize: false };
  }
  let body: string | null;
  try {
    body = await readBoundedText(upstream, MCP_MAX_BODY_BYTES, signal);
  } catch {
    return { ok: false, oversize: false };
  }
  if (body === null || signal.aborted) {
    // Oversize (null) or a timeout that won the race during the read: never
    // echo upstream bytes, just report the class.
    void upstream.body?.cancel().catch(() => {});
    return { ok: false, oversize: body === null };
  }
  try {
    const data = JSON.parse(body) as unknown;
    if ((path === "/readyz" && !isReadyzReport(data)) ||
        (path === "/ops/audit" && !isAuditExcerpt(data))) {
      return { ok: false, oversize: false };
    }
    return { ok: true, data };
  } catch {
    if (path !== "/health") {
      // Non-JSON bodies are not readiness reports or audit excerpts.
      return { ok: false, oversize: false };
    }
    return { ok: true, data: { text: body } };
  }
}

function gatewayExcerpt(readyz: unknown): Record<string, unknown> {
  if (typeof readyz !== "object" || readyz === null) return {};
  const report = readyz as Record<string, unknown>;
  const out: Record<string, unknown> = {};
  if ("gateway_failure" in report) out["gateway_failure"] = report["gateway_failure"];
  const components = report["components"];
  // HealthReport.components is Vec<(String, ComponentStatus)>, serialized as
  // [["process","ready"],["gateway","down"],...] (crates/core/src/health.rs).
  if (Array.isArray(components)) {
    for (const entry of components) {
      if (Array.isArray(entry) && entry[0] === "gateway") {
        out["gateway"] = entry[1];
        break;
      }
    }
  } else if (typeof components === "object" && components !== null) {
    const gateway = (components as Record<string, unknown>)["gateway"];
    if (gateway !== undefined) out["gateway"] = gateway;
  }
  return out;
}

function jobsExcerpt(readyz: unknown): Record<string, unknown> {
  if (typeof readyz !== "object" || readyz === null) return {};
  const jobs = (readyz as Record<string, unknown>)["jobs"];
  if (typeof jobs === "object" && jobs !== null) {
    return jobs as Record<string, unknown>;
  }
  return {};
}

async function callTool(
  env: McpEnv,
  request: Request,
  id: unknown,
  name: McpToolName,
  fullEnv: Record<string, string | undefined>,
): Promise<Response> {
  const signal = combineSignals(request, MCP_FETCH_TIMEOUT_MS);
  switch (name) {
    case "get_status": {
      const res = await readContainerJson(env, "/health", signal);
      if (!res.ok) {
        return jsonError(
          id,
          -32000,
          res.oversize ? "container body exceeds limit" : "container request failed",
        );
      }
      return toolResult(id, res.data);
    }
    case "get_gateway": {
      const res = await readContainerJson(env, "/readyz", signal);
      if (!res.ok) {
        return jsonError(
          id,
          -32000,
          res.oversize ? "container body exceeds limit" : "container request failed",
        );
      }
      return toolResult(id, gatewayExcerpt(res.data));
    }
    case "get_backfill_progress": {
      const res = await readContainerJson(env, "/readyz", signal);
      if (!res.ok) {
        return jsonError(
          id,
          -32000,
          res.oversize ? "container body exceeds limit" : "container request failed",
        );
      }
      // No backfill JSON API exists yet: pin the explicit stub shape around
      // the /readyz jobs excerpt instead of faking progress data.
      return toolResult(id, {
        implemented: false,
        source: "readyz.jobs",
        jobs: jobsExcerpt(res.data),
      });
    }
    case "get_config": {
      // Worker-side redacted snapshot only: build identity plus non-secret
      // routing/keepalive vars. Secrets are never read here by construction.
      // CF_VERSION_METADATA.id is the Worker version (already on McpEnv);
      // the /mcp route only forwards BOT_PORT/KEEPALIVE_SECONDS, so no
      // CF_REVISION key exists in fullEnv.
      return toolResult(id, {
        build_revision: env.CF_VERSION_METADATA?.id ?? fullEnv["CF_REVISION"] ?? "unknown",
        bot_port: fullEnv["BOT_PORT"] ?? "8080",
        keepalive_seconds: fullEnv["KEEPALIVE_SECONDS"] ?? "60",
      });
    }
    case "get_audit": {
      const res = await readContainerJson(env, "/ops/audit", signal);
      if (!res.ok) {
        return jsonError(
          id,
          -32000,
          res.oversize ? "container body exceeds limit" : "container request failed",
        );
      }
      return toolResult(id, res.data);
    }
  }
}

/**
 * Collect at most `limit` inbound bytes under `timeoutMs` before parsing.
 * The count is measured on the bytes actually read, never on Content-Length.
 * Oversize, deadline, client-abort and stream errors all resolve `ok: false`;
 * the reader is cancelled and released so the tail is never drained.
 * Decoding happens once, after a complete read, so a multi-byte character
 * split across chunks still parses.
 */
async function readInboundText(
  request: Request,
  limit: number,
  timeoutMs: number,
): Promise<{ ok: true; text: string } | { ok: false }> {
  const stream = request.body;
  if (!stream) return { ok: true, text: "" };
  const clientSignal = request.signal;
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  // The deadline and the client abort share one mechanism: cancelling the
  // reader settles the pending read as done, and the flags below tell that
  // apart from a genuine end of body. Cancelling directly (instead of racing
  // the read against timer/abort promises) keeps each read to a single await
  // hop, so an oversize stream is stopped before its tail is pulled.
  let expired = false;
  const expire = () => {
    expired = true;
    reader.cancel().catch(() => {});
  };
  const timer = setTimeout(expire, timeoutMs);
  const onAbort = () => {
    expired = true;
    reader.cancel().catch(() => {});
  };
  clientSignal?.addEventListener("abort", onAbort, { once: true });
  if (clientSignal?.aborted) {
    clearTimeout(timer);
    clientSignal.removeEventListener("abort", onAbort);
    try {
      await reader.cancel();
    } catch {
      // Reader already closed or errored; the refusal is the same.
    }
    reader.releaseLock();
    return { ok: false };
  }
  try {
    for (;;) {
      let next: ReadableStreamReadResult<Uint8Array>;
      try {
        next = await reader.read();
      } catch {
        // Stream error mid-body: same fixed refusal, never an echo.
        try {
          await reader.cancel();
        } catch {
          // Reader already closed or errored; the refusal is the same.
        }
        return { ok: false };
      }
      if (expired || clientSignal?.aborted) {
        try {
          await reader.cancel();
        } catch {
          // Cancellation itself failing changes nothing about the refusal.
        }
        return { ok: false };
      }
      if (next.done) break;
      size += next.value.byteLength;
      if (size > limit) {
        try {
          await reader.cancel();
        } catch {
          // Cancellation itself failing changes nothing about the refusal.
        }
        return { ok: false };
      }
      chunks.push(next.value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return { ok: true, text: new TextDecoder().decode(bytes) };
  } finally {
    clearTimeout(timer);
    clientSignal?.removeEventListener("abort", onAbort);
    reader.releaseLock();
  }
}

/**
 * Stateless per-request JSON-RPC over POST. The caller is already
 * authenticated; this function never touches auth state.
 */
export async function handleMcp(
  request: Request,
  env: McpEnv,
  fullEnv?: Record<string, string | undefined>,
): Promise<Response> {
  const intake = await readInboundText(
    request,
    MCP_INBOUND_MAX_BODY_BYTES,
    MCP_INBOUND_BODY_TIMEOUT_MS,
  );
  if (!intake.ok) {
    return jsonError(null, -32700, "parse error");
  }
  let body: unknown;
  try {
    body = JSON.parse(intake.text) as unknown;
  } catch {
    return jsonError(null, -32700, "parse error");
  }
  if (typeof body !== "object" || body === null) {
    return jsonError(null, -32600, "invalid request");
  }
  const msg = body as Record<string, unknown>;
  if (msg["jsonrpc"] !== "2.0" || typeof msg["method"] !== "string") {
    return jsonError(
      (msg as { id?: unknown })["id"] ?? null,
      -32600,
      "invalid request",
    );
  }
  const method = msg["method"] as string;
  const id = ("id" in msg ? msg["id"] : undefined) as unknown;
  const params = (msg["params"] ?? {}) as Record<string, unknown>;

  // JSON-RPC notifications (no id) get a bare 202, never a result body.
  if (id === undefined || id === null) {
    if (method.startsWith("notifications/")) {
      return new Response(null, { status: 202, headers: withSecurityHeaders() });
    }
    return jsonError(null, -32600, "invalid request");
  }

  switch (method) {
    case "initialize": {
      // MCP spec: echo the client's version only when the server supports
      // it, else answer with a supported version. Unknown versions (e.g.
      // "1999-01-01") must not be echoed back as negotiated.
      const requested =
        typeof params["protocolVersion"] === "string"
          ? (params["protocolVersion"] as string)
          : "";
      const protocolVersion = MCP_SUPPORTED_PROTOCOL_VERSIONS.includes(requested)
        ? requested
        : MCP_PROTOCOL_VERSION;
      return jsonResult(id, {
        protocolVersion,
        capabilities: { tools: {} },
        serverInfo: { name: MCP_SERVER_NAME, version: MCP_SERVER_VERSION },
      });
    }
    case "tools/list": {
      return jsonResult(id, { tools: toolEntries() });
    }
    case "tools/call": {
      const name = params["name"];
      if (!isToolName(name)) {
        return jsonError(id, -32601, "unknown tool");
      }
      const snapshot: Record<string, string | undefined> = {};
      if (fullEnv) {
        for (const [k, v] of Object.entries(fullEnv)) snapshot[k] = v;
      }
      return callTool(env, request, id, name, snapshot);
    }
    case "ping": {
      return jsonResult(id, {});
    }
    default: {
      if (method.startsWith("notifications/")) {
        return new Response(null, { status: 202, headers: withSecurityHeaders() });
      }
      return jsonError(id, -32601, "method not found");
    }
  }
}
