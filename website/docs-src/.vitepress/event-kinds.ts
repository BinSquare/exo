import { readFileSync } from "node:fs";

// Use the runtime's variant-to-kind mapping, excluding legacy aliases.
export function eventKindsMarkdown(): string {
  const source = readFileSync(
    new URL("../../../crates/exoharness/src/types.rs", import.meta.url),
    "utf8",
  );
  const names = new Map(
    Array.from(
      source.matchAll(
        /pub const ([A-Z_]+): EventKind =\s*EventKind\(Cow::Borrowed\("([^"]+)"\)\);/g,
      ),
      (match) => [match[1], match[2]],
    ),
  );
  const start = source.indexOf("pub fn kind(&self) -> EventKind");
  const end = source.indexOf("Self::Custom", start);
  if (start < 0 || end < 0) throw new Error("EventData::kind mapping not found");
  const rows = Array.from(
    source.slice(start, end).matchAll(
      /Self::(\w+)(?:\s*\{[^}]*\})?\s*=>\s*EventKind::([A-Z_]+)/g,
    ),
    (match) => {
      const name = names.get(match[2]);
      if (!name) throw new Error(`Event kind missing: ${match[2]}`);
      return `| \`${name}\` |`;
    },
  );
  if (rows.length === 0) throw new Error("No event kinds found");
  return ["| Event type |", "|:-----------|", ...rows].join("\n");
}
