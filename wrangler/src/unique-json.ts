/** Validate the grammar, then reject overwritten object members at any depth. */
export function parseUniqueJson(text: string): unknown {
  const value: unknown = JSON.parse(text);
  const scopes: (Set<string> | null)[] = [];
  for (const match of text.matchAll(/"(?:[^"\\]|\\.)*"\s*:?|[{}\[\]]/g)) {
    const token = match[0];
    if (token === "{") scopes.push(new Set());
    else if (token === "[") scopes.push(null);
    else if (token === "}" || token === "]") scopes.pop();
    else if (token.endsWith(":")) {
      const keys = scopes.at(-1);
      const key: string = JSON.parse(token.slice(0, -1));
      if (!keys || keys.has(key)) throw new Error("duplicate_json_key");
      keys.add(key);
    }
  }
  return value;
}
