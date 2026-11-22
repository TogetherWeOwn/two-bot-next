import { resolve } from "node:path";

// Emit counts only. Node's file wrapper can pass without running a test.
export default async function* mutationReporter(events) {
  let pass = 0;
  let fail = 0;
  for await (const { type, data } of events) {
    if ((type === "test:pass" || type === "test:fail") && data.details.type === "test" &&
        data.line > 0 && !data.skip && !data.todo) {
      const fileWrapper = data.line === 1 && data.column === 1 && resolve(data.name) === data.file;
      if (!fileWrapper && type === "test:pass") pass++;
      if (!fileWrapper && type === "test:fail") fail++;
    }
    if (type === "test:summary" && data.file === undefined) {
      yield `# pass ${pass}\n# fail ${fail}\n# cancelled ${data.counts.cancelled}\n`;
    }
  }
}
