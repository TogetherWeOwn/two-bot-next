import { control } from './production-ownership-control.mjs';
import { isDirectInvocation } from './is-direct-invocation.mjs';

const MAX_BODY_BYTES = 4096;

export function readStatus({ url, stagingUrl, token }, send = fetch) {
  // The workflow binds the version after the GET so it can observe running containers.
  return control({ action: 'status', url, productionUrl: url, stagingUrl, token, statusAttempts: 1 },
    async (endpoint, options) => {
      const response = await send(endpoint, options);
      if (!response.ok) return response;
      if (response.status !== 200) throw new Error('unexpected ownership response status');
      return {
        ok: true,
        async json() {
          const chunks = [];
          let bytes = 0;
          for await (const chunk of response.body) {
            bytes += chunk.byteLength;
            if (bytes > MAX_BODY_BYTES) throw new Error('ownership response too large');
            chunks.push(chunk);
          }
          return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks)));
        },
      };
    });
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  await readStatus({ url: process.env.PRODUCTION_WORKER_URL,
    stagingUrl: process.env.STAGING_WORKER_URL, token: process.env.OWNERSHIP_CONTROL_TOKEN })
    .then((state) => console.log(JSON.stringify(state)))
    .catch(() => {
      console.error('Production ownership GET refused');
      process.exitCode = 1;
    });
}
