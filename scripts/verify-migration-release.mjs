#!/usr/bin/env node
// Verifies a Memories migration bundle before Lookback stages or packages it.
//
// Lookback deliberately knows nothing about Memories' migration tasks, their
// generations, or its schema versions: the bundle's own `bundle verify`
// command checks the catalog, Atlas checksums, and its content digest. This
// script adds what only the packager can check — that a release bundle still
// matches the pinned Memories source it was built from, and that the shipped
// Atlas binary matches its lock after any code signing.
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { assertRagQueryWorkerContract } from "./verify-embedding-contract.mjs";

const argv = process.argv.slice(2);
const sourceMode = argv[0] === "--source";
const sourceRoot = sourceMode ? argv[1] : null;
const [bundle, ...verifyOptions] = sourceMode ? [] : argv;
if (sourceMode ? !sourceRoot : !bundle || bundle.startsWith("--")) {
  throw new Error(
    "usage: verify-migration-release.mjs BUNDLE_DIRECTORY [--require-provenance] [--source-artifacts] [--app-assets AGENT_APP] [--distributed-assets RESOURCES] [--atlas-platform PLATFORM] [--front FILE] [--import FILE]\n       verify-migration-release.mjs --source MEMORIES_SOURCE_DIRECTORY --app-assets AGENT_APP",
  );
}
const bundleRoot = sourceMode ? "" : path.resolve(bundle);

