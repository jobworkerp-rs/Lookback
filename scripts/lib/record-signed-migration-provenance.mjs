#!/usr/bin/env node
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const [bundleRoot, frontPath, importPath, ...signedArtifacts] = process.argv.slice(2);
if (![bundleRoot, frontPath, importPath].every(Boolean)) {
  throw new Error("usage: record-signed-migration-provenance.mjs BUNDLE FRONT IMPORT [SIGNED_ARTIFACT ...]");
}

const manifestPath = path.join(bundleRoot, "memories-source-provenance.json");
let manifest;
try {
  manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
} catch (error) {
  throw new Error(`cannot read Memories source provenance: ${error.message}`);
}
if (manifest.format_version !== 3) {
  throw new Error("signed migration provenance requires format version 3 source artifact hashes");
}

const files = {
  front: frontPath,
  memories_import: importPath,
  memories_db_migrate: path.join(bundleRoot, "memories-db-migrate"),
};
const signedSet = new Set(signedArtifacts);
if (signedSet.size !== signedArtifacts.length ||
    [...signedSet].some((artifact) => !Object.hasOwn(files, artifact))) {
  throw new Error("signed artifact list contains an unknown or duplicate artifact");
}
const sha256 = (file) => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
const distributedDigests = {};
for (const [artifact, file] of Object.entries(files)) {
  const sourceDigest = manifest.source_artifact_sha256?.[artifact];
  if (!/^[a-f0-9]{64}$/i.test(sourceDigest ?? "")) {
    throw new Error(`Memories source provenance lacks a valid source digest for ${artifact}`);
  }
  let stat;
  try {
    stat = fs.statSync(file);
  } catch {
    throw new Error(`signed Memories ${artifact} artifact is missing`);
  }
  if (!stat.isFile()) throw new Error(`signed Memories ${artifact} artifact is not a file`);
  const distributedDigest = sha256(file);
  if (!signedSet.has(artifact) && distributedDigest !== sourceDigest) {
    throw new Error(`unsigned Memories ${artifact} artifact changed after source verification`);
  }
  distributedDigests[artifact] = distributedDigest;
}

manifest.distributed_artifact_sha256 = distributedDigests;
manifest.signed_artifacts = [...signedSet].sort();
const temporary = `${manifestPath}.tmp-${process.pid}`;
fs.writeFileSync(temporary, `${JSON.stringify(manifest, null, 2)}\n`, { flag: "wx", mode: 0o644 });
fs.renameSync(temporary, manifestPath);
console.log("post-sign Memories artifact digests recorded");
