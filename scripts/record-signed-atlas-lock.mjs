#!/usr/bin/env node
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const [atlasRoot, platform] = process.argv.slice(2);
if (!atlasRoot || !platform) {
  throw new Error("usage: record-signed-atlas-lock.mjs ATLAS_DIRECTORY PLATFORM");
}

const lockPath = path.join(atlasRoot, "atlas-tool.lock.json");
const atlasPath = path.join(atlasRoot, "bin", "atlas");
let lock;
try {
  lock = JSON.parse(fs.readFileSync(lockPath, "utf8"));
} catch (error) {
  throw new Error(`cannot read atlas-tool.lock.json: ${error.message}`);
}

const entry = lock.platforms?.[platform];
if (!entry || typeof entry.sha256 !== "string" || !/^[a-f0-9]{64}$/i.test(entry.sha256)) {
  throw new Error(`atlas-tool.lock.json lacks a valid SHA-256 for ${platform}`);
}
if (typeof entry.source_sha256 === "string" && entry.source_sha256 !== entry.sha256) {
  throw new Error(`atlas-tool.lock.json already records a distinct source SHA-256 for ${platform}`);
}

let signedSha256;
try {
  signedSha256 = crypto.createHash("sha256").update(fs.readFileSync(atlasPath)).digest("hex");
} catch (error) {
  throw new Error(`cannot read signed Atlas binary: ${error.message}`);
}

entry.source_sha256 = entry.sha256.toLowerCase();
entry.sha256 = signedSha256;
fs.writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`);
console.log(`Recorded signed Atlas SHA-256: ${platform}`);
