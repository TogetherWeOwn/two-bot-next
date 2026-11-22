// Emit counts only. A passing file with no test declarations is not a pass.
export default async function* mutationReporter(events) {
  let pass = 0;
  let fail = 0;
  for await (const { type, data } of events) {
    if (data.line > 0 && !data.skip && !data.todo) {
      if (type === "test:pass" && data.details.type === "test") pass++;
      if (type === "test:fail" && data.details.type === "test") fail++;
    }
    if (type === "test:summary" && data.file === undefined) {
      yield `# pass ${pass}\n# fail ${fail}\n# cancelled ${data.counts.cancelled}\n`;
    }
  }
}