let requireProvenance = false;
let frontArtifact = null;
let importArtifact = null;
let requireSourceArtifacts = false;
let appAssetsRoot = null;
let distributedAssetsRoot = null;
let atlasPlatformOverride = null;
const options = sourceMode ? argv.slice(2) : verifyOptions;
for (let index = 0; index < options.length; index += 1) {
  const option = options[index];
  if (option === "--require-provenance") {
    requireProvenance = true;
  } else if (option === "--source-artifacts") {
    requireSourceArtifacts = true;
  } else if (option === "--app-assets" && options[index + 1]) {
    appAssetsRoot = path.resolve(options[++index]);
  } else if (option === "--distributed-assets" && options[index + 1]) {
    distributedAssetsRoot = path.resolve(options[++index]);
  } else if (option === "--atlas-platform" && options[index + 1]) {
    atlasPlatformOverride = options[++index];
    if (!["linux-amd64", "darwin-arm64"].includes(atlasPlatformOverride)) {
      throw new Error(`unsupported Atlas verification platform: ${atlasPlatformOverride}`);
    }
  } else if (option === "--front" && options[index + 1]) {
    frontArtifact = options[++index];
  } else if (option === "--import" && options[index + 1]) {
    importArtifact = options[++index];
  } else {
    throw new Error(`unknown or incomplete verifier option: ${option}`);
  }
}
if (requireProvenance && !appAssetsRoot) {
  throw new Error("provenance-verified release bundle verification requires --app-assets AGENT_APP");
}
const requireFile = (relative, executable = false) => {
  const target = path.join(bundleRoot, relative);
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
const readJson = (relative) => {
  try {
    return JSON.parse(fs.readFileSync(requireFile(relative), "utf8"));
  } catch (error) {
    throw new Error(`cannot read migration bundle ${relative}: ${error.message}`);
  }
};
const assert = (condition, message) => {
  if (!condition) throw new Error(message);
};
const run = (executable, args, label, timeout = 30_000, options = {}) => {
  const result = spawnSync(executable, args, { ...options, encoding: "utf8", timeout });
  if (result.error) throw new Error(`${label} could not run: ${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(`${label} failed (${result.status}): ${(result.stderr || result.stdout).trim()}`);
  }
  return `${result.stdout || ""}\n${result.stderr || ""}`;
};

/** Build inputs the release pipeline needs from a pinned Memories checkout. */
function verifyMemoriesSource(root, appRoot) {
  for (const required of [
    "grpc-admin/Cargo.toml",
    "grpc-admin/src/bin/front.rs",
    "grpc-admin/src/bin/memories_db_migrate.rs",
    "agent-chat-import/Cargo.toml",
    "infra/atlas/post-migration-tasks.json",
    "infra/atlas/post-migration-task-history.json",
    "scripts/build-memories-db-migrate-sqlite.sh",
  ]) {
    const sourceFile = path.join(root, required);
    assert(
      fs.existsSync(sourceFile) && fs.statSync(sourceFile).isFile(),
      `Memories source is missing build input ${required}`,
    );
  }
  assert(appRoot, "Memories source verification requires --app-assets AGENT_APP");
  run(
    process.execPath,
    [path.join(path.dirname(fileURLToPath(import.meta.url)), "verify-embedding-contract.mjs"), root, appRoot],
    "Memories source/app embedding contract verification",
    30_000,
  );
}

if (sourceMode) {
  verifyMemoriesSource(sourceRoot, appAssetsRoot);
  console.log(`Memories source build inputs verified: ${sourceRoot}`);
  process.exit(0);
}

const cliPath = requireFile("memories-db-migrate", true);
const atlasPath = requireFile(path.join("atlas", "bin", "atlas"), true);
requireFile(path.join("atlas", "atlas-tool.lock.json"));
requireFile(path.join("atlas", "migrate.hcl"));
requireFile(path.join("atlas", "verify.hcl"));
requireFile(path.join("atlas", "licenses", "ATLAS_LICENSE"));
requireFile(path.join("atlas", "post-migration-tasks.json"));
requireFile(path.join("atlas", "post-migration-task-history.json"));
const manifest = readJson("bundle-manifest.json");
assert(
  /^[a-f0-9]{64}$/i.test(manifest.content_digest ?? ""),
  "bundle-manifest.json lacks a valid content digest",
);

const sha256 = (bytes) => crypto.createHash("sha256").update(bytes).digest("hex");
const digestOfBundleFile = (relative) => sha256(fs.readFileSync(requireFile(relative)));

const embeddingMappings = Object.freeze({
  "protobuf/protobuf/llm_memory/data/embedding_space.proto": "proto/llm_memory/data/embedding_space.proto",
  "protobuf/protobuf/llm_memory/service/embedding_space.proto": "proto/llm_memory/service/embedding_space.proto",
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
const embeddingSourceFiles = Object.freeze([
  ...Object.keys(embeddingMappings),
  "grpc-admin/src/bin/memories_db_migrate.rs",
  "grpc-admin/src/bin/memories_db_migrate/embedding_command.rs",
  "grpc-admin/src/bin/memories_db_migrate/embedding_migrate.rs",
  "grpc-admin/src/bin/memories_db_migrate/embedding_finish.rs",
]);
const embeddingOverlays = Object.freeze([
  "src-tauri/src/commands/embedding_workers_yaml.rs",
  "workers/workflows/rag/lookback-recall.yaml",
  "workers/workflows/rag/lookback-get-context.yaml",
  "workers/workflows/rag/lookback-get-reflections.yaml",
]);

function safeEmbeddingDigest(root, relative) {
  assert(!path.isAbsolute(relative) && !relative.includes("\\") &&
    relative.split("/").every((part) => part && part !== "." && part !== ".."),
  `unsafe embedding contract path: ${relative}`);
  const rootPath = path.resolve(root);
  let current = rootPath;
  for (const component of relative.split("/")) {
    current = path.join(current, component);
    let stat;
    try {
      stat = fs.lstatSync(current);
    } catch {
      throw new Error(`packaged agent-app embedding asset is missing: ${relative}`);
    }
    assert(!stat.isSymbolicLink(), `packaged agent-app embedding asset is a symlink: ${relative}`);
    assert(stat.isDirectory() || current === path.join(rootPath, ...relative.split("/")),
      `packaged agent-app embedding asset path is not a directory: ${relative}`);
  }
  assert(fs.lstatSync(current).isFile(), `packaged agent-app embedding asset is not a file: ${relative}`);
  return sha256(fs.readFileSync(current));
}

function verifyEmbeddedContract(provenance, appRoot = null, distributedRoot = null) {
  const contract = provenance.embedding_contract;
  assert(contract && contract.format_version === 1,
    "Memories source provenance lacks the required embedding contract");
  assert(contract.source_revision === provenance.source_revision,
    "Memories embedding contract revision differs from source provenance");

  const mappings = contract.mappings;
  assert(mappings && typeof mappings === "object" && !Array.isArray(mappings) &&
    JSON.stringify(Object.keys(mappings).sort()) === JSON.stringify(Object.keys(embeddingMappings).sort()),
  "Memories embedding contract source-to-app mapping is incomplete or unexpected");
  for (const [sourcePath, appPath] of Object.entries(embeddingMappings)) {
    assert(mappings[sourcePath] === appPath,
      `Memories embedding contract has an invalid asset mapping for ${sourcePath}`);
  }

  const sourceDigests = contract.source_sha256;
  const expectedSourceFiles = [...embeddingSourceFiles].sort();
  assert(sourceDigests && typeof sourceDigests === "object" && !Array.isArray(sourceDigests) &&
    JSON.stringify(Object.keys(sourceDigests).sort()) === JSON.stringify(expectedSourceFiles),
  "Memories embedding contract source digest map is incomplete or unexpected");
  for (const relative of expectedSourceFiles) {
    assert(/^[a-f0-9]{64}$/i.test(sourceDigests[relative] ?? ""),
      `Memories embedding contract has an invalid source digest for ${relative}`);
  }

  const appDigests = contract.app_sha256;
  const expectedAppFiles = [...Object.values(embeddingMappings), ...embeddingOverlays].sort();
  assert(appDigests && typeof appDigests === "object" && !Array.isArray(appDigests) &&
    JSON.stringify(Object.keys(appDigests).sort()) === JSON.stringify(expectedAppFiles),
  "Memories embedding contract app digest map is incomplete or unexpected");
  for (const relative of expectedAppFiles) {
    assert(/^[a-f0-9]{64}$/i.test(appDigests[relative] ?? ""),
      `Memories embedding contract has an invalid app digest for ${relative}`);
  }
  assert(JSON.stringify(contract.overlays) === JSON.stringify([...embeddingOverlays]),
    "Memories embedding contract app overlay mapping is incomplete or unexpected");
  for (const [sourcePath, appPath] of Object.entries(embeddingMappings)) {
    assert(sourceDigests[sourcePath] === appDigests[appPath],
      `Memories source and app embedding digests differ for ${sourcePath}`);
  }
  if (appRoot) {
    for (const relative of expectedAppFiles) {
      assert(safeEmbeddingDigest(appRoot, relative) === appDigests[relative],
        `packaged agent-app embedding asset does not match the source contract: ${relative}`);
    }
    assertRagQueryWorkerContract(fs.readFileSync(
      path.join(appRoot, "workers/workflows/rag/lookback-recall.yaml"), "utf8",
    ));
  }
  if (distributedRoot) {
    for (const relative of expectedAppFiles.filter((file) => file.startsWith("workers/"))) {
      assert(safeEmbeddingDigest(distributedRoot, relative) === appDigests[relative],
        `distributed agent-app embedding asset does not match the source contract: ${relative}`);
    }
  }
}

const provenancePath = path.join(bundleRoot, "memories-source-provenance.json");
const hasProvenance = fs.existsSync(provenancePath);
assert(!requireProvenance || hasProvenance, "migration bundle is missing required Memories source provenance");
if (hasProvenance) {
  const provenance = readJson("memories-source-provenance.json");
  assert(provenance.format_version === 3, "unsupported Memories source provenance format");
  assert(
    provenance.bundle_content_digest === manifest.content_digest,
    "Memories source provenance was recorded for a different bundle-manifest.json",
  );
  assert(
    typeof provenance.source_repository === "string" &&
      /^(https:\/\/|ssh:\/\/)[^/\s]+\/.+/.test(provenance.source_repository) &&
      !/\/\/[^/\s]*@/.test(provenance.source_repository) &&
      !/[?#]/.test(provenance.source_repository),
    "Memories source provenance repository must be a credential-free remote URL",
  );
  assert(
    typeof provenance.source_ref === "string" && provenance.source_ref.trim().length > 0,
    "Memories source provenance ref is missing",
  );
  assert(
    /^[a-f0-9]{40}$/i.test(provenance.source_revision),
    "Memories source provenance revision must be a full Git SHA-1",
  );
  verifyEmbeddedContract(provenance, appAssetsRoot, distributedAssetsRoot);
  const expectedDigests = {
    task_catalog: digestOfBundleFile("atlas/post-migration-tasks.json"),
    task_history: digestOfBundleFile("atlas/post-migration-task-history.json"),
  };
  for (const backend of ["sqlite", "postgres"]) {
    const migrationDir = path.join(bundleRoot, "atlas", backend, "migrations");
    const latestName = fs.readdirSync(migrationDir)
      .filter((name) => /^\d{14}_.*\.sql$/.test(name))
      .sort()
      .at(-1);
    expectedDigests[`${backend}_migration`] = sha256(
      fs.readFileSync(path.join(migrationDir, latestName)),
    );
  }
  const sourceArtifacts = provenance.source_artifact_sha256;
  const distributedArtifacts = provenance.distributed_artifact_sha256;
  assert(sourceArtifacts && distributedArtifacts && provenance.source_sha256,
    "Memories source provenance digest maps are missing");
  assert(Array.isArray(provenance.signed_artifacts),
    "Memories source provenance signed artifact list is missing");
  const signedArtifacts = new Set(provenance.signed_artifacts);
  assert(signedArtifacts.size === provenance.signed_artifacts.length &&
    [...signedArtifacts].every((artifact) => ["front", "memories_import", "memories_db_migrate"].includes(artifact)),
  "Memories source provenance signed artifact list is invalid");
  for (const artifact of ["front", "memories_import", "memories_db_migrate"]) {
    assert(sourceArtifacts[artifact] === distributedArtifacts[artifact] || signedArtifacts.has(artifact),
      `unsigned Memories ${artifact} artifact differs from its verified source digest`);
  }
  for (const [key, value] of Object.entries(expectedDigests)) {
    assert(provenance.source_sha256[key] === value, `Memories source provenance digest differs for ${key}`);
  }
  const artifactPaths = {
    front: frontArtifact,
    memories_import: importArtifact,
    memories_db_migrate: cliPath,
  };
  if (requireSourceArtifacts) {
    assert(frontArtifact && importArtifact,
      "pre-sign verification requires staged front and memories-import artifacts");
    assert(provenance.signed_artifacts.length === 0,
      "pre-sign verification requires an unsigned source manifest");
  }
  for (const artifact of ["front", "memories_import", "memories_db_migrate"]) {
    assert(
      /^[a-f0-9]{64}$/i.test(sourceArtifacts[artifact] ?? ""),
      `Memories source provenance has an invalid source ${artifact} digest`,
    );
    assert(
      /^[a-f0-9]{64}$/i.test(distributedArtifacts[artifact] ?? ""),
      `Memories source provenance has an invalid distributed ${artifact} digest`,
    );
  }
  for (const artifact of ["front", "memories_import", "memories_db_migrate"]) {
    const actualPath = artifactPaths[artifact];
    if (!actualPath) continue;
    assert(
      fs.existsSync(actualPath) && fs.statSync(actualPath).isFile(),
      `Memories ${artifact} artifact is missing: ${actualPath}`,
    );
    assert(
      sha256(fs.readFileSync(actualPath)) === distributedArtifacts[artifact],
      `distributed Memories ${artifact} artifact does not match the provenance digest`,
    );
    if (requireSourceArtifacts) {
      assert(
        sourceArtifacts[artifact] === distributedArtifacts[artifact],
        `pre-sign Memories ${artifact} source and distributed digests differ`,
      );
      assert(
        sha256(fs.readFileSync(actualPath)) === sourceArtifacts[artifact],
        `pre-sign Memories ${artifact} artifact does not match its verified source digest`,
      );
    }
  }
} else {
  assert(!frontArtifact && !importArtifact && !requireSourceArtifacts,
    "artifact checks require a provenance manifest");
}

const atlasLock = readJson(path.join("atlas", "atlas-tool.lock.json"));
const platform =
  process.platform === "linux" && process.arch === "x64"
    ? "linux-amd64"
    : process.platform === "darwin" && process.arch === "arm64"
      ? "darwin-arm64"
      : null;
assert(platform, `unsupported migration bundle verification platform: ${process.platform}/${process.arch}`);
const atlasPlatformName = atlasPlatformOverride ?? platform;
const atlasPlatform = atlasLock.platforms?.[atlasPlatformName];
assert(atlasPlatform?.sha256, `Atlas tool lock lacks ${atlasPlatformName}`);
const atlasDigest = crypto.createHash("sha256").update(fs.readFileSync(atlasPath)).digest("hex");
assert(
  atlasDigest.toLowerCase() === atlasPlatform.sha256.toLowerCase(),
  `Atlas binary SHA-256 differs from the ${atlasPlatformName} lock`,
);
const atlasVersion = run(atlasPath, ["version"], "bundled Atlas version");
assert(
  atlasVersion.includes(atlasLock.version),
  `bundled Atlas version does not match lock ${atlasLock.version}`,
);
for (const backend of ["sqlite", "postgres"]) {
  requireFile(path.join("atlas", backend, "migrations", "atlas.sum"));
  const migrationsDir = path.join(bundleRoot, "atlas", backend, "migrations");
  run(
    atlasPath,
    ["migrate", "validate", "--dir", pathToFileURL(migrationsDir).href],
    `${backend} Atlas migration checksum validation`,
  );
}

const cliHelp = run(cliPath, ["--help"], "bundled Memories CLI smoke test", 10_000);
assert(
  cliHelp.includes("memories-db-migrate") && cliHelp.includes("Usage:"),
  "bundled Memories CLI returned unexpected --help output",
);

// These help requests exercise each embedding command's clap registration,
// and terminate before any environment or user database is opened.
const embeddingHelp = run(cliPath, ["embedding", "--help"], "bundled Memories embedding CLI smoke test", 10_000);
for (const command of ["inspect", "plan", "switch", "abandon", "finalize", "restore"]) {
  assert(embeddingHelp.includes(command),
    `bundled Memories embedding CLI is missing the ${command} subcommand`);
  const commandHelp = run(
    cliPath,
    ["embedding", command, "--help"],
    `bundled Memories embedding ${command} help smoke test`,
    10_000,
  );
  assert(commandHelp.includes("Usage:") && commandHelp.includes(command),
    `bundled Memories embedding ${command} subcommand returned unexpected help output`);
}

// A provenance manifest is added after the build and macOS signing rewrites
// the executables, so `bundle verify`'s content digest only holds for a bundle
// exactly as Memories built it. Release bundles are instead tied to that
// build-time digest through the provenance recorded above.
if (!hasProvenance) {
  const output = run(cliPath, ["bundle", "verify"], "bundled Memories CLI bundle verify", 120_000);
  const verified = output.match(/^bundle_verify status=verified digest=([a-f0-9]{64})$/m);
  assert(verified, `bundled Memories CLI did not verify its bundle: ${output.trim()}`);
  assert(
    verified[1] === manifest.content_digest,
    "bundle verify reported a digest different from bundle-manifest.json",
  );
}

console.log(`migration bundle verified: ${manifest.content_digest}`);
