#!/usr/bin/env node
// Verify that a pinned Memories source and the app's deliberately curated
// protobuf/workflow assets form one embedding contract.
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";

export const EMBEDDING_SOURCE_TO_APP = Object.freeze({
  "protobuf/protobuf/llm_memory/data/embedding_space.proto":
    "proto/llm_memory/data/embedding_space.proto",
  "protobuf/protobuf/llm_memory/service/embedding_space.proto":
    "proto/llm_memory/service/embedding_space.proto",
  "workflows/auto-embedding.yaml": "workers/workflows/auto-embedding.yaml",
  "workflows/auto-embedding-workers.yaml": "workers/workflows/auto-embedding-workers.yaml",
  "workflows/auto-image-caption.yaml": "workers/workflows/auto-image-caption.yaml",
  "workflows/auto-image-embedding.yaml": "workers/workflows/auto-image-embedding.yaml",
  "workflows/auto-image-embedding-workers.yaml": "workers/workflows/auto-image-embedding-workers.yaml",
  "workflows/auto-thread-embedding.yaml": "workers/workflows/auto-thread-embedding.yaml",
  "workflows/auto-thread-embedding-workers.yaml": "workers/workflows/auto-thread-embedding-workers.yaml",
  "workflows/thread-reflection/auto-reflection-intent-embedding.yaml":
    "workers/workflows/thread-reflection/auto-reflection-intent-embedding.yaml",
  "workflows/thread-reflection/auto-reflection-intent-embedding-workers.yaml":
    "workers/workflows/thread-reflection/auto-reflection-intent-embedding-workers.yaml",
  "workflows/thread-reflection/auto-reflection-summary-embedding.yaml":
    "workers/workflows/thread-reflection/auto-reflection-summary-embedding.yaml",
  "workflows/thread-reflection/auto-reflection-summary-embedding-workers.yaml":
    "workers/workflows/thread-reflection/auto-reflection-summary-embedding-workers.yaml",
});

export const EMBEDDING_SOURCE_CLI_FILES = Object.freeze([
  "grpc-admin/src/bin/memories_db_migrate.rs",
  "grpc-admin/src/bin/memories_db_migrate/embedding_command.rs",
  "grpc-admin/src/bin/memories_db_migrate/embedding_migrate.rs",
  "grpc-admin/src/bin/memories_db_migrate/embedding_finish.rs",
]);

// These are app-owned overlays, not files to copy from the Memories checkout.
export const EMBEDDING_APP_OVERLAYS = Object.freeze([
  "src-tauri/src/commands/embedding_workers_yaml.rs",
  "workers/workflows/rag/lookback-recall.yaml",
  "workers/workflows/rag/lookback-get-context.yaml",
  "workers/workflows/rag/lookback-get-reflections.yaml",
]);

const WORKER_WORKFLOWS = Object.freeze([
  ["workflows/auto-embedding-workers.yaml", ["memories-auto-embedding"]],
  ["workflows/auto-image-embedding-workers.yaml", ["memories-auto-image-embedding"]],
  ["workflows/auto-thread-embedding-workers.yaml", ["memories-auto-thread-embedding"]],
  ["workflows/thread-reflection/auto-reflection-intent-embedding-workers.yaml", ["memories-auto-reflection-intent-embedding"]],
  ["workflows/thread-reflection/auto-reflection-summary-embedding-workers.yaml", ["memories-auto-reflection-summary-embedding"]],
]);

const EXPECTED_EMBEDDING_COMMANDS = Object.freeze([
  "inspect",
  "plan",
  "switch",
  "abandon",
  "finalize",
  "restore",
]);

const sha256 = (bytes) => crypto.createHash("sha256").update(bytes).digest("hex");
const sourcePaths = Object.freeze([
  ...Object.keys(EMBEDDING_SOURCE_TO_APP),
  ...EMBEDDING_SOURCE_CLI_FILES,
]);

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function assertSafeRelativePath(relative) {
  assert(
    typeof relative === "string" && relative.length > 0 && !path.posix.isAbsolute(relative) &&
      !relative.includes("\\") && relative.split("/").every((part) => part && part !== "." && part !== ".."),
    `unsafe embedding contract path: ${relative}`,
  );
}

function readRegularFile(root, relative, label) {
  assertSafeRelativePath(relative);
  let current = path.resolve(root);
  for (const component of relative.split("/")) {
    current = path.join(current, component);
    let stat;
    try {
      stat = fs.lstatSync(current);
    } catch {
      throw new Error(`${label} is missing ${relative}`);
    }
    assert(!stat.isSymbolicLink(), `${label} must not contain a symbolic link: ${relative}`);
    if (current !== path.resolve(root)) {
      assert(stat.isDirectory() || current === path.join(path.resolve(root), ...relative.split("/")),
        `${label} contains a non-directory path component: ${relative}`);
    }
  }
  const stat = fs.lstatSync(current);
  assert(stat.isFile(), `${label} path is not a regular file: ${relative}`);
  return fs.readFileSync(current);
}

