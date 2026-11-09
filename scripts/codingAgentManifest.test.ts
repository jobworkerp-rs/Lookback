import { execFileSync, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { credentialFreeOrigin } from "./lib/git-origin.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const manifestScript = path.join(repoRoot, "scripts/lib/coding-agent-manifest.mjs");
const verifier = path.join(repoRoot, "scripts/verify-coding-agent-artifacts.mjs");
const stageLibrary = path.join(repoRoot, "scripts/lib/stage-coding-agent-package.sh");
const buildCommon = path.join(repoRoot, "scripts/lib/build-common.sh");
const target = "x86_64-unknown-linux-gnu";
const gitEnv = {
  PATH: process.env.PATH ?? "/usr/bin:/bin",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_TERMINAL_PROMPT: "0",
};

let scratchRoot: string;

beforeEach(() => {
  fs.mkdirSync("/tmp/opencode", { recursive: true });
  scratchRoot = fs.mkdtempSync("/tmp/opencode/coding-agent-manifest-");
});

afterEach(() => {
  fs.rmSync(scratchRoot, { recursive: true, force: true });
});

function git(repo: string, ...args: string[]): string {
  return execFileSync("git", ["-C", repo, ...args], { encoding: "utf8", env: gitEnv }).trim();
}

function initRepo(name: string, branch: string, remote: string) {
  const repo = path.join(scratchRoot, name);
  fs.mkdirSync(repo);
  execFileSync("git", ["init", "--quiet", "--initial-branch", branch, repo], { env: gitEnv });
  git(repo, "remote", "add", "origin", remote);
  git(repo, "config", "user.name", "manifest fixture");
  git(repo, "config", "user.email", "manifest-fixture@example.invalid");
  return repo;
}

function commit(repo: string, relative: string, contents: string) {
  const file = path.join(repo, relative);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, contents);
  git(repo, "add", "--", relative);
  git(repo, "commit", "--quiet", "-m", `add ${relative}`);
  return git(repo, "rev-parse", "HEAD");
}

