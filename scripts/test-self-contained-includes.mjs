// CI checks out this repository alone, so every file the Rust crate embeds
// at compile time (include_str! / include_bytes!, tests included) must live
// inside it. A path into a sibling checkout compiles locally and breaks CI.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const crateRoot = path.join(repoRoot, "src-tauri");
const includePattern = /include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)/g;

function rustFiles(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) return entry.name === "target" ? [] : rustFiles(full);
    return entry.name.endsWith(".rs") ? [full] : [];
  });
}

const escaping = [];
let checked = 0;
for (const file of rustFiles(crateRoot)) {
  for (const [, literal] of fs.readFileSync(file, "utf8").matchAll(includePattern)) {
    checked += 1;
    const target = path.resolve(path.dirname(file), literal);
    if (path.relative(repoRoot, target).startsWith("..")) {
      escaping.push(`${path.relative(repoRoot, file)}: ${literal}`);
    }
  }
}

assert.ok(checked > 0, "expected include_str!/include_bytes! uses under src-tauri");
assert.deepEqual(escaping, [], `includes outside the repository:\n${escaping.join("\n")}`);
console.log(`self-contained include checks passed (${checked} includes)`);
