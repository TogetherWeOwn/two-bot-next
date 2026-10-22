/** Protected offline browser fixture generator, not a production server.
 * Run from wrangler with the Cloudflare loader and a scratch .html output path.
 * Open the resulting file (or its sibling .url data URL) in a real browser.
 * All fetches/time are intercepted BEFORE the unchanged served script runs.
 * The fixture adds hashes for its two test scripts to a meta CSP; the acceptance
 * test independently checks the original response's exact strict CSP hashes.
 */
import { writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { gzipSync } from "node:zlib";
import { installLifecycleHarness, servedLifecyclePage } from "./ops-dashboard-lifecycle-harness.ts";

async function browserLifecycle() {
  const h = (globalThis as unknown as { __lifecycle: ReturnType<typeof installLifecycleHarness> }).__lifecycle;
  const results: { name: string; pass: boolean; status: string; heading: string; color: string }[] = [];
  const byId = (id: string) => document.getElementById(id)!;
  const summary = () => byId("summary").textContent ?? "";
  const snapshot = () => `${byId("status").textContent}\n${byId("summary").innerHTML}`;
  const notFresh = () => /unavailable|stale|expired|disconnect|timeout|timed?\s*out/i.test(summary()) &&
    !byId("summary").querySelector(".ok") && !/>\s*READY\s*</i.test(byId("summary").innerHTML);
  const record = (name: string, pass: boolean) => {
    const heading = byId("summary").querySelector("h2");
    results.push({ name, pass: Boolean(pass), status: byId("status").textContent ?? "",
      heading: heading?.textContent ?? "", color: heading ? getComputedStyle(heading).color : "none" });
  };
  const credentials = () => {
    (byId("clientId") as HTMLInputElement).value = "synthetic-offline-browser-id";
    (byId("clientSecret") as HTMLInputElement).value = "synthetic-offline-browser-secret";
  };
  const click = async (id: string) => { byId(id).click(); await h.flush(); };
  const ready = (marker: string) => ({ components: [["process", "ready"]], jobs: {}, build_revision: marker, build_id: "fixture" });
  try {
    credentials();
    await click("connect");
    h.requests[0]!.reply(ready("browser-first"));
    await h.flush();
    record("fresh READY control", summary().includes("READY") && summary().includes("browser-first"));
    await h.tick();
    const old = h.requests.at(-1)!;
    await h.advance(60_000);
    record("hung headers retire green readiness", Boolean(notFresh()));
    const timedOut = snapshot();
    old.reply(ready("forbidden-browser-late"));
    await h.flush();
    record("late timeout completion is fenced", snapshot() === timedOut);
    await click("disconnect");
    record("disconnect visibly retires READY", Boolean(notFresh()));
    record("disconnect clears credential inputs", !(byId("clientId") as HTMLInputElement).value && !(byId("clientSecret") as HTMLInputElement).value);
    credentials();
    await click("connect");
    const body = h.requests.at(-1)!;
    body.headers();
    await h.flush();
    await h.advance(60_000);
    record("hung body is bounded", Boolean(notFresh()) && Boolean(body.init.signal?.aborted));
    await click("disconnect");
    credentials();
    await click("connect");
    h.requests.at(-1)!.reply(ready("browser-recovered"));
    await h.flush();
    record("recovery returns fresh READY", summary().includes("browser-recovered") && Boolean(byId("summary").querySelector(".ok")));
    const recovered = snapshot();
    body.body(ready("obsolete-browser-body"));
    await h.flush();
    record("obsolete body cannot overwrite recovery", snapshot() === recovered);
    let peak = 0;
    for (let i = 0; i < 100; i++) { await h.tick(); peak = Math.max(peak, h.outstanding()); }
    record("100 ticks keep transport bounded", peak <= 2);
    await click("disconnect");
    record("disconnect releases timers and transport", h.timeouts.size === 0 && h.intervals.size === 0 && h.outstanding() === 0);
    record("credentials are never stored or logged", h.storageWrites.length === 0 && !h.logs.join(" ").includes("synthetic-offline-browser-secret"));
  } catch (error) {
    record(`fixture error: ${String(error)}`, false);
  }
  const report = document.createElement("pre");
  report.id = "acceptance-results";
  report.textContent = JSON.stringify({ passed: results.filter((r) => r.pass).length, total: results.length, results }, null, 2);
  document.body.append(report);
  document.title = `Offline lifecycle: ${results.filter((r) => r.pass).length}/${results.length}`;
}

const output = process.argv[2];
if (!output?.endsWith(".html")) throw new Error("pass a run-owned scratch .html output path");
const { html, csp } = await servedLifecyclePage();
if (!csp) throw new Error("served dashboard has no CSP");
const bootstrap = `globalThis.__lifecycle = (${installLifecycleHarness.toString()})(globalThis, false);`;
const drive = `(${browserLifecycle.toString()})();`;
const hash = (script: string) => `'sha256-${createHash("sha256").update(script).digest("base64")}'`;
const fixtureCsp = csp.replace(/frame-ancestors\s+[^;]+;?\s*/i, "")
  .replace(/(script-src\s+[^;]+)/, `$1 ${hash(bootstrap)} ${hash(drive)}`);
const fixture = html.replace(/<head>/i, `<head><meta http-equiv="Content-Security-Policy" content="${fixtureCsp}">`)
  .replace("<script>", `<script>${bootstrap}</script><script>`)
  .replace(/<\/body>/i, `<script>${drive}</script></body>`);
await writeFile(output, fixture);
// Compress only the transport envelope, never the served script's bytes.
const compressed = gzipSync(fixture).toString("base64");
const loader = `<script>(async()=>{const bytes=Uint8Array.from(atob("${compressed}"),c=>c.charCodeAt(0));const html=await new Response(new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"))).text();document.open();document.write(html);document.close()})()</script>`;
await writeFile(output.replace(/\.html$/, ".url"), `data:text/html,${encodeURIComponent(loader)}`);
console.log(JSON.stringify({ fixture: output, bytes: Buffer.byteLength(fixture), dataUrl: output.replace(/\.html$/, ".url") }));
