#!/usr/bin/env node
// Generate a self-contained Memories migration bundle for shell tests.
// The executable stubs implement the small public surface exercised by
// verify-migration-release.mjs (`--help`, `bundle verify`); no live database
// or upstream checkout is used.
//
// Modes:
//   valid               release-shaped bundle with format 3 provenance
//   dev                 bundle exactly as Memories builds it (no provenance)
//   broken-cli          dev bundle whose `bundle verify` rejects the bundle
//   stale-digest        dev bundle whose `bundle verify` reports another digest
//   provenance-mismatch release bundle whose provenance names another bundle
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import {
  EMBEDDING_APP_OVERLAYS,
  EMBEDDING_SOURCE_CLI_FILES,
  EMBEDDING_SOURCE_TO_APP,
} from "../verify-embedding-contract.mjs";

const [root, mode = "valid", contentSeed = "fixture"] = process.argv.slice(2);
if (!root) throw new Error("usage: migration-test-fixture.mjs OUTPUT_DIRECTORY [MODE] [CONTENT_SEED]");
const withProvenance = mode === "valid" || mode === "provenance-mismatch";
const appAssetsRoot = `${path.resolve(root)}.app-assets`;

const sha256 = (bytes) => crypto.createHash("sha256").update(bytes).digest("hex");
fs.rmSync(root, { recursive: true, force: true });
const write = (relative, content, executable = false) => {
  const target = path.join(root, relative);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, content);
  if (executable) fs.chmodSync(target, 0o755);
};

const schema = "20260930000001";
// `CONTENT_SEED` lets tests produce bundles with different content digests.
write("atlas/post-migration-tasks.json", JSON.stringify({ tasks: [], seed: contentSeed }, null, 2));
write("atlas/post-migration-task-history.json", JSON.stringify({ entries: [] }, null, 2));
for (const backend of ["sqlite", "postgres"]) {
  write(`atlas/${backend}/migrations/${schema}_fixture.sql`, `-- fixture ${backend} migration\n`);
  write(`atlas/${backend}/migrations/atlas.sum`, "fixture-atlas-sum\n");
  write(`atlas/${backend}/seed-expectations.json`, JSON.stringify({ version: schema, tables: [] }));
}
write("atlas/migrate.hcl", 'env "sqlite" { url = getenv("MEMORIES_ATLAS_DATABASE_URL") }\n');
write("atlas/verify.hcl", 'env "sqlite" { url = getenv("MEMORIES_ATLAS_DATABASE_URL") }\n');
write("atlas/licenses/ATLAS_LICENSE", "fixture license\n");
const atlasStub = `#!/usr/bin/env node
if (process.argv[2] === "version") { console.log("atlas version v1.3.0"); process.exit(0); }
if (process.argv[2] === "migrate" && process.argv[3] === "validate") process.exit(0);
console.error("unexpected Atlas fixture invocation"); process.exit(2);
`;
write("atlas/bin/atlas", atlasStub, true);
const atlasHash = sha256(atlasStub);
write("atlas/atlas-tool.lock.json", JSON.stringify({
  version: "v1.3.0",
  platforms: {
    "linux-amd64": { url: "fixture://atlas/linux-amd64", sha256: atlasHash },
    "darwin-arm64": { url: "fixture://atlas/darwin-arm64", sha256: atlasHash },
  },
}, null, 2));

const verifyBehaviour = {
  "broken-cli": 'console.log("bundle_verify status=failed error_code=bundle_invalid"); process.exit(1);',
  "stale-digest": `console.log("bundle_verify status=verified digest=${"0".repeat(64)}"); process.exit(0);`,
}[mode] ?? `
  const fs = require("node:fs");
  const path = require("node:path");
  const manifest = JSON.parse(fs.readFileSync(path.join(__dirname, "bundle-manifest.json"), "utf8"));
  console.log("bundle_verify status=verified digest=" + manifest.content_digest);
  process.exit(0);`;
const embeddingCommands = ["inspect", "plan", "switch", "abandon", "finalize", "restore"];
const help = "Usage: memories-db-migrate <COMMAND>\\nCommands: schema post-migrate local bundle embedding\\n";
const cli = `#!/usr/bin/env node
if (process.argv.slice(2).join(" ") === "--help") { process.stdout.write(${JSON.stringify(help)}); process.exit(0); }
if (process.argv[2] === "embedding" && process.argv[3] === "--help") {
  process.stdout.write("Usage: memories-db-migrate embedding <COMMAND>\\nCommands: ${embeddingCommands.join(" ")}\\n");
  process.exit(0);
}
if (process.argv[2] === "embedding" && process.argv[4] === "--help" && ${JSON.stringify(embeddingCommands)}.includes(process.argv[3])) {
  process.stdout.write("Usage: memories-db-migrate embedding " + process.argv[3] + "\\n");
  process.exit(0);
}
if (process.argv.slice(2).join(" ") === "bundle verify") {
  ${verifyBehaviour}
}
console.error("unexpected Memories CLI fixture invocation"); process.exit(2);
`;
write("memories-db-migrate", cli, true);

