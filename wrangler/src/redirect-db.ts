/**
 * Postgres `ConnectFn` for the go.two.gg store, used only when the
 * `REDIRECT_DB` Hyperdrive binding exists (TOG-12194).
 *
 * node-postgres runs in the Worker under `nodejs_compat` and reaches
 * Hyperdrive through `cloudflare:sockets` (pg-cloudflare). One Client per
 * request: Hyperdrive owns the pool, and a Worker I/O object must not outlive
 * the request that created it.
 * https://developers.cloudflare.com/hyperdrive/examples/connect-to-postgres/postgres-drivers-and-libraries/node-postgres/
 *
 * RedirectStore bounds connect + query and always ends the client; the driver
 * timeouts below tear the socket down from inside pg as well.
 */

import { Client } from "pg";
import { DB_TIMEOUT_MS, type ConnectFn } from "./redirect-store.ts";

export const connectPostgres: ConnectFn = async (connectionString) => {
  const client = new Client({
    connectionString,
    connectionTimeoutMillis: DB_TIMEOUT_MS,
    query_timeout: DB_TIMEOUT_MS,
  });
  // An unhandled 'error' event is an uncaught exception, and its message can
  // name the host or user. Pending calls reject on their own; drop the event.
  client.on("error", () => undefined);
  try {
    await client.connect();
  } catch (err) {
    // pg has usually destroyed the socket already. Not awaited: end() waits
    // for a socket 'end' event that a dead connection may never emit.
    client.end().catch(() => undefined);
    throw err;
  }
  return client;
};