function sha256(file: string): string {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

/**
 * A development checkout as found on a contributor machine: scp-style remote,
 * branch `main`, an uncommitted file, and the runtime checkout clean on `main`.
 */
function developmentFixture() {
  const clientRevision = "c".repeat(40);
  const app = initRepo("app", "main", "git@gitea.example:team/agent-app.git");
  commit(
    app,
    "Cargo.lock",
    `version = 4\n\n[[package]]\nname = "jobworkerp-client"\nversion = "0.1.0"\nsource = "git+https://example.invalid/jobworkerp/client.git?branch=main#${clientRevision}"\n`,
  );
  commit(app, "coding-agent-executor/Cargo.toml", '[package]\nname = "coding-agent-executor"\nversion = "0.1.0"\n');
  commit(app, "coding-agent-executor/src/model.rs", "pub const PROTOCOL_VERSION: u32 = 2;\n");
  fs.writeFileSync(path.join(app, "work-in-progress.txt"), "uncommitted\n");

  const runtime = initRepo("runtime", "main", "https://gitea.example/team/jobworkerp-rs.git");
  commit(runtime, "Cargo.toml", '[workspace]\nresolver = "3"\n');

  const artifactRoot = path.join(scratchRoot, "package");
  fs.mkdirSync(path.join(artifactRoot, "runtime"), { recursive: true });
  fs.mkdirSync(path.join(artifactRoot, "bin"), { recursive: true });
  fs.writeFileSync(path.join(artifactRoot, "runtime/all-in-one"), "runtime bytes");
  const capabilities = JSON.stringify([
    "resolve_base",
    "check_repository",
    "inspect_source",
    "prepare",
    "capture",
    "seal",
    "resolve",
    "artifact_only_publication",
    "push",
    "push_workspace",
    "lookup_publication",
    "pull_request",
    "secret_scan_supported_formats_v1",
  ]);
  fs.writeFileSync(
    path.join(artifactRoot, "bin/coding-agent-executor"),
    `#!/bin/sh\ncase "$1" in\n  --version) printf '%s\\n' 'coding-agent-executor 0.1.0 protocol=2' ;;\n  --capabilities) printf '%s\\n' '{"protocol_version":2,"executor_version":"0.1.0","features":${capabilities}}' ;;\n  *) exit 97 ;;\nesac\n`,
    { mode: 0o755 },
  );
  return { app, runtime, artifactRoot, clientRevision };
}

function runManifest(args: string[]) {
  return spawnSync("node", [manifestScript, ...args], { encoding: "utf8", env: gitEnv });
}

function runVerifier(args: string[]) {
  return spawnSync("node", [verifier, ...args], { cwd: scratchRoot, encoding: "utf8", env: gitEnv });
}

describe("credentialFreeOrigin", () => {
  it("rewrites scp-style remotes as ssh URLs and strips credentials and selectors", () => {
    expect(credentialFreeOrigin("git@gitea.example:team/agent-app.git")).toBe(
      "ssh://gitea.example/team/agent-app.git",
    );
    expect(credentialFreeOrigin("https://user:token@example.invalid/team/app.git?branch=main#x")).toBe(
      "https://example.invalid/team/app.git",
    );
    expect(credentialFreeOrigin("ssh://git@example.invalid/team/app.git/")).toBe(
      "ssh://example.invalid/team/app.git",
    );
  });

  it("rejects local paths with the caller's label", () => {
    expect(() => credentialFreeOrigin("/srv/git/app.git", "Memories Git origin")).toThrow(
      "Memories Git origin must be a remote HTTPS or SSH URL",
    );
  });
});

describe("coding-agent development manifest", () => {
  it("records working-tree provenance truthfully and normalises the scp remote", () => {
    const fixture = developmentFixture();
    const result = runManifest([
      "--app-root",
      fixture.app,
      "--runtime-root",
      fixture.runtime,
      "--artifact-root",
      fixture.artifactRoot,
      "--target",
      target,
      "--allow-development-sources",
    ]);
    expect(result.status, `${result.stdout}${result.stderr}`).toBe(0);

    const manifestPath = path.join(fixture.artifactRoot, "coding-agent-artifact-manifest.json");
    const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
    expect(manifest.sources.agent_app).toEqual({
      repository: "ssh://gitea.example/team/agent-app.git",
      ref: "main",
      revision: git(fixture.app, "rev-parse", "HEAD"),
      dirty: true,
      submodules: [],
    });
    expect(manifest.sources.jobworkerp_runtime).toEqual({
      repository: "https://gitea.example/team/jobworkerp-rs.git",
      ref: "main",
      revision: git(fixture.runtime, "rev-parse", "HEAD"),
      dirty: false,
      submodules: [],
    });
    expect(manifest.sources.jobworkerp_client).toEqual({
      repository: "https://example.invalid/jobworkerp/client.git",
      revision: fixture.clientRevision,
      lockfile_sha256: sha256(path.join(fixture.app, "Cargo.lock")),
    });
    expect(manifest.executor).toEqual({
      version: "0.1.0",
      protocol_version: 2,
      capabilities: expect.arrayContaining(["prepare", "push_workspace"]),
    });
    expect(manifest.artifacts.runtime.sha256).toBe(sha256(path.join(fixture.artifactRoot, "runtime/all-in-one")));
    expect(manifest.artifacts.executor.sha256).toBe(
      sha256(path.join(fixture.artifactRoot, "bin/coding-agent-executor")),
    );
    expect(fs.statSync(manifestPath).mode & 0o777).toBe(0o644);

    // Re-running replaces the manifest in place instead of failing on an existing file.
    const rerun = runManifest([
      "--app-root",
      fixture.app,
      "--runtime-root",
      fixture.runtime,
      "--artifact-root",
      fixture.artifactRoot,
      "--target",
      target,
      "--allow-development-sources",
    ]);
    expect(rerun.status, `${rerun.stdout}${rerun.stderr}`).toBe(0);
    expect(fs.readdirSync(fixture.artifactRoot).filter((name) => name.endsWith(".tmp"))).toEqual([]);
  });

  it("records a detached HEAD as the literal HEAD ref", () => {
    const fixture = developmentFixture();
    git(fixture.runtime, "checkout", "--quiet", "--detach");
    const result = runManifest([
      "--app-root",
      fixture.app,
      "--runtime-root",
      fixture.runtime,
      "--artifact-root",
      fixture.artifactRoot,
      "--target",
      target,
      "--allow-development-sources",
    ]);
    expect(result.status, `${result.stdout}${result.stderr}`).toBe(0);

    const manifestPath = path.join(fixture.artifactRoot, "coding-agent-artifact-manifest.json");
    const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
    expect(manifest.sources.jobworkerp_runtime.ref).toBe("HEAD");
  });

  it("refuses dirty or floating sources without the development flag and writes nothing", () => {
    const fixture = developmentFixture();
    const result = runManifest([
      "--app-root",
      fixture.app,
      "--runtime-root",
      fixture.runtime,
      "--artifact-root",
      fixture.artifactRoot,
      "--target",
      target,
      "--app-url",
      "ssh://gitea.example/team/agent-app.git",
      "--app-ref",
      "main",
      "--app-revision",
      git(fixture.app, "rev-parse", "HEAD"),
      "--runtime-url",
      "https://gitea.example/team/jobworkerp-rs.git",
      "--runtime-ref",
      "main",
      "--runtime-revision",
      git(fixture.runtime, "rev-parse", "HEAD"),
      "--client-revision",
      fixture.clientRevision,
    ]);
    expect(result.status).toBe(1);
    expect(result.stderr).toContain("could not record coding-agent artifact provenance");
    expect(fs.existsSync(path.join(fixture.artifactRoot, "coding-agent-artifact-manifest.json"))).toBe(false);
  });

  it("is accepted by the verifier only when development sources are explicitly allowed", () => {
    const fixture = developmentFixture();
    const written = runManifest([
      "--app-root",
      fixture.app,
      "--runtime-root",
      fixture.runtime,
      "--artifact-root",
      fixture.artifactRoot,
      "--target",
      target,
      "--allow-development-sources",
    ]);
    expect(written.status, `${written.stdout}${written.stderr}`).toBe(0);
    const common = [
      "--manifest",
      path.join(fixture.artifactRoot, "coding-agent-artifact-manifest.json"),
      "--artifact-root",
      fixture.artifactRoot,
      "--config-root",
      fixture.app,
      "--provenance-only",
      "--json",
    ];

    const strict = runVerifier(common);
    expect(strict.status).not.toBe(0);
    expect(JSON.parse(strict.stdout).diagnostics).toContain("manifest_invalid");

    const relaxed = runVerifier([...common, "--allow-development-sources"]);
    expect(relaxed.status, `${relaxed.stdout}${relaxed.stderr}`).toBe(0);
    const report = JSON.parse(relaxed.stdout);
    expect(report.provenance_valid).toBe(true);
    expect(report.enable_ready).toBe(false);
    expect(report.diagnostics).toContain("development_sources_accepted");

    // Argument parsing fails before `--json` takes effect, so the report is plain text.
    const unknown = runVerifier([...common, "--allow-dirty"]);
    expect(unknown.status).not.toBe(0);
    expect(unknown.stdout).toContain("invalid_arguments");
  });
});

/**
 * Source the staging library the way stage-dev-external-bins.sh does and call
 * `stage_coding_agent_package` against a fixture checkout with a fake cargo.
 */
function runStage(fixture: ReturnType<typeof developmentFixture>, options: { dryRun?: boolean; triple?: string } = {}) {
  const fakeBin = path.join(scratchRoot, "fake-bin");
  fs.mkdirSync(fakeBin, { recursive: true });
  const cargo = path.join(fakeBin, "cargo");
  if (!fs.existsSync(cargo)) {
    fs.writeFileSync(
      cargo,
      `#!/bin/sh
set -eu
out="$CARGO_TARGET_DIR/debug/coding-agent-executor"
mkdir -p "$(dirname "$out")"
cp "\${FIXTURE_EXECUTOR}" "$out"
chmod 755 "$out"
`,
      { mode: 0o755 },
    );
  }
  const targetDir = path.join(scratchRoot, "cargo-target");
  const runtimeBinary = path.join(scratchRoot, "sidecar-all-in-one");
  if (!fs.existsSync(runtimeBinary)) fs.writeFileSync(runtimeBinary, "sidecar runtime bytes", { mode: 0o755 });
  const script = `
    set -euo pipefail
    source '${buildCommon}'
    source '${stageLibrary}'
    AGENT_APP='${fixture.app}'
    TRIPLE='${options.triple ?? target}'
    JOBWORKERP_BIN='${runtimeBinary}'
    DRY_RUN=${options.dryRun ? 1 : 0}
    LOOKBACK_CODING_AGENT_RUNTIME_SOURCE_DIR='${fixture.runtime}'
    stage_coding_agent_package
  `;
  const result = spawnSync("bash", ["-c", script], {
    cwd: fixture.app,
    encoding: "utf8",
    env: {
      ...gitEnv,
      PATH: `${fakeBin}:${gitEnv.PATH}`,
      CARGO_TARGET_DIR: targetDir,
      FIXTURE_EXECUTOR: path.join(fixture.artifactRoot, "bin/coding-agent-executor"),
    },
  });
  return { result, packageDir: path.join(targetDir, "debug/coding-agent"), runtimeBinary };
}

describe("development package staging", () => {
  it("builds the executor and stages a verified package into the cargo target directory", () => {
    const fixture = developmentFixture();
    // The fixture app must look like the real checkout: the verifier reads
    // scripts relative to AGENT_APP.
    fs.mkdirSync(path.join(fixture.app, "scripts/lib"), { recursive: true });
    for (const relative of [
      "scripts/verify-coding-agent-artifacts.mjs",
      "scripts/lib/coding-agent-manifest.mjs",
      "scripts/lib/git-origin.mjs",
    ]) {
      fs.copyFileSync(path.join(repoRoot, relative), path.join(fixture.app, relative));
    }

    const first = runStage(fixture);
    expect(first.result.status, `${first.result.stdout}${first.result.stderr}`).toBe(0);
    const manifestPath = path.join(first.packageDir, "coding-agent-artifact-manifest.json");
    const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
    expect(manifest.sources.agent_app.dirty).toBe(true);
    expect(manifest.sources.agent_app.ref).toBe("main");
    expect(manifest.artifacts.runtime.sha256).toBe(sha256(first.runtimeBinary));
    expect(fs.readFileSync(path.join(first.packageDir, "runtime/all-in-one"), "utf8")).toBe("sidecar runtime bytes");
    for (const relative of ["", "bin", "runtime"]) {
      expect(fs.statSync(path.join(first.packageDir, relative)).mode & 0o7022).toBe(0);
    }
    expect(fs.statSync(path.join(first.packageDir, "bin/coding-agent-executor")).mode & 0o111).not.toBe(0);
    expect(fs.readdirSync(first.packageDir).filter((name) => name.startsWith("."))).toEqual([]);
    expect(first.result.stderr).toContain("staged coding-agent package:");

    const before = fs.statSync(manifestPath).ino;
    const second = runStage(fixture);
    expect(second.result.status, `${second.result.stdout}${second.result.stderr}`).toBe(0);
    expect(second.result.stderr).toContain("staged coding-agent package is current");
    expect(fs.statSync(manifestPath).ino).toBe(before);
  });

  it("only describes the work in dry-run mode and skips unsupported triples", () => {
    const fixture = developmentFixture();
    const dry = runStage(fixture, { dryRun: true });
    expect(dry.result.status, `${dry.result.stdout}${dry.result.stderr}`).toBe(0);
    expect(dry.result.stdout).toContain("would build coding-agent-executor and stage coding-agent package");
    expect(fs.existsSync(dry.packageDir)).toBe(false);

    const mac = runStage(fixture, { triple: "aarch64-apple-darwin" });
    expect(mac.result.status, `${mac.result.stdout}${mac.result.stderr}`).toBe(0);
    expect(mac.result.stderr).toContain("skipping for aarch64-apple-darwin");
    expect(fs.existsSync(mac.packageDir)).toBe(false);
  });
});
