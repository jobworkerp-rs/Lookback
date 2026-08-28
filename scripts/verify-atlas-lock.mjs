#!/usr/bin/env node
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const [atlasRoot, platform] = process.argv.slice(2);
if (!atlasRoot || !platform) {
  throw new Error("usage: verify-atlas-lock.mjs ATLAS_DIRECTORY PLATFORM");
}

const lockPath = path.join(atlasRoot, "atlas-tool.lock.json");
const atlasPath = path.join(atlasRoot, "bin", "atlas");
let lock;
try {
  lock = JSON.parse(fs.readFileSync(lockPath, "utf8"));
} catch (error) {
  throw new Error(`cannot read atlas-tool.lock.json: ${error.message}`);
}

const expected = lock.platforms?.[platform]?.sha256;
if (typeof expected !== "string" || !/^[a-f0-9]{64}$/i.test(expected)) {
  throw new Error(`atlas-tool.lock.json lacks a valid SHA-256 for ${platform}`);
}

let actual;
try {
  actual = crypto.createHash("sha256").update(fs.readFileSync(atlasPath)).digest("hex");
} catch (error) {
  throw new Error(`cannot read fixed Atlas binary: ${error.message}`);
}

if (actual.toLowerCase() !== expected.toLowerCase()) {
  throw new Error(
    `Fixed Atlas binary SHA-256 does not match atlas-tool.lock.json for ${platform}: expected ${expected}, got ${actual}`,
  );
}

console.log(`Atlas lock verified: ${platform}`);
