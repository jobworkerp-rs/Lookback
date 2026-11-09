#!/usr/bin/env node
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const [command, rootArg, manifestArg] = process.argv.slice(2);
if (!(["create", "verify"].includes(command) && rootArg && manifestArg)) {
  throw new Error("usage: migration-bundle-snapshot.mjs create|verify BUNDLE_DIRECTORY SNAPSHOT_FILE");
}

const root = path.resolve(rootArg);
const manifestPath = path.resolve(manifestArg);
const sha256 = (bytes) => crypto.createHash("sha256").update(bytes).digest("hex");

function inventory() {
  const entries = [];
  const visit = (directory, relative = "") => {
    for (const dirent of fs.readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const name = relative ? `${relative}/${dirent.name}` : dirent.name;
      const target = path.join(directory, dirent.name);
      const stat = fs.lstatSync(target);
      const mode = stat.mode & 0o7777;
      if (stat.isDirectory()) {
        entries.push({ path: name, type: "directory", mode });
        visit(target, name);
      } else if (stat.isFile()) {
        entries.push({ path: name, type: "file", mode, sha256: sha256(fs.readFileSync(target)) });
      } else if (stat.isSymbolicLink()) {
        entries.push({ path: name, type: "symlink", target: fs.readlinkSync(target) });
      } else {
        throw new Error(`unsupported migration bundle entry type: ${name}`);
      }
    }
  };
  if (!fs.existsSync(root) || !fs.lstatSync(root).isDirectory()) {
    throw new Error(`migration bundle snapshot root is not a directory: ${root}`);
  }
  visit(root);
  return entries;
}

if (command === "create") {
  if (fs.existsSync(manifestPath)) throw new Error(`migration bundle snapshot already exists: ${manifestPath}`);
  const manifest = { format_version: 1, entries: inventory() };
  fs.mkdirSync(path.dirname(manifestPath), { recursive: true });
  const temporary = `${manifestPath}.tmp-${process.pid}`;
  fs.writeFileSync(temporary, `${JSON.stringify(manifest, null, 2)}\n`, { flag: "wx", mode: 0o600 });
  fs.renameSync(temporary, manifestPath);
} else {
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  } catch (error) {
    throw new Error(`cannot read migration bundle snapshot: ${error.message}`);
  }
  if (manifest.format_version !== 1 || !Array.isArray(manifest.entries)) {
    throw new Error("unsupported migration bundle snapshot format");
  }
  const expectedPaths = new Set();
  for (const entry of manifest.entries) {
    if (
      typeof entry.path !== "string" ||
      entry.path.length === 0 ||
      path.posix.isAbsolute(entry.path) ||
      entry.path.split("/").some((part) => part === "" || part === "." || part === "..") ||
      expectedPaths.has(entry.path)
    ) {
      throw new Error("migration bundle snapshot contains an invalid or duplicate path");
    }
    expectedPaths.add(entry.path);
  }
  const actual = inventory();
  if (JSON.stringify(actual) !== JSON.stringify(manifest.entries)) {
    throw new Error("migration bundle backup differs from its preserved activation snapshot");
  }
}