function assertWorkflowMarkers(sourcePath, contents) {
  const filename = path.posix.basename(sourcePath);
  for (const workerName of WORKER_WORKFLOWS.find(([candidate]) => candidate === sourcePath)?.[1] ?? []) {
    assert(
      contents.includes(`${workerName}%{MEMORY_EMBEDDING_SPACE_SUFFIX}`),
      `Memories embedding worker lost its space suffix in ${sourcePath}`,
    );
  }
  if (filename === "auto-image-embedding-workers.yaml") {
    assert(contents.includes("$file: auto-image-caption.yaml"),
      "Memories image embedding worker no longer references its caption workflow");
    const captionWorker = contents.match(/^[ \t]*-?\s*name:\s*["']?memories-auto-image-caption[^\n]*/m)?.[0];
    assert(captionWorker && !captionWorker.includes("MEMORY_EMBEDDING_SPACE_SUFFIX"),
      "image caption worker must not be suffixed to an embedding space");
  }

  if (!sourcePath.endsWith("-workers.yaml")) return;
  for (const reference of contents.matchAll(/^\s*\$file:\s*([A-Za-z0-9._/-]+)\s*$/gm)) {
    const referencedSource = path.posix.normalize(
      path.posix.join(path.posix.dirname(sourcePath), reference[1]),
    );
    assert(
      Object.hasOwn(EMBEDDING_SOURCE_TO_APP, referencedSource),
      `embedding workflow reference has no explicit app mapping: ${sourcePath} -> ${reference[1]}`,
    );
  }
}

function fieldValueInYamlBlock(lines, parentIndex, boundaryIndent, fieldIndent, fieldName) {
  const values = [];
  const fieldPattern = new RegExp(`^ {${fieldIndent}}${fieldName}:[ \\t]*(.*?)[ \\t]*(?:#.*)?$`);
  for (let index = parentIndex + 1; index < lines.length; index += 1) {
    const line = lines[index];
    if (!line.trim() || /^[ \t]*#/.test(line)) continue;
    const indentation = line.match(/^ */)[0].length;
    if (indentation <= boundaryIndent) break;
    const match = line.match(fieldPattern);
    if (match) values.push(match[1]);
  }
  return values.length === 1 ? values[0] : null;
}

export function assertRagQueryWorkerContract(contents) {
  const lines = contents.split(/\r?\n/);
  const embedStep = lines.findIndex((line) => /^([ ]*)-[ ]+embedQuery:[ ]*(?:#.*)?$/.test(line));
  const stepIndent = embedStep < 0 ? -1 : lines[embedStep].match(/^ */)[0].length;
  const runIndent = stepIndent + 4;
  const workerIndent = runIndent + 2;
  const argumentsIndent = workerIndent + 2;
  const run = embedStep < 0 ? null : fieldValueInYamlBlock(
    lines, embedStep, stepIndent, runIndent, "run",
  );
  const runIndex = run === null ? -1 : lines.findIndex((line, index) => index > embedStep &&
    new RegExp(`^ {${runIndent}}run:[ \\t]*(?:#.*)?$`).test(line));
  const worker = runIndex < 0 ? null : fieldValueInYamlBlock(
    lines, runIndex, runIndent, workerIndent, "worker",
  );
  const workerIndex = worker === null ? -1 : lines.findIndex((line, index) => index > runIndex &&
    new RegExp(`^ {${workerIndent}}worker:[ \\t]*(?:#.*)?$`).test(line));
  const workerName = workerIndex < 0 ? null : fieldValueInYamlBlock(
    lines, workerIndex, workerIndent, workerIndent + 2, "name",
  );
  const argumentsField = workerIndex < 0 ? null : fieldValueInYamlBlock(
    lines, workerIndex, workerIndent, argumentsIndent, "arguments",
  );
  const argumentsIndex = argumentsField === null ? -1 : lines.findIndex((line, index) => index > workerIndex &&
    new RegExp(`^ {${argumentsIndent}}arguments:[ \\t]*(?:#.*)?$`).test(line));
  const queryPrefix = argumentsIndex < 0 ? null : fieldValueInYamlBlock(
    lines, argumentsIndex, argumentsIndent, argumentsIndent + 2, "prefix",
  );
  const scalar = (value) => value?.match(/^(["'])([^"']*)\1$/)?.[2] ?? null;

  assert(scalar(workerName) === "%{LOOKBACK_MM_EMBEDDING_WORKER}" &&
    scalar(queryPrefix) === "%{MEMORY_EMBEDDING_QUERY_PREFIX:-}",
  "agent-app RAG query embedding overlay must use the resolved worker placeholder and query prefix");
}

function verifyCliSource(sourceContents) {
  const entrypoint = sourceContents["grpc-admin/src/bin/memories_db_migrate.rs"].toString("utf8");
  const commands = sourceContents[
    "grpc-admin/src/bin/memories_db_migrate/embedding_command.rs"
  ].toString("utf8");
  assert(/Embedding\s*\{/.test(entrypoint) && /run_embedding/.test(entrypoint),
    "Memories source does not register the embedding CLI command");
  for (const command of EXPECTED_EMBEDDING_COMMANDS) {
    const commandName = command[0].toUpperCase() + command.slice(1);
    assert(new RegExp(`\\b${commandName}\\b`).test(commands),
      `Memories embedding CLI is missing the ${command} subcommand`);
  }
  for (const required of ["embedding_migrate.rs", "embedding_finish.rs"]) {
    assert(sourceContents[
      `grpc-admin/src/bin/memories_db_migrate/${required}`
    ].length > 0, `Memories embedding CLI implementation is empty: ${required}`);
  }
}

export function verifyEmbeddingContract(sourceRoot, appRoot) {
  const sourceContents = Object.fromEntries(sourcePaths.map((relative) => [
    relative,
    readRegularFile(sourceRoot, relative, "Memories source"),
  ]));
  const sourceDigests = Object.fromEntries(sourcePaths.map((relative) => [
    relative,
    sha256(sourceContents[relative]),
  ]));

  verifyCliSource(sourceContents);
  for (const [sourcePath, appPath] of Object.entries(EMBEDDING_SOURCE_TO_APP)) {
    const sourceBytes = sourceContents[sourcePath];
    const appBytes = readRegularFile(appRoot, appPath, "agent-app embedding assets");
    if (sourcePath.startsWith("workflows/")) {
      assertWorkflowMarkers(sourcePath, sourceBytes.toString("utf8"));
    }
    assert(
      sha256(sourceBytes) === sha256(appBytes),
      `agent-app embedding asset differs from pinned Memories source: ${sourcePath} -> ${appPath}`,
    );
  }

  const appDigests = Object.fromEntries(Object.entries(EMBEDDING_SOURCE_TO_APP).map(([, appPath]) => [
    appPath,
    sha256(readRegularFile(appRoot, appPath, "agent-app embedding assets")),
  ]));
  for (const relative of EMBEDDING_APP_OVERLAYS) {
    const bytes = readRegularFile(appRoot, relative, "agent-app embedding overlay");
    appDigests[relative] = sha256(bytes);
    if (relative === "src-tauri/src/commands/embedding_workers_yaml.rs") {
      const renderer = bytes.toString("utf8");
      assert(renderer.includes("workers/workflows/auto-embedding-workers.yaml"),
        "agent-app renderer no longer consumes the bundled embedding worker template");
    }
    if (relative === "workers/workflows/rag/lookback-recall.yaml") {
      assertRagQueryWorkerContract(bytes.toString("utf8"));
    }
  }

  return {
    format_version: 1,
    source_sha256: sourceDigests,
    app_sha256: appDigests,
    mappings: { ...EMBEDDING_SOURCE_TO_APP },
    overlays: [...EMBEDDING_APP_OVERLAYS],
  };
}

function git(sourceRoot, args) {
  const result = spawnSync("git", ["-C", sourceRoot, ...args], { encoding: "utf8" });
  if (result.error || result.status !== 0) {
    throw new Error(`cannot inspect Memories source Git revision (${args[0]})`);
  }
  return result.stdout.trim();
}

function recordProvenance(sourceRoot, appRoot, provenancePath) {
  let provenance;
  try {
    provenance = JSON.parse(fs.readFileSync(provenancePath, "utf8"));
  } catch (error) {
    throw new Error(`cannot read Memories source provenance: ${error.message}`);
  }
  assert(provenance.format_version === 3,
    "embedding contract requires format version 3 Memories source provenance");
  const sourceRevision = git(sourceRoot, ["rev-parse", "HEAD"]).toLowerCase();
  assert(/^[a-f0-9]{40}$/.test(provenance.source_revision ?? "") &&
    sourceRevision === provenance.source_revision.toLowerCase(),
  "Memories embedding contract source revision differs from bundle provenance");
  assert(git(sourceRoot, ["status", "--porcelain", "--untracked-files=all"]).length === 0,
    "Memories embedding contract source worktree is not clean");

  const contract = verifyEmbeddingContract(sourceRoot, appRoot);
  contract.source_revision = sourceRevision;
  provenance.embedding_contract = contract;
  const temporary = `${provenancePath}.tmp-${process.pid}`;
  fs.writeFileSync(temporary, `${JSON.stringify(provenance, null, 2)}\n`, {
    flag: "wx",
    mode: 0o644,
  });
  fs.renameSync(temporary, provenancePath);
  return contract;
}

export function verifyEmbeddedContract(provenance, appRoot = null) {
  const contract = provenance.embedding_contract;
  assert(contract && contract.format_version === 1,
    "Memories source provenance lacks the required embedding contract");
  assert(contract.source_revision === provenance.source_revision,
    "Memories embedding contract revision differs from source provenance");

  const mappings = contract.mappings;
  assert(mappings && typeof mappings === "object" && !Array.isArray(mappings),
    "Memories embedding contract mappings are missing");
  assert(JSON.stringify(Object.keys(mappings).sort()) ===
    JSON.stringify(Object.keys(EMBEDDING_SOURCE_TO_APP).sort()),
  "Memories embedding contract source-to-app mapping is incomplete or unexpected");
  for (const [sourcePath, appPath] of Object.entries(EMBEDDING_SOURCE_TO_APP)) {
    assert(mappings[sourcePath] === appPath,
      `Memories embedding contract has an invalid asset mapping for ${sourcePath}`);
  }

  const expectedSourceFiles = [...sourcePaths].sort();
  const sourceDigests = contract.source_sha256;
  assert(sourceDigests && typeof sourceDigests === "object" && !Array.isArray(sourceDigests) &&
    JSON.stringify(Object.keys(sourceDigests).sort()) === JSON.stringify(expectedSourceFiles),
  "Memories embedding contract source digest map is incomplete or unexpected");
  for (const relative of expectedSourceFiles) {
    assert(/^[a-f0-9]{64}$/i.test(sourceDigests[relative] ?? ""),
      `Memories embedding contract has an invalid source digest for ${relative}`);
  }

  const expectedAppFiles = [
    ...Object.values(EMBEDDING_SOURCE_TO_APP),
    ...EMBEDDING_APP_OVERLAYS,
  ].sort();
  const appDigests = contract.app_sha256;
  assert(appDigests && typeof appDigests === "object" && !Array.isArray(appDigests) &&
    JSON.stringify(Object.keys(appDigests).sort()) === JSON.stringify(expectedAppFiles),
  "Memories embedding contract app digest map is incomplete or unexpected");
  for (const relative of expectedAppFiles) {
    assert(/^[a-f0-9]{64}$/i.test(appDigests[relative] ?? ""),
      `Memories embedding contract has an invalid app digest for ${relative}`);
  }
  assert(JSON.stringify(contract.overlays) === JSON.stringify([...EMBEDDING_APP_OVERLAYS]),
    "Memories embedding contract app overlay mapping is incomplete or unexpected");
  for (const [sourcePath, appPath] of Object.entries(EMBEDDING_SOURCE_TO_APP)) {
    assert(sourceDigests[sourcePath] === appDigests[appPath],
      `Memories source and app embedding digests differ for ${sourcePath}`);
  }

  if (appRoot) {
    for (const relative of expectedAppFiles) {
      assert(
        sha256(readRegularFile(appRoot, relative, "packaged agent-app embedding asset")) === appDigests[relative],
        `packaged agent-app embedding asset does not match the source contract: ${relative}`,
      );
    }
    assertRagQueryWorkerContract(readRegularFile(
      appRoot,
      "workers/workflows/rag/lookback-recall.yaml",
      "packaged agent-app embedding asset",
    ).toString("utf8"));
  }
  return contract;
}

function usage() {
  return "usage: verify-embedding-contract.mjs MEMORIES_SOURCE AGENT_APP [--record-provenance FILE]";
}

function main(argv) {
  const [sourceRoot, appRoot, ...options] = argv;
  assert(sourceRoot && appRoot, usage());
  let provenancePath = null;
  for (let index = 0; index < options.length; index += 1) {
    if (options[index] === "--record-provenance" && options[index + 1]) {
      provenancePath = options[++index];
    } else {
      throw new Error(`unknown or incomplete embedding contract option: ${options[index]}`);
    }
  }
  if (provenancePath) {
    const contract = recordProvenance(sourceRoot, appRoot, provenancePath);
    console.log(`Memories embedding contract recorded: ${contract.source_revision}`);
  } else {
    verifyEmbeddingContract(sourceRoot, appRoot);
    console.log(`Memories embedding source/app contract verified: ${sourceRoot}`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
