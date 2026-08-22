#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";

const [releaseSource, bundle] = process.argv.slice(2);
if (!releaseSource || !bundle) {
  throw new Error("usage: verify-migration-release.mjs RELEASE_RUST_SOURCE BUNDLE_DIRECTORY");
}
const source = fs.readFileSync(releaseSource, "utf8");
const requireFile = (relative, executable = false) => {
  const target = path.join(bundle, relative);
  let stat;
  try {
    stat = fs.statSync(target);
  } catch {
    throw new Error(`migration bundle file is missing: ${relative}`);
  }
  if (!stat.isFile()) {
    throw new Error(`migration bundle path is not a file: ${relative}`);
  }
  if (executable && (stat.mode & 0o111) === 0) {
    throw new Error(`migration bundle file is not executable: ${relative}`);
  }
  return target;
};

requireFile("memories-db-migrate", true);
requireFile(path.join("atlas", "bin", "atlas"), true);
requireFile(path.join("atlas", "sqlite", "migrations", "atlas.sum"));
const capture = (pattern, label) => {
  const match = source.match(pattern);
  if (!match) throw new Error(`release ${label} is missing`);
  return match[1];
};
const migrationId = capture(/migration_id:\s*"([^"]+)"/, "migration_id");
const expectedContract = capture(
  /expected_schema_contract:\s*"(\d{14})"/,
  "expected_schema_contract",
);
const migrationsDir = path.join(bundle, "atlas", "sqlite", "migrations");
const versions = fs
  .readdirSync(migrationsDir)
  .map((name) => name.match(/^(\d{14})_.*\.sql$/)?.[1])
  .filter(Boolean)
  .sort();
const latest = versions.at(-1);
if (latest !== expectedContract) {
  throw new Error(`release schema contract ${expectedContract} != bundled latest ${latest}`);
}
const latestSql = fs.readFileSync(
  path.join(
    migrationsDir,
    fs.readdirSync(migrationsDir).find((name) => name.startsWith(`${latest}_`)),
  ),
  "utf8",
);
if (!latestSql.includes(`VALUES ('rdb_schema', '${expectedContract}')`)) {
  throw new Error("latest migration does not publish the expected schema contract");
}
const catalog = JSON.parse(
  fs.readFileSync(requireFile(path.join("atlas", "post-migration-tasks.json")), "utf8"),
);
const required = catalog.tasks.find(
  (task) =>
    task.id === migrationId &&
    task.generation === 1 &&
    task.lifecycle === "active" &&
    task.completion_required_by_schema_version === expectedContract,
);
if (!required) {
  throw new Error(
    `bundle lacks required ${migrationId}@1 task for schema contract ${expectedContract}`,
  );
}
console.log(`migration release verified: ${migrationId}@1 / ${expectedContract}`);
