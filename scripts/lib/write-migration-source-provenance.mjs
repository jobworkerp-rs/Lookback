#!/usr/bin/env node
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { credentialFreeOrigin } from "./git-origin.mjs";

const [sourceRoot, bundleRoot, expectedRef, expectedRevision, frontPath, importPath, resolvedRef] = process.argv.slice(2);
if (![sourceRoot, bundleRoot, expectedRef, expectedRevision, frontPath, importPath].every(Boolean)) {
  throw new Error(
    "usage: write-migration-source-provenance.mjs SOURCE BUNDLE REF REVISION FRONT IMPORT [RESOLVED_REF]",
  );
}
if (!/^[a-f0-9]{40}$/i.test(expectedRevision)) {
  throw new Error("Memories source revision must be a full 40-character Git SHA-1");
}

const git = (args) => {
  const result = spawnSync("git", ["-C", sourceRoot, ...args], { encoding: "utf8" });
  if (result.error || result.status !== 0) {
    throw new Error(`cannot inspect pinned Memories Git source (${args[0]})`);
  }
  return result.stdout.trim();
};
const revision = git(["rev-parse", "HEAD"]).toLowerCase();
if (revision !== expectedRevision.toLowerCase()) {
  throw new Error("Memories source HEAD does not match the requested pinned revision");
}
const sourceRevisionRef = resolvedRef || expectedRef;
const refRevision = git([
  "rev-parse", "--verify", "--end-of-options", `${sourceRevisionRef}^{commit}`,
]).toLowerCase();
git(["merge-base", "--is-ancestor", revision, refRevision]);
const worktree = git(["status", "--porcelain", "--untracked-files=all"]);
if (worktree.length > 0) {
  throw new Error("Memories source worktree is not clean; provenance cannot claim the pinned revision");
}
const rawOrigin = git(["remote", "get-url", "origin"]);

const sourceRepository = credentialFreeOrigin(rawOrigin, "Memories Git origin");
const sha256 = (bytes) => crypto.createHash("sha256").update(bytes).digest("hex");
const readBundleFile = (relative) => {
  const file = path.join(bundleRoot, relative);
  let stat;
  try {
    stat = fs.statSync(file);
  } catch {
    throw new Error(`verified Memories bundle is missing ${relative}`);
  }
  if (!stat.isFile()) throw new Error(`verified Memories bundle path is not a file: ${relative}`);
  return fs.readFileSync(file);
};
const readArtifact = (file, label) => {
  let stat;
  try {
    stat = fs.statSync(file);
  } catch {
    throw new Error(`Memories ${label} build output is missing`);
  }
  if (!stat.isFile()) throw new Error(`Memories ${label} build output is not a file`);
  return sha256(fs.readFileSync(file));
};
const latestMigration = (root, backend) => {
  const dir = path.join(root, "infra", "atlas", backend, "migrations");
  const latest = fs.readdirSync(dir)
    .filter((name) => /^\d{14}_.*\.sql$/.test(name))
    .sort()
    .at(-1);
  if (!latest) throw new Error(`Memories bundle has no ${backend} migration`);
  return { name: latest, digest: sha256(fs.readFileSync(path.join(dir, latest))) };
};

const sourceSqliteMigration = latestMigration(sourceRoot, "sqlite");
const sourcePostgresMigration = latestMigration(sourceRoot, "postgres");
const sourceFiles = {
  task_catalog: path.join(sourceRoot, "infra/atlas/post-migration-tasks.json"),
  task_history: path.join(sourceRoot, "infra/atlas/post-migration-task-history.json"),
  sqlite_migration: path.join(sourceRoot, "infra/atlas/sqlite/migrations", sourceSqliteMigration.name),
  postgres_migration: path.join(sourceRoot, "infra/atlas/postgres/migrations", sourcePostgresMigration.name),
};
const sourceDigests = Object.fromEntries(
  Object.entries(sourceFiles).map(([key, file]) => {
    let stat;
    try {
      stat = fs.statSync(file);
    } catch {
      throw new Error(`pinned Memories source is missing ${key}`);
    }
    if (!stat.isFile()) throw new Error(`pinned Memories source ${key} is not a file`);
    return [key, sha256(fs.readFileSync(file))];
  }),
);
const bundleDigests = {
  task_catalog: sha256(readBundleFile("atlas/post-migration-tasks.json")),
  task_history: sha256(readBundleFile("atlas/post-migration-task-history.json")),
  sqlite_migration: sha256(readBundleFile(path.join(
    "atlas", "sqlite", "migrations", sourceSqliteMigration.name,
  ))),
  postgres_migration: sha256(readBundleFile(path.join(
    "atlas", "postgres", "migrations", sourcePostgresMigration.name,
  ))),
};
for (const [key, sourceDigest] of Object.entries(sourceDigests)) {
  if (bundleDigests[key] !== sourceDigest) {
    throw new Error(`generated Memories bundle ${key} differs from pinned source ${revision}`);
  }
}

const sourceArtifactDigests = {
  front: readArtifact(frontPath, "front"),
  memories_import: readArtifact(importPath, "memories-import"),
  memories_db_migrate: sha256(readBundleFile("memories-db-migrate")),
};
// Ties this provenance to the bundle exactly as Memories built and
// self-verified it; later steps add this file and may sign the executables.
let bundleManifest;
try {
  bundleManifest = JSON.parse(readBundleFile("bundle-manifest.json").toString("utf8"));
} catch (error) {
  throw new Error(`verified Memories bundle manifest is unreadable: ${error.message}`);
}
if (!/^[a-f0-9]{64}$/i.test(bundleManifest.content_digest ?? "")) {
  throw new Error("verified Memories bundle manifest lacks a valid content digest");
}
const manifest = {
  format_version: 3,
  source_repository: sourceRepository,
  source_ref: expectedRef,
  source_revision: revision,
  bundle_content_digest: bundleManifest.content_digest,
  source_artifact_sha256: sourceArtifactDigests,
  distributed_artifact_sha256: { ...sourceArtifactDigests },
  signed_artifacts: [],
  source_sha256: sourceDigests,
};
const target = path.join(bundleRoot, "memories-source-provenance.json");
const temporary = `${target}.tmp-${process.pid}`;
fs.writeFileSync(temporary, `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o644, flag: "wx" });
fs.renameSync(temporary, target);
console.log(`Memories source provenance recorded: ${sourceRepository} ${expectedRef}@${revision}`);