// Any stable digest over the shipped files models Memories' content digest.
const listFiles = (dir) =>
  fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    return entry.isDirectory() ? listFiles(full) : [full];
  });
const contentDigest = sha256(
  listFiles(root)
    .map((file) => `${path.relative(root, file)}\0${sha256(fs.readFileSync(file))}`)
    .sort()
    .join("\n"),
);
write("bundle-manifest.json", JSON.stringify({
  format: "memories-db-migrate-bundle-v1",
  content_digest: contentDigest,
  source_revision: "f".repeat(40),
  source_dirty: false,
}, null, 2));
// Stand-ins for the separately staged front / memories-import binaries.
write("test-artifacts/front", "fixture Memories front\n", true);
write("test-artifacts/memories-import", "fixture memories-import\n", true);

let embeddingContract;
if (withProvenance) {
  // Keep release-verifier fixtures structurally valid without pretending
  // these synthetic bytes are a real Memories checkout or app build.
  fs.rmSync(appAssetsRoot, { recursive: true, force: true });
  const sourceSha256 = {};
  const appSha256 = {};
  for (const [sourcePath, appPath] of Object.entries(EMBEDDING_SOURCE_TO_APP)) {
    const bytes = Buffer.from(`fixture embedding asset: ${appPath}\n`);
    const target = path.join(appAssetsRoot, appPath);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, bytes);
    const digest = sha256(bytes);
    sourceSha256[sourcePath] = digest;
    appSha256[appPath] = digest;
  }
  for (const sourcePath of EMBEDDING_SOURCE_CLI_FILES) {
    sourceSha256[sourcePath] = sha256(Buffer.from(`fixture Memories CLI source: ${sourcePath}\n`));
  }
  for (const relative of EMBEDDING_APP_OVERLAYS) {
    const bytes = Buffer.from(relative === "workers/workflows/rag/lookback-recall.yaml"
      ? `do:
  - embedQuery:
      run:
        worker:
          name: "%{LOOKBACK_MM_EMBEDDING_WORKER}"
          using: embed_text
          arguments:
            text: "\${ $workflow.input.query }"
            prefix: "%{MEMORY_EMBEDDING_QUERY_PREFIX:-}"
`
      : `fixture app-owned embedding overlay: ${relative}\n`);
    const target = path.join(appAssetsRoot, relative);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, bytes);
    appSha256[relative] = sha256(bytes);
  }
  embeddingContract = {
    format_version: 1,
    source_revision: "f".repeat(40),
    source_sha256: sourceSha256,
    app_sha256: appSha256,
    mappings: { ...EMBEDDING_SOURCE_TO_APP },
    overlays: [...EMBEDDING_APP_OVERLAYS],
  };
  fs.writeFileSync(
    path.join(appAssetsRoot, "embedding-contract.json"),
    `${JSON.stringify(embeddingContract, null, 2)}\n`,
  );
}

if (withProvenance) {
  const sha256File = (relative) => sha256(fs.readFileSync(path.join(root, relative)));
  const sourceArtifacts = {
    front: sha256File("test-artifacts/front"),
    memories_import: sha256File("test-artifacts/memories-import"),
    memories_db_migrate: sha256File("memories-db-migrate"),
  };
  write("memories-source-provenance.json", `${JSON.stringify({
    format_version: 3,
    source_repository: "ssh://gitea.example.invalid/jobworkerp-rs/LLM-Memories.git",
    source_ref: "migration-fixture",
    source_revision: "f".repeat(40),
    bundle_content_digest: mode === "provenance-mismatch" ? "1".repeat(64) : contentDigest,
    source_artifact_sha256: sourceArtifacts,
    distributed_artifact_sha256: { ...sourceArtifacts },
    signed_artifacts: [],
    source_sha256: {
      task_catalog: sha256File("atlas/post-migration-tasks.json"),
      task_history: sha256File("atlas/post-migration-task-history.json"),
      sqlite_migration: sha256File(`atlas/sqlite/migrations/${schema}_fixture.sql`),
      postgres_migration: sha256File(`atlas/postgres/migrations/${schema}_fixture.sql`),
    },
    embedding_contract: embeddingContract,
  }, null, 2)}\n`);
}
