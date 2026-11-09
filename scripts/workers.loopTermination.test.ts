import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { describe, expect, it } from "vitest";

// A `for` over a fixed range keeps iterating after its work is done, and the
// engine copies the workflow context for every guard it evaluates on the way
// (jobworkerp-rs #355). Measured on the live engine: 1000 idle iterations of
// three guarded tasks took 10 s with a 200 KB context and 0.05 s once the
// loop carried a `while` condition. Every paging loop must therefore stop
// through `while`.
const workersDir = join(__dirname, "..", "workers");

function yamlFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return name === "schema" ? [] : yamlFiles(path);
    return name.endsWith(".yaml") ? [path] : [];
  });
}

function fixedRangeLoopsWithoutWhile(text: string): number[] {
  const lines = text.split("\n");
  const missing: number[] = [];
  lines.forEach((line, index) => {
    if (!/^\s+in: .*\[range\(0; *\d{3,}\)\]/.test(line)) return;
    const forIndent = lines[index - 2]?.match(/^(\s*)for:/)?.[1] ?? lines[index - 1]?.match(/^(\s*)for:/)?.[1];
    const ownerIndent = (forIndent ?? "").length;
    let hasWhile = false;
    for (let next = index + 1; next < lines.length; next += 1) {
      const candidate = lines[next];
      if (candidate.trim() === "") continue;
      const indent = candidate.length - candidate.trimStart().length;
      if (indent < ownerIndent) break;
      if (indent === ownerIndent && /^\s*while:/.test(candidate)) {
        hasWhile = true;
        break;
      }
      if (indent === ownerIndent && /^\s*do:/.test(candidate)) break;
    }
    if (!hasWhile) missing.push(index + 1);
  });
  return missing;
}

describe("fixed-range paging loops", () => {
  it("detects a loop without while and accepts one with while", () => {
    const loop = (extra: string) =>
      ["  - page:", "      for:", "        each: n", "        in: '${ [range(0; 1000)] }'", extra, "      do:", "        - a: {}"].join("\n");
    expect(fixedRangeLoopsWithoutWhile(loop(""))).toEqual([4]);
    expect(fixedRangeLoopsWithoutWhile(loop("      while: '${ $done | not }'"))).toEqual([]);
  });

  it("every workflow paging loop stops through while", () => {
    const offenders = yamlFiles(workersDir).flatMap((file) =>
      fixedRangeLoopsWithoutWhile(readFileSync(file, "utf8")).map((line) => `${relative(workersDir, file)}:${line}`),
    );
    expect(offenders).toEqual([]);
  });
});
